//! Windows 限速引擎：全局句柄接管网络包 → 五元组识别进程 → 用户态令牌桶分派 → 放行/丢弃。
//!
//! ## 架构（WinDivert 2.2 兼容）
//!
//! WinDivert 2.2 的 `WINDIVERT_LAYER_NETWORK` **不支持 `processId` 过滤字段**（官方文档：
//! "this field is not supported by the WINDIVERT_LAYER_NETWORK* layers"）。实测
//! `WinDivertHelperCompileFilter` 对含 `processId` 的表达式返回
//! "Filter expression contains a bad token for layer"，导致 `WinDivertOpen` 必然失败。
//!
//! 因此本引擎不再给每个程序开句柄，而是：
//!
//! 1. **每方向只开 1 个全局句柄**，filter 用 `Direction::base_filter()`
//!    （`inbound/outbound and ip and (tcp or udp) and not loopback`，不含 processId）；
//! 2. 收到包后解析 IPv4 + TCP/UDP 头得到五元组；
//! 3. 用 [`proc_map::ProcMap`]（GetExtendedTcpTable/UdpTable owner-pid）查到 owning PID；
//! 4. 按 [`LimitConfig`] 把 PID 映射到程序 key，选出该程序的令牌桶；未匹配则走
//!    「其他流量」令牌桶；都没有速率限制则直接放行；
//! 5. 令牌足够 → `WinDivertSend` 重注入并计数；不足 → 丢弃（TCP 触发拥塞控制自然降速）。
//!
//! 副作用是两个句柄的 filter 天然互斥（inbound vs outbound），**彻底消除了原实现
//! 「多句柄 filter 重叠会重复注入」与 `MAX_EXCLUDES` 限制**。

pub mod proc_map;

use std::collections::HashMap;
use std::ffi::{c_char, CStr, CString};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use libloading::{Library, Symbol};
use rl_core::{ByteCounter, Direction, Engine, LimitConfig, TokenBucket};

/// WinDivert 文档定义的最大包长 = 40 + 0xFFFF
const WINDIVERT_MTU_MAX: usize = 40 + 0xFFFF;
/// recv 队列超时（毫秒）。设短一点，保证 poll 能及时返回去响应新配置
const QUEUE_TIME_MS: u64 = 4;
/// WINDIVERT_LAYER_NETWORK
const LAYER_NETWORK: u32 = 0;
/// WINDIVERT_PARAM_QUEUE_TIME
const PARAM_QUEUE_TIME: u32 = 1;
/// 「其他流量」在统计表 / 桶表里的键（真实程序 key 不会是它）
pub const OTHER: &str = "__other__";

/// 统计维度：(程序 key 或 OTHER, 方向)
pub type StatKey = (String, Direction);
pub type StatsMap = Arc<Mutex<HashMap<StatKey, Arc<ByteCounter>>>>;

/// WINDIVERT_ADDRESS：只做透传（recv 填、send 传回），不解释内部位域，
/// 所以用定长缓冲表示，避免位域布局对不齐导致的问题。
///
/// ⚠️ 大小必须是 **80 字节**（windivert.h）：
/// `INT64 Timestamp`(8) + `UINT32 位域块`(4) + `UINT32 Reserved2`(4)
/// + 匿名 union（`UINT8 Reserved3[64]` 主导，64）= 80。
/// 原实现写成 64 字节，`WinDivertRecv` 写入时会溢出、破坏相邻内存。
/// 之前句柄从未打开成功（filter 含 processId 非法），recv 没被调用才没暴露。
#[repr(C, align(8))]
#[derive(Clone, Copy)]
struct Address([u8; 80]);

/// 句柄用整数表示而不是裸指针，这样天然满足 Send
type Handle = isize;
const INVALID_HANDLE: Handle = -1;

type FnOpen = unsafe extern "system" fn(*const c_char, u32, i16, u64) -> Handle;
type FnRecv = unsafe extern "system" fn(Handle, *mut u8, u32, *mut u32, *mut Address) -> i32;
type FnSend = unsafe extern "system" fn(Handle, *const u8, u32, *mut u32, *const Address) -> i32;
type FnClose = unsafe extern "system" fn(Handle) -> i32;
type FnSetParam = unsafe extern "system" fn(Handle, u32, u64) -> i32;
/// BOOL WinDivertHelperCompileFilter(LPCSTR, UINT32 layer, char* object, UINT objLen,
///                                  const char** errorStr, UINT* errorPos)
type FnCompileFilter = unsafe extern "system" fn(
    *const c_char, u32, *mut c_char, u32, *mut *const c_char, *mut u32,
) -> i32;

