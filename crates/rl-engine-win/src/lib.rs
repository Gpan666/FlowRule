//! Windows 限速引擎：接管网络包 → 令牌桶判定 → 放行重注入 / 丢弃。
//!
//! 通过 `libloading` 在运行时动态加载 WinDivert.dll，而不是编译期链接 `.lib`：
//! - 不需要 MSVC 生成工具，GNU 工具链同样能编译；
//! - 驱动缺失时只是返回错误，程序不崩溃。
//!
//! 支持**按进程**限速：每个进程、每个方向各开一个 WinDivert 句柄，filter 里带
//! `processId == N`。「其他流量」的 filter 会显式排除这些 pid，保证句柄之间不重叠
//! （重叠会导致同一个包被重注入两次）。
//!
//! 单线程 reactor 模型：GUI 通过 channel 下发 `LimitConfig`，引擎线程 `poll` 处理一批包。

use std::collections::HashMap;
use std::ffi::{c_char, CString};
use std::sync::{Arc, Mutex, OnceLock};

use libloading::{Library, Symbol};
use rl_core::{
    build_filter, build_program_filter, ByteCounter, Direction, Engine, LimitConfig, TokenBucket,
};

/// WinDivert 文档定义的最大包长 = 40 + 0xFFFF
const WINDIVERT_MTU_MAX: usize = 40 + 0xFFFF;
/// recv 队列超时（毫秒）。设短一点，保证 poll 能及时返回去响应新配置
const QUEUE_TIME_MS: u64 = 4;
/// WINDIVERT_LAYER_NETWORK
const LAYER_NETWORK: u32 = 0;
/// WINDIVERT_PARAM_QUEUE_TIME
const PARAM_QUEUE_TIME: u32 = 1;
/// 「其他流量」在统计表 / 句柄表里的键（真实 pid 不会是空字符串）
pub const OTHER: &str = "__other__";

/// 统计维度：(程序 key 或 OTHER, 方向)
pub type StatKey = (String, Direction);
pub type StatsMap = Arc<Mutex<HashMap<StatKey, Arc<ByteCounter>>>>;

/// WINDIVERT_ADDRESS：只做透传（recv 填、send 传回），不解释内部位域，
/// 所以用定长缓冲表示，避免位域布局对不齐导致的问题。
#[repr(C, align(8))]
#[derive(Clone, Copy)]
struct Address([u8; 64]);

/// 句柄用整数表示而不是裸指针，这样 `Limiter` 天然满足 Send（要在线程间移动）
type Handle = isize;
const INVALID_HANDLE: Handle = -1;

type FnOpen = unsafe extern "system" fn(*const c_char, u32, i16, u64) -> Handle;
type FnRecv = unsafe extern "system" fn(Handle, *mut u8, u32, *mut u32, *mut Address) -> i32;
type FnSend = unsafe extern "system" fn(Handle, *const u8, u32, *mut u32, *const Address) -> i32;
type FnClose = unsafe extern "system" fn(Handle) -> i32;
type FnSetParam = unsafe extern "system" fn(Handle, u32, u64) -> i32;

#[derive(Clone, Copy)]
struct Api {
    open: FnOpen,
    recv: FnRecv,
    send: FnSend,
    close: FnClose,
    set_param: FnSetParam,
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

        let open: Symbol<FnOpen> = symbol(&dll, b"WinDivertOpen")?;
        let recv: Symbol<FnRecv> = symbol(&dll, b"WinDivertRecv")?;
        let send: Symbol<FnSend> = symbol(&dll, b"WinDivertSend")?;
        let close: Symbol<FnClose> = symbol(&dll, b"WinDivertClose")?;
        let set_param: Symbol<FnSetParam> = symbol(&dll, b"WinDivertSetParam")?;

        let api = Api {
            open: *open,
            recv: *recv,
            send: *send,
            close: *close,
            set_param: *set_param,
        };
        // 函数指针脱离 Symbol 的生命周期，必须让 DLL 常驻内存
        std::mem::forget(dll);
        Ok(api)
    }
}