#[derive(Clone, Copy)]
struct Api {
    open: FnOpen,
    recv: FnRecv,
    send: FnSend,
    close: FnClose,
    set_param: FnSetParam,
    compile_filter: FnCompileFilter,
}

/// 加载并缓存 WinDivert 函数表（只需一次）
fn api() -> Result<&'static Api, String> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(load_api).as_ref().map_err(|e| e.clone())
}

fn load_api() -> Result<Api, String> {
    unsafe {
        let dll = Library::new("WinDivert.dll").map_err(|e| {
            format!(
                "加载 WinDivert.dll 失败：{e}。请把 WinDivert.dll 与 WinDivert64.sys 放到程序同目录（见 assets/README.md）"
            )
        })?;

        let open: Symbol<FnOpen> = symbol(&dll, b"WinDivertOpen\0")?;
        let recv: Symbol<FnRecv> = symbol(&dll, b"WinDivertRecv\0")?;
        let send: Symbol<FnSend> = symbol(&dll, b"WinDivertSend\0")?;
        let close: Symbol<FnClose> = symbol(&dll, b"WinDivertClose\0")?;
        let set_param: Symbol<FnSetParam> = symbol(&dll, b"WinDivertSetParam\0")?;
        let compile_filter: Symbol<FnCompileFilter> = symbol(&dll, b"WinDivertHelperCompileFilter\0")?;

        let api = Api {
            open: *open,
            recv: *recv,
            send: *send,
            close: *close,
            set_param: *set_param,
            compile_filter: *compile_filter,
        };
        // 函数指针脱离 Symbol 的生命周期，必须让 DLL 常驻内存
        std::mem::forget(dll);
        Ok(api)
    }
}

fn symbol<'a, T>(dll: &'a Library, name: &[u8]) -> Result<Symbol<'a, T>, String> {
    unsafe { dll.get(name) }.map_err(|e| format!("找不到 {}: {e}", String::from_utf8_lossy(name)))
}

/// 打开句柄前先离线编译 filter —— 这是防止再次引入 NETWORK 层不支持字段的关键防线。
fn validate_filter(api: &Api, filter: &str) -> Result<(), String> {
    let expr = CString::new(filter).map_err(|_| "过滤表达式含非法字符".to_string())?;
    let mut err_ptr: *const c_char = std::ptr::null();
    let mut err_pos: u32 = 0;
    let ok = unsafe {
        (api.compile_filter)(
            expr.as_ptr(), LAYER_NETWORK, std::ptr::null_mut(), 0,
            &mut err_ptr, &mut err_pos,
        )
    };
    if ok != 0 {
        return Ok(());
    }
    let msg = if err_ptr.is_null() {
        "未知错误".to_string()
    } else {
        unsafe { CStr::from_ptr(err_ptr) }.to_string_lossy().into_owned()
    };
    Err(format!("过滤表达式非法（位置 {err_pos}）：{msg}；表达式：{filter}"))
}

/// 把 WinDivertOpen 的 Win32 错误码翻译成可操作的中文建议。
/// 原实现把所有失败一律归因为「请以管理员身份运行」，是误报。
fn explain_open_error(code: i32) -> &'static str {
    match code {
        2 => "找不到 WinDivert64.sys 驱动文件，请确认它与 rl-app.exe 在同一目录",
        5 => "权限不足：请右键「以管理员身份运行」本程序（或点顶部横幅的提权按钮）",
        87 => "过滤表达式 / 参数非法（引擎内部 filter 校验未通过，属于程序缺陷）",
        577 => "WinDivert64.sys 驱动数字签名无效，请使用官方发布包里的驱动",
        654 => "系统已加载不兼容版本的 WinDivert 驱动，请重启电脑后重试",
        1060 => "WinDivert 驱动服务未安装，请确认 WinDivert64.sys 存在",
        1275 => "驱动被安全软件拦截，或当前是不支持内核驱动的虚拟化环境；请把程序加入白名单",
        1753 => "Windows 的 Base Filtering Engine 服务被禁用，请启用 BFE 服务后重试",
        _ => "驱动加载失败，请确认以管理员身份运行、驱动文件齐全且未被安全软件拦截",
    }
}

/// 一个方向一个全局句柄
struct DirHandle {
    dir: Direction,
    handle: Handle,
    buf: Vec<u8>,
    addr: Address,
}

impl DirHandle {
    fn open(dir: Direction) -> Result<Self, String> {
        let api = api()?;
        let filter = dir.base_filter();
        validate_filter(api, filter)?;

        let expr = CString::new(filter).map_err(|_| "过滤表达式非法".to_string())?;
        let handle = unsafe { (api.open)(expr.as_ptr(), LAYER_NETWORK, 0, 0) };
        if handle == 0 || handle == INVALID_HANDLE {
            let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            return Err(format!(
                "打开{}全局句柄失败（Win32 错误 {code}）：{}",
                dir.label(), explain_open_error(code)
            ));
        }
        unsafe { (api.set_param)(handle, PARAM_QUEUE_TIME, QUEUE_TIME_MS) };

        Ok(Self {
            dir,
            handle,
            buf: vec![0u8; WINDIVERT_MTU_MAX],
            addr: Address([0; 80]),
        })
    }

    /// 收一个包，返回包长；0 表示本轮没包（队列超时）
    fn recv(&mut self) -> Result<usize, String> {
        let api = api()?;
        let mut len: u32 = 0;
        let ok = unsafe {
            (api.recv)(
                self.handle,
                self.buf.as_mut_ptr(),
                self.buf.len() as u32,
                &mut len,
                &mut self.addr,
            )
        };
        if ok == 0 {
            // QUEUE_TIME(4ms) 到期、队列为空时 WinDivertRecv 返回 FALSE，
            // 这是「本轮没包」而不是错误（与原实现一致）。
            return Ok(0);
        }
        Ok(len as usize)
    }

    /// 原样重注入（校验和由驱动处理）
    fn reinject(&mut self, len: usize) -> Result<(), String> {
        let api = api()?;
        let mut sent: u32 = 0;
        let ok = unsafe {
            (api.send)(
                self.handle,
                self.buf.as_ptr(),
                len as u32,
                &mut sent,
                &self.addr,
            )
        };
        if ok == 0 {
            let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            return Err(format!("重注入失败（Win32 错误 {code}）"));
        }
        Ok(())
    }
}

impl Drop for DirHandle {
    fn drop(&mut self) {
        if let Ok(api) = api() {
            unsafe { (api.close)(self.handle) };
        }
    }
}

/// 从 IPv4 包解析五元组。返回 `(src, sport, dst, dport, is_tcp)`。
///
/// - filter 含 `ip` 且 `tcp or udp`，所以只会收到 IPv4 TCP/UDP 包；
/// - 非首片 IP 分片没有端口信息，返回 None → 归入「其他流量」兜底；
/// - 端口按网络字节序读出（主机序 u16），与 proc_map 的 `mib_port` 一致；
/// - 地址按 `from_ne_bytes` 读 4 字节，与 MIB 结构里 DWORD 的原生布局一致。
pub fn parse_tuple(pkt: &[u8]) -> Option<(u32, u16, u32, u16, bool)> {
    if pkt.len() < 20 {
        return None;
    }
    let ver_ihl = pkt[0];
    if ver_ihl >> 4 != 4 {
        return None; // 不是 IPv4
    }
    let ihl = ((ver_ihl & 0x0f) as usize) * 4;
    if ihl < 20 || pkt.len() < ihl + 4 {
        return None;
    }
    let proto = pkt[9];
    let is_tcp = match proto {
        6 => true,
        17 => false,
        _ => return None,
    };
    // 非首片分片（fragment offset != 0）没有传输层头
    let frag = u16::from_be_bytes([pkt[6], pkt[7]]);
    if frag & 0x1fff != 0 {
        return None;
    }
    let src = u32::from_ne_bytes([pkt[12], pkt[13], pkt[14], pkt[15]]);
    let dst = u32::from_ne_bytes([pkt[16], pkt[17], pkt[18], pkt[19]]);
    let sport = u16::from_be_bytes([pkt[ihl], pkt[ihl + 1]]);
    let dport = u16::from_be_bytes([pkt[ihl + 2], pkt[ihl + 3]]);
    Some((src, sport, dst, dport, is_tcp))
}

/// 判断一个包归属哪个程序 key（找不到就归 OTHER）
fn classify(
    pm: &proc_map::ProcMap,
    pid_key: &HashMap<(u32, Direction), String>,
    pkt: &[u8],
    dir: Direction,
) -> String {
    let tup = match parse_tuple(pkt) {
        Some(t) => t,
        None => return OTHER.to_string(),
    };
    let outbound = dir == Direction::Up;
    pm.lookup_pid(tup.0, tup.1, tup.2, tup.3, tup.4, outbound)
        .and_then(|pid| pid_key.get(&(pid, dir)).cloned())
        .unwrap_or_else(|| OTHER.to_string())
}