fn symbol<'a, T>(dll: &'a Library, name: &[u8]) -> Result<Symbol<'a, T>, String> {
    unsafe { dll.get(name) }.map_err(|e| format!("找不到 {}: {e}", String::from_utf8_lossy(name)))
}

/// 一个「程序 + 方向」对应一个句柄和一个令牌桶
struct Limiter {
    handle: Handle,
    filter: String,
    bucket: TokenBucket,
    buf: Vec<u8>,
    addr: Address,
}

impl Limiter {
    /// 打开句柄。`label` 只用于错误提示（程序 key 或 OTHER）
    fn open(label: &str, direction: Direction, filter: &str, bps: u64) -> Result<Self, String> {
        let api = api()?;
        let expr = CString::new(filter).map_err(|_| "过滤表达式非法".to_string())?;

        let handle = unsafe { (api.open)(expr.as_ptr(), LAYER_NETWORK, 0, 0) };
        if handle == 0 || handle == INVALID_HANDLE {
            return Err(format!(
                "打开 {}（{}）句柄失败，Win32 错误 {}。请以管理员身份运行，并确认 WinDivert64.sys 在程序同目录",
                direction.label(),
                label,
                std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
            ));
        }
        unsafe { (api.set_param)(handle, PARAM_QUEUE_TIME, QUEUE_TIME_MS) };

        Ok(Self {
            handle,
            filter: filter.to_string(),
            bucket: TokenBucket::new(bps),
            buf: vec![0u8; WINDIVERT_MTU_MAX],
            addr: Address([0; 64]),
        })
    }

    /// 收一个包并判定；返回是否真的处理了一个包
    fn step(&mut self, counter: &ByteCounter) -> Result<bool, String> {
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
            return Ok(false); // 队列超时，本轮没包
        }
        let len = len as usize;

        if self.bucket.try_consume(len as u64) {
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
                return Err("重注入失败".to_string());
            }
            counter.add(len as u64);
        }
        // 令牌不足直接丢弃：TCP 触发拥塞控制自然降速，UDP 直接丢
        Ok(true)
    }
}

impl Drop for Limiter {
    fn drop(&mut self) {
        if let Ok(api) = api() {
            unsafe { (api.close)(self.handle) };
        }
    }
}

pub struct WinDivertEngine {
    limiters: HashMap<StatKey, Limiter>,
    /// 轮询顺序，避免每轮重新收集 keys
    order: Vec<StatKey>,
    stats: StatsMap,
    config: LimitConfig,
    /// 上一次 apply 的非致命提示（如「其他流量」因进程太多被暂停）
    warning: Option<String>,
}