/// 选桶：优先程序专属桶，其次「其他流量」桶，都没有返回 None（=不限速）
fn pick_bucket(
    buckets: &HashMap<StatKey, TokenBucket>,
    key: &str,
    dir: Direction,
) -> Option<StatKey> {
    let own = (key.to_string(), dir);
    if buckets.contains_key(&own) {
        return Some(own);
    }
    let other = (OTHER.to_string(), dir);
    if buckets.contains_key(&other) {
        return Some(other);
    }
    None
}

pub struct WinDivertEngine {
    /// 两个全局句柄；为空表示当前没有任何限速规则（不接管流量）
    handles: Vec<DirHandle>,
    /// (程序key|OTHER, 方向) -> 令牌桶
    buckets: HashMap<StatKey, TokenBucket>,
    /// (PID, 方向) -> 程序 key。只登记该方向确实设了限速的程序，
    /// 否则未在该方向单独限速的程序应走「其他流量」桶。
    pid_key: HashMap<(u32, Direction), String>,
    /// 本地快取，避免每个包都锁共享 stats
    counters: HashMap<StatKey, Arc<ByteCounter>>,
    stats: StatsMap,
    config: LimitConfig,
    proc_map: proc_map::ProcMap,
    /// 上一次的「速率签名」（不含 pids）：pids 每 3 秒刷新，只有速率真变了才重建桶
    rate_sig: Option<(Vec<(String, u64, u64)>, u64, u64)>,
    warning: Option<String>,
}

/// 速率签名：故意不含 pids。进程扫描导致 pids 变化时不应打断令牌桶或清空曲线。
fn rate_signature(config: &LimitConfig) -> (Vec<(String, u64, u64)>, u64, u64) {
    let mut v: Vec<(String, u64, u64)> = config
        .rules
        .iter()
        .filter(|r| r.is_active())
        .map(|r| (r.key.clone(), r.down_bps, r.up_bps))
        .collect();
    v.sort();
    (v, config.other_down_bps, config.other_up_bps)
}

impl Default for WinDivertEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl WinDivertEngine {
    pub fn new() -> Self {
        Self {
            handles: Vec::new(),
            buckets: HashMap::new(),
            pid_key: HashMap::new(),
            counters: HashMap::new(),
            stats: Arc::new(Mutex::new(HashMap::new())),
            config: LimitConfig::unlimited(),
            proc_map: proc_map::ProcMap::new(),
            rate_sig: Some(rate_signature(&LimitConfig::unlimited())),
            warning: None,
        }
    }

    /// 给 GUI 的共享统计表
    pub fn stats(&self) -> StatsMap {
        self.stats.clone()
    }

    pub fn config(&self) -> &LimitConfig {
        &self.config
    }

    /// 当前非致命提示（连接表读取失败 / 规则被跳过等）
    pub fn warning(&self) -> Option<&str> {
        self.warning.as_deref()
    }

    /// 依据配置重建 pid→key、令牌桶，并按需开关全局句柄。
    fn rebuild(&mut self, config: &LimitConfig) -> Result<(), String> {
        // 期望的 (StatKey, 速率)；同时另建 pid→key 映射
        let mut desired: HashMap<StatKey, u64> = HashMap::new();
        let mut pid_key: HashMap<(u32, Direction), String> = HashMap::new();
        let mut notes: Vec<String> = Vec::new();

        for r in config.active_rules() {
            let pids = r.normalized_pids();
            if pids.is_empty() {
                notes.push(format!("程序 {} 当前没有活动进程，未限速", r.key));
                continue;
            }
            if r.down_bps > 0 {
                desired.insert((r.key.clone(), Direction::Down), r.down_bps);
                for pid in &pids {
                    pid_key.insert((*pid, Direction::Down), r.key.clone());
                }
            }
            if r.up_bps > 0 {
                desired.insert((r.key.clone(), Direction::Up), r.up_bps);
                for pid in &pids {
                    pid_key.insert((*pid, Direction::Up), r.key.clone());
                }
            }
        }
        if config.other_down_bps > 0 {
            desired.insert((OTHER.to_string(), Direction::Down), config.other_down_bps);
        }
        if config.other_up_bps > 0 {
            desired.insert((OTHER.to_string(), Direction::Up), config.other_up_bps);
        }

        // pid→key 每次都要更新：浏览器开新标签页、进程退出都会改变归属
        self.pid_key = pid_key;

        let sig = rate_signature(config);
        let rates_changed = self.rate_sig.as_ref() != Some(&sig);
        if rates_changed {
            // 复用已有桶并仅更新速率，避免每 3 秒把令牌重置为满、曲线清零
            self.buckets.retain(|k, _| desired.contains_key(k));
            for (k, rate) in desired {
                match self.buckets.get_mut(&k) {
                    Some(old) => old.set_rate(rate),
                    None => { self.buckets.insert(k, TokenBucket::new(rate)); }
                }
            }
            self.rate_sig = Some(sig);
        }

        // 没有任何限速 → 关闭全部句柄，让流量完全不经过用户态（暂停后瞬时恢复满速）
        if self.buckets.is_empty() {
            self.handles.clear();
        } else if self.handles.is_empty() {
            let down = DirHandle::open(Direction::Down)?;
            let up = DirHandle::open(Direction::Up).map_err(|e| format!("{e}"))?;
            self.handles.push(down);
            self.handles.push(up);
        }

        self.warning = notes.into_iter().next();
        Ok(())
    }
}

impl Engine for WinDivertEngine {
    fn apply(&mut self, config: LimitConfig) -> Result<(), String> {
        if config == self.config {
            return Ok(());
        }
        // 先重建；失败时不更新 self.config，下一轮会重试
        self.rebuild(&config)?;
        self.config = config;
        Ok(())
    }

    /// 轮询两个全局句柄，最多处理 max_packets 个包。
    fn poll(&mut self, max_packets: usize) -> Result<usize, String> {
        if self.handles.is_empty() {
            return Ok(0);
        }
        self.proc_map.refresh_if_due(Instant::now());

        let mut handled = 0;
        for _ in 0..max_packets {
            let mut idle = true;
            for i in 0..self.handles.len() {
                let h = &mut self.handles[i];
                let len = h.recv()?;
                if len == 0 {
                    continue;
                }
                idle = false;
                handled += 1;
                let dir = h.dir;

                let key = classify(&self.proc_map, &self.pid_key, &h.buf[..len], dir);
                let bucket_key = pick_bucket(&self.buckets, &key, dir);
                let pass = match &bucket_key {
                    Some(bk) => self.buckets.get_mut(bk).map(|b| b.try_consume(len as u64)).unwrap_or(true),
                    None => true,
                };

                if pass {
                    h.reinject(len)?;
                    // 统计用「程序 key」而非实际桶：界面上该程序能看到自己的实时速率，
                    // 即使它走的是「其他流量」兜底。
                    let sk = (key, dir);
                    let counter = match self.counters.get(&sk) {
                        Some(c) => c.clone(),
                        None => {
                            let c = Arc::new(ByteCounter::new());
                            self.counters.insert(sk.clone(), c.clone());
                            if let Ok(mut m) = self.stats.lock() {
                                m.insert(sk.clone(), c.clone());
                            }
                            c
                        }
                    };
                    counter.add(len as u64);
                }
                // 令牌不足：直接不重注入（丢弃），TCP 触发拥塞控制自然降速
            }
            if idle {
                break;
            }
        }

        // 连接表读取失败属于非致命降级：仍能全局限速，但按进程分派不准
        if let Some(e) = self.proc_map.last_error() {
            self.warning = Some(format!("进程识别不可用（{}），按程序限速可能不准确", e));
        }
        Ok(handled)
    }
}

/// 驱动不可用时的空引擎：GUI 照常启动，只是限速不生效。
pub struct NoopEngine;

impl Engine for NoopEngine {
    fn apply(&mut self, _config: LimitConfig) -> Result<(), String> {
        Err("限速引擎不可用：需要管理员权限与 WinDivert 驱动".to_string())
    }