impl WinDivertEngine {
    pub fn new() -> Self {
        Self {
            limiters: HashMap::new(),
            order: Vec::new(),
            stats: Arc::new(Mutex::new(HashMap::new())),
            config: LimitConfig::unlimited(),
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

    /// 上一次 apply 的非致命提示（None 表示一切正常）
    pub fn warning(&self) -> Option<&str> {
        self.warning.as_deref()
    }

    /// 根据配置算出「应该存在哪些句柄」：key -> (filter, 速率)，外加非致命提示。
    ///
    /// - 每个限速程序一个句柄（上下行各一个），filter 用 `(pid or pid ...)` 合并所有进程；
    /// - 「其他流量」每个方向一个句柄，filter 只能排除**该方向**已覆盖的进程，
    ///   否则没被覆盖的那部分流量会被两个句柄同时收到 → 重复注入。
    ///   进程太多排除不下时宁可暂停「其他流量」限速，也要保证程序限速正确生效。
    fn wanted(
        &self,
        cfg: &LimitConfig,
    ) -> Result<(HashMap<StatKey, (String, u64)>, Option<String>), String> {
        let mut want: HashMap<StatKey, (String, u64)> = HashMap::new();
        let mut covered_down: Vec<u32> = Vec::new();
        let mut covered_up: Vec<u32> = Vec::new();
        let mut notes: Vec<String> = Vec::new();

        for r in cfg.active_rules() {
            let pids = r.normalized_pids();
            if pids.is_empty() {
                notes.push(format!("程序 {} 当前没有活动进程，未限速", r.key));
                continue;
            }
            if r.down_bps > 0 {
                match build_program_filter(Direction::Down, &pids) {
                    Ok((f, cov)) => {
                        want.insert((r.key.clone(), Direction::Down), (f, r.down_bps));
                        covered_down.extend(cov);
                    }
                    Err(e) => notes.push(format!("程序 {} 下行未限速：{}", r.key, e)),
                }
            }
            if r.up_bps > 0 {
                match build_program_filter(Direction::Up, &pids) {
                    Ok((f, cov)) => {
                        want.insert((r.key.clone(), Direction::Up), (f, r.up_bps));
                        covered_up.extend(cov);
                    }
                    Err(e) => notes.push(format!("程序 {} 上行未限速：{}", r.key, e)),
                }
            }
        }

        add_other(
            Direction::Down,
            cfg.other_down_bps,
            &mut covered_down,
            &mut want,
            &mut notes,
        );
        add_other(
            Direction::Up,
            cfg.other_up_bps,
            &mut covered_up,
            &mut want,
            &mut notes,
        );

        Ok((want, notes.into_iter().next()))
    }
}

/// 「其他流量」句柄：排除本方向已覆盖的进程后才打开，保证不与程序句柄重叠。
fn add_other(
    dir: Direction,
    bps: u64,
    covered: &mut Vec<u32>,
    want: &mut HashMap<StatKey, (String, u64)>,
    notes: &mut Vec<String>,
) {
    if bps == 0 {
        return;
    }
    covered.sort_unstable();
    covered.dedup();
    if covered.len() > rl_core::process::MAX_EXCLUDES {
        notes.push(format!(
            "单独限速的程序覆盖进程过多（>{} 个），已暂停「其他流量」的{}限速",
            rl_core::process::MAX_EXCLUDES,
            dir.label()
        ));
        return;
    }
    match build_filter(dir, None, covered) {
        Ok(f) => {
            want.insert((OTHER.to_string(), dir), (f, bps));
        }
        Err(e) => notes.push(format!("「其他流量」{}未限速：{}", dir.label(), e)),
    }
}

impl Default for WinDivertEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine for WinDivertEngine {
    fn apply(&mut self, config: LimitConfig) -> Result<(), String> {
        self.warning = None;
        if config == self.config {
            return Ok(());
        }
        let (want, warning) = self.wanted(&config)?;
        self.warning = warning;

        // 不再需要的句柄关掉（Drop 里会 close）
        self.limiters.retain(|k, _| want.contains_key(k));

        // filter 变了必须重开：否则「其他流量」会和程序句柄重叠，导致重复注入
        let stale: Vec<StatKey> = self
            .limiters
            .iter()
            .filter(|(k, l)| want[*k].0 != l.filter)
            .map(|(k, _)| k.clone())
            .collect();
        for k in stale {
            self.limiters.remove(&k);
        }

        for (key, (filter, bps)) in want {
            match self.limiters.get_mut(&key) {
                Some(l) => l.bucket.set_rate(bps),
                None => {
                    let (ref label, dir) = key;
                    let limiter = Limiter::open(label, dir, &filter, bps)?;
                    self.limiters.insert(key.clone(), limiter);
                    self.stats
                        .lock()
                        .unwrap()
                        .entry(key)
                        .or_insert_with(|| Arc::new(ByteCounter::new()));
                }
            }
        }
        // 清掉已停用的统计项
        self.stats.lock().unwrap().retain(|k, _| self.limiters.contains_key(k));

        let mut order: Vec<StatKey> = self.limiters.keys().cloned().collect();
        order.sort();
        self.order = order;
        self.config = config;
        Ok(())
    }

    /// 所有句柄轮流处理，保证公平；每轮最多 max_packets 个包
    fn poll(&mut self, max_packets: usize) -> Result<usize, String> {
        let mut handled = 0;
        for _ in 0..max_packets {
            let mut idle = true;
            for key in self.order.clone() {
                let counter = self.stats.lock().unwrap().get(&key).cloned();
                if let (Some(limiter), Some(counter)) = (self.limiters.get_mut(&key), counter) {
                    if limiter.step(&counter)? {
                        idle = false;
                        handled += 1;
                    }
                }
            }
            if idle {
                break;
            }
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
    use rl_core::{LimitConfig, ProgramRule};

    fn prog(key: &str, pids: Vec<u32>, down: u64, up: u64) -> ProgramRule {
        ProgramRule { key: key.to_string(), pids, down_bps: down, up_bps: up }
    }

    fn has<'a>(want: &'a HashMap<StatKey, (String, u64)>, key: &str, dir: Direction) -> Option<&'a str> {
        want.get(&(key.to_string(), dir)).map(|(f, _)| f.as_str())
    }

    #[test]
    fn program_handle_union_and_other_excludes_them() {
        let e = WinDivertEngine::new();
        let cfg = LimitConfig {
            rules: vec![prog("chrome", vec![200, 100], 1024, 0)],
            other_down_bps: 2048,
            other_up_bps: 0,
        };
        let (want, warning) = e.wanted(&cfg).unwrap();
        assert!(warning.is_none());

        let pf = has(&want, "chrome", Direction::Down).unwrap();
        assert!(pf.contains("processId == 100") && pf.contains("processId == 200"));
        assert!(pf.contains(" or "), "同一程序的多个进程应该用 or 合并");
        assert!(has(&want, "chrome", Direction::Up).is_none(), "上行未限速则不开句柄");

        let of = has(&want, OTHER, Direction::Down).unwrap();
        assert!(of.contains("processId != 100") && of.contains("processId != 200"));
        assert!(has(&want, OTHER, Direction::Up).is_none());
    }

    #[test]
    fn other_up_does_not_exclude_down_only_program() {
        // 程序只限下行：它的上行流量必须仍由「其他流量:上行」接管，否则没人重注入
        let e = WinDivertEngine::new();
        let cfg = LimitConfig {
            rules: vec![prog("app", vec![7, 8], 1024, 0)],
            other_down_bps: 0,
            other_up_bps: 1024,
        };
        let (want, _) = e.wanted(&cfg).unwrap();

        let ou = has(&want, OTHER, Direction::Up).unwrap();
        assert!(
            !ou.contains("processId !="),
            "只限下行的程序，其上行必须留给其他流量，不能排除"
        );
        let pd = has(&want, "app", Direction::Down).unwrap();
        assert!(pd.contains("processId == 7") && pd.contains("processId == 8"));
        assert!(has(&want, OTHER, Direction::Down).is_none(), "其他流量下行未限速则不开句柄");
    }

    #[test]
    fn other_dropped_when_too_many_pids() {
        let e = WinDivertEngine::new();
        let rules: Vec<ProgramRule> = (0..25)
            .map(|i| prog(&format!("p{i}"), vec![1000 + i], 1024, 1024))
            .collect();
        let cfg = LimitConfig {
            rules,
            other_down_bps: 2048,
            other_up_bps: 2048,
        };
        let (want, warning) = e.wanted(&cfg).unwrap();
        assert_eq!(want.len(), 50, "程序句柄必须全部照常打开");
        assert!(has(&want, OTHER, Direction::Down).is_none());
        assert!(has(&want, OTHER, Direction::Up).is_none());
        assert!(warning.unwrap().contains("其他流量"));
    }

    #[test]
    fn empty_pids_reports_note() {
        let e = WinDivertEngine::new();
        let cfg = LimitConfig {
            rules: vec![prog("dead", vec![], 1024, 0)],
            other_down_bps: 0,
            other_up_bps: 0,
        };
        let (want, warning) = e.wanted(&cfg).unwrap();
        assert!(want.is_empty());
        assert!(warning.unwrap().contains("没有活动进程"));
    }
}