    fn poll(&mut self, _max_packets: usize) -> Result<usize, String> {
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ipv4_tcp(src: [u8; 4], sport: u16, dst: [u8; 4], dport: u16, proto: u8) -> Vec<u8> {
        let mut p = vec![0u8; 40];
        p[0] = 0x45; // IPv4, IHL=5 (20 bytes)
        p[2..4].copy_from_slice(&40u16.to_be_bytes());
        p[6..8].copy_from_slice(&0u16.to_be_bytes()); // 无分片
        p[9] = proto;
        p[12..16].copy_from_slice(&src);
        p[16..20].copy_from_slice(&dst);
        p[20..22].copy_from_slice(&sport.to_be_bytes());
        p[22..24].copy_from_slice(&dport.to_be_bytes());
        p
    }

    #[test]
    fn parses_ipv4_tcp_tuple() {
        let p = ipv4_tcp([192, 168, 1, 2], 50000, [10, 0, 0, 1], 443, 6);
        let (src, sport, dst, dport, is_tcp) = parse_tuple(&p).unwrap();
        assert_eq!(sport, 50000);
        assert_eq!(dport, 443);
        assert!(is_tcp);
        assert_eq!(src, u32::from_ne_bytes([192, 168, 1, 2]));
        assert_eq!(dst, u32::from_ne_bytes([10, 0, 0, 1]));
    }

    #[test]
    fn parses_udp_and_rejects_fragment() {
        let p = ipv4_tcp([1, 2, 3, 4], 5353, [5, 6, 7, 8], 53, 17);
        assert!(!parse_tuple(&p).unwrap().4);

        let mut frag = p.clone();
        frag[6..8].copy_from_slice(&0x2001u16.to_be_bytes()); // offset != 0
        assert!(parse_tuple(&frag).is_none());
    }

    #[test]
    fn rejects_short_or_non_ipv4() {
        assert!(parse_tuple(&[0u8; 10]).is_none());
        let mut p = ipv4_tcp([1, 2, 3, 4], 80, [5, 6, 7, 8], 80, 6);
        p[0] = 0x65; // IPv6
        assert!(parse_tuple(&p).is_none());
    }

    #[test]
    fn base_filters_never_contain_process_id() {
        // 回归防线：NETWORK 层不支持 processId，一旦再引入就会让 WinDivertOpen 报 87
        for d in Direction::ALL {
            assert!(!d.base_filter().contains("processId"), "{}", d.label());
        }
    }

    #[test]
    fn bucket_precedence_program_then_other() {
        let mut buckets = HashMap::new();
        buckets.insert((OTHER.to_string(), Direction::Down), TokenBucket::new(1024));
        assert_eq!(
            pick_bucket(&buckets, "chrome", Direction::Down),
            Some((OTHER.to_string(), Direction::Down))
        );
        buckets.insert(("chrome".to_string(), Direction::Down), TokenBucket::new(2048));
        assert_eq!(
            pick_bucket(&buckets, "chrome", Direction::Down),
            Some(("chrome".to_string(), Direction::Down))
        );
        assert_eq!(pick_bucket(&buckets, "chrome", Direction::Up), None);
    }

    #[test]
    fn classify_maps_pid_to_program_key() {
        let mut pm = proc_map::ProcMap::new();
        let mut tcp = HashMap::new();
        tcp.insert((1, 1000, 2, 80), 4242);
        tcp.insert((2, 80, 1, 1000), 4242);
        pm.seed_for_test(tcp, HashMap::new());
        let mut pid_key = HashMap::new();
        pid_key.insert((4242, Direction::Up), "chrome".to_string());

        let pkt = ipv4_tcp([1, 1, 1, 1], 1000, [2, 2, 2, 2], 80, 6);
        // 表里的地址是数值 1/2，与包的 [1,1,1,1] 不一致 → 落到 OTHER
        assert_eq!(classify(&pm, &pid_key, &pkt, Direction::Up), OTHER);

        let addr_a = 1u32.to_ne_bytes();
        let addr_b = 2u32.to_ne_bytes();
        let pkt2 = ipv4_tcp(addr_a, 1000, addr_b, 80, 6);
        assert_eq!(classify(&pm, &pid_key, &pkt2, Direction::Up), "chrome");
    }

    #[test]
    fn pid_map_only_registers_limited_direction() {
        // 只限下行的程序，其上行流量必须走「其他流量」桶，不能被当成程序专属桶
        let mut pid_key = HashMap::new();
        pid_key.insert((4242, Direction::Down), "chrome".to_string());
        let mut buckets = HashMap::new();
        buckets.insert(("chrome".to_string(), Direction::Down), TokenBucket::new(1024));
        buckets.insert((OTHER.to_string(), Direction::Up), TokenBucket::new(2048));
        assert_eq!(
            pick_bucket(&buckets, &classify_placeholder(&pid_key, 4242, Direction::Up), Direction::Up),
            Some((OTHER.to_string(), Direction::Up))
        );
    }

    fn classify_placeholder(
        pid_key: &HashMap<(u32, Direction), String>,
        pid: u32,
        dir: Direction,
    ) -> String {
        pid_key.get(&(pid, dir)).cloned().unwrap_or_else(|| OTHER.to_string())
    }
}
