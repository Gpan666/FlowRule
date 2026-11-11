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
//!    （`inbound/outbound and (ip or ipv6) and (tcp or udp) and not impostor`，
//!    不含 processId，**也不含任何网卡条件**：句柄必须看到全部流量，
//!    「同一个包只限一次」由用户态按包内 IP 去重保证，见 [`Dedupe`]）；
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
use std::time::{Duration, Instant};

use libloading::{Library, Symbol};
use rl_core::{ByteCounter, Direction, Engine, LimitConfig, TokenBucket};

/// WinDivert 文档定义的最大包长 = 40 + 0xFFFF
const WINDIVERT_MTU_MAX: usize = 40 + 0xFFFF;
/// 包在驱动队列里被自动丢弃前的最长排队时间（毫秒）。
///
/// WinDivert 允许的范围是 100ms ~ 16000ms（默认 2000ms）。原实现的 20ms 低于
/// 合法下限，`WinDivertSetParam` 直接失败、实际仍是 2000ms，等于没设置。
/// 取 100ms：既给了用户态充足的排空时间，又避免令牌不足时包在驱动里积压 2 秒
/// 带来的巨大延迟与重复重传。
const QUEUE_TIME_MS: u64 = 100;
/// WINDIVERT_LAYER_NETWORK
const LAYER_NETWORK: u32 = 0;
/// WINDIVERT_PARAM_QUEUE_TIME
const PARAM_QUEUE_TIME: u32 = 1;
/// 重叠 I/O：操作已挂起，完成时事件会被置位
const ERROR_IO_PENDING: i32 = 997;
const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 258;
/// 「其他流量」在统计表 / 桶表里的键（真实程序 key 不会是它）
pub const OTHER: &str = "__other__";

/// 同一个包跨网卡被限多次的去重窗口。
///
/// VPN / 代理（Clash TUN、v2rayN、WireGuard…）在本机中转时，同一份数据会在
/// 虚拟网卡与物理网卡各出现一次，两个方向又各捕获一次 —— 一个字节最多被扣 4 次。
/// **只能在用户态按包内 IP 判**：filter 里按网卡猜过一次，猜错就是「流量根本不进句柄」。
///
/// 阈值取值依据：真实五元组重复出现的间隔至少是一个 TCP 重传超时（≥200ms，通常秒级），
/// 而跨网卡转发的副本在同一毫秒量级出队，200ms 之内同 IP 的重复一定是同一份流量。
const DEDUPE_WINDOW: Duration = Duration::from_millis(200);

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

/// Win32 `OVERLAPPED`（x64 布局，共 32 字节）。
///
/// 用更精确的字段声明而不是定长缓冲：`h_event` 需要读出来做等待。
/// `Internal` / `InternalHigh` 由内核读写，绝不能移动该结构体的地址，
/// 所以 `DirHandle` 里用 `Box<Overlapped>` 固定它的位置。
#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    h_event: Handle,
}

#[cfg_attr(windows, link(name = "kernel32"))]
extern "system" {
    fn CreateEventW(
        attributes: *mut std::ffi::c_void,
        manual_reset: i32,
        initial_state: i32,
        name: *const u16,
    ) -> Handle;
    fn ResetEvent(handle: Handle) -> i32;
    fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
    fn GetOverlappedResult(
        handle: Handle,
        overlapped: *mut Overlapped,
        transferred: *mut u32,
        wait: i32,
    ) -> i32;
    fn CloseHandle(handle: Handle) -> i32;
}

/// 句柄用整数表示而不是裸指针，这样天然满足 Send
type Handle = isize;
const INVALID_HANDLE: Handle = -1;

type FnOpen = unsafe extern "system" fn(*const c_char, u32, i16, u64) -> Handle;
/// 带 `lpOverlapped` 的收包接口：挂起重叠 I/O，从而可以「等一小会儿就拿走」，
/// 不会像 `WinDivertRecv` 那样在一个方向没包时永远阻塞。
type FnRecvEx = unsafe extern "system" fn(
    Handle,
    *mut u8,
    u32,
    *mut u32,
    u64,
    *mut Address,
    *mut u32,
    *mut Overlapped,
) -> i32;
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
    recv_ex: FnRecvEx,
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
        let recv_ex: Symbol<FnRecvEx> = symbol(&dll, b"WinDivertRecvEx\0")?;
        let send: Symbol<FnSend> = symbol(&dll, b"WinDivertSend\0")?;
        let close: Symbol<FnClose> = symbol(&dll, b"WinDivertClose\0")?;
        let set_param: Symbol<FnSetParam> = symbol(&dll, b"WinDivertSetParam\0")?;
        let compile_filter: Symbol<FnCompileFilter> = symbol(&dll, b"WinDivertHelperCompileFilter\0")?;

        let api = Api {
            open: *open,
            recv_ex: *recv_ex,
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
    buf: Box<[u8]>,
    /// 地址 / 长度 / OVERLAPPED 都放进 Box：重叠 I/O 挂起期间驱动会异步回写，
    /// 这些内存必须保持地址稳定（不能是栈变量）
    addr: Box<Address>,
    recv_len: Box<u32>,
    addr_len: Box<u32>,
    overlapped: Box<Overlapped>,
    /// 是否已有一个收包请求挂在驱动里等待完成
    recv_pending: bool,
    h_event: Handle,
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

        let h_event = unsafe { CreateEventW(std::ptr::null_mut(), 0, 0, std::ptr::null()) };
        if h_event == 0 {
            let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            unsafe { (api.close)(handle) };
            return Err(format!("创建收包事件失败（Win32 错误 {code}）"));
        }

        Ok(Self {
            dir,
            handle,
            buf: vec![0u8; WINDIVERT_MTU_MAX].into_boxed_slice(),
            addr: Box::new(Address([0; 80])),
            recv_len: Box::new(0),
            addr_len: Box::new(std::mem::size_of::<Address>() as u32),
            overlapped: Box::new(Overlapped {
                internal: 0,
                internal_high: 0,
                offset: 0,
                offset_high: 0,
                h_event,
            }),
            recv_pending: false,
            h_event,
        })
    }

    /// 收一个包：`Some(包长)` 表示取到了包，`None` 表示本轮该方向没包。
    ///
    /// 必须用 `WinDivertRecvEx` + 重叠 I/O。原来的 `WinDivertRecv` 在没有包时
    /// **会一直阻塞**（不是「队列超时返回」），而引擎又是两个句柄串行轮询：
    /// 下载时上行只有零星 ACK，收下行后就会卡在上行 recv 上，直到下一个 ACK
    /// 才继续处理一个下行包 → 整机吞吐被压到「每 RTT 一个包」的量级。
    /// 这就是「只限一个程序，整台电脑却像被限速」的真正原因。
    ///
    /// 排空队列时传 `timeout_ms = 0`：只在包**已经**就绪时取走，绝不阻塞。
    fn recv(&mut self, timeout_ms: u32) -> Result<Option<usize>, String> {
        let api = api()?;
        if !self.recv_pending {
            // 上一次若同步完成，事件可能仍处于置位状态；先清掉，避免本轮误判为已完成。
            // ⚠️ 只在「没有挂起请求」时清事件：驱动已经置位的事件若被清掉，
            // 下一次 WaitForSingleObject 会一直等下去（表现为该方向彻底收不到包）。
            unsafe { ResetEvent(self.h_event) };
            *self.recv_len = 0;
            *self.addr_len = std::mem::size_of::<Address>() as u32;
            let ok = unsafe {
                (api.recv_ex)(
                    self.handle,
                    self.buf.as_mut_ptr(),
                    self.buf.len() as u32,
                    &mut *self.recv_len,
                    0,
                    &mut *self.addr,
                    &mut *self.addr_len,
                    &mut *self.overlapped,
                )
            };
            if ok != 0 {
                // 已完成（队列里本来就有包）
                return Ok(Some(*self.recv_len as usize));
            }
            let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if code != ERROR_IO_PENDING {
                return Err(format!("收包失败（Win32 错误 {code}）"));
            }
            self.recv_pending = true;
        }

        match unsafe { WaitForSingleObject(self.h_event, timeout_ms) } {
            // 超时后也要问一次 GetOverlappedResult：包可能在「事件置位」与「清除」之间
            // 到达，事件已被消费掉，此时请求其实已经完成。不查就会一直 pending、
            // 这个方向从此收不到包（配合「再挂一个请求」更是双输）。
            WAIT_TIMEOUT => {
                let mut transferred: u32 = 0;
                let ok = unsafe {
                    GetOverlappedResult(self.handle, &mut *self.overlapped, &mut transferred, 0)
                };
                if ok == 0 {
                    let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                    // 仍在等（或事件未置位）→ 保持 pending，下一轮继续等
                    const ERROR_IO_INCOMPLETE: i32 = 996;
                    if code == ERROR_IO_INCOMPLETE || code == ERROR_IO_PENDING || code == 0 {
                        return Ok(None);
                    }
                    self.recv_pending = false;
                    return Err(format!("收包完成失败（Win32 错误 {code}）"));
                }
                self.recv_pending = false;
                Ok(Some(transferred as usize))
            }
            WAIT_OBJECT_0 => {
                let mut transferred: u32 = 0;
                let ok = unsafe {
                    GetOverlappedResult(
                        self.handle,
                        &mut *self.overlapped,
                        &mut transferred,
                        0,
                    )
                };
                self.recv_pending = false;
                if ok == 0 {
                    let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                    return Err(format!("收包完成失败（Win32 错误 {code}）"));
                }
                Ok(Some(transferred as usize))
            }
            other => {
                // 操作可能仍挂在驱动里：保留 pending，下一轮继续等，避免对同一个
                // OVERLAPPED 重复发起收包
                Err(format!("等待收包事件失败（Win32 返回 {other}）"))
            }
        }
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
                &*self.addr,
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
            // 先关 WinDivert：会取消挂起的重叠收包，之后再释放事件对象
            unsafe { (api.close)(self.handle) };
        }
        // 非 Windows（CI 上跑纯逻辑测试）没有 kernel32，关句柄只可能发生在
        // `WinDivertOpen` 之后，那些代码在测试里跑不到，这里直接跳过。
        #[cfg(windows)]
        unsafe {
            CloseHandle(self.h_event);
        }
        #[cfg(not(windows))]
        let _ = self.h_event;
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

/// 解析 IPv6 包的地址/端口。
///
/// 固定 40 字节头 + 可能存在的扩展头（跳 hop / 路由 / 目的选项 / 分片）。
/// 非首片分片没有传输层端口信息，返回 None → 归入「其他流量」。
fn parse_v6(pkt: &[u8]) -> Option<(proc_map::Flow, bool)> {
    if pkt.len() < 40 {
        return None;
    }
    let mut next = pkt[6];
    let mut off = 40usize;
    let mut is_tcp = false;
    // 扩展头链最多走 8 层，防止畸形包造成死循环
    for _ in 0..8 {
        match next {
            6 => {
                is_tcp = true;
                break;
            }
            17 => {
                is_tcp = false;
                break;
            }
            // hop-by-hop(0) / routing(43) / destination options(60)
            0 | 43 | 60 => {
                if off + 2 > pkt.len() {
                    return None;
                }
                let hdr_len = (pkt[off + 1] as usize + 1) * 8;
                next = pkt[off];
                off += hdr_len;
                if off + 4 > pkt.len() {
                    return None;
                }
            }
            // fragment(44)：首片才有端口
            44 => {
                if off + 8 > pkt.len() {
                    return None;
                }
                let frag = u16::from_be_bytes([pkt[off + 2], pkt[off + 3]]);
                if frag & 0xfff8 != 0 {
                    return None;
                }
                next = pkt[off];
                off += 8;
            }
            _ => return None,
        }
    }
    if off + 4 > pkt.len() {
        return None;
    }
    let mut src = [0u8; 16];
    src.copy_from_slice(&pkt[8..24]);
    let mut dst = [0u8; 16];
    dst.copy_from_slice(&pkt[24..40]);
    let sport = u16::from_be_bytes([pkt[off], pkt[off + 1]]);
    let dport = u16::from_be_bytes([pkt[off + 2], pkt[off + 3]]);
    Some((proc_map::Flow::V6 { src, sport, dst, dport }, is_tcp))
}

/// 解析包的地址/端口（IPv4 或 IPv6）与传输层协议。
///
/// - IPv4：IHL 决定头长，非首片分片没有端口 → None
/// - IPv6：40 字节定长头 + 扩展头链
pub fn parse_flow(pkt: &[u8]) -> Option<(proc_map::Flow, bool)> {
    if pkt.len() < 20 {
        return None;
    }
    let ver = pkt[0] >> 4;
    if ver == 6 {
        return parse_v6(pkt);
    }
    if ver != 4 {
        return None;
    }
    let ver_ihl = pkt[0];
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
    Some((proc_map::Flow::V4 { src, sport, dst, dport }, is_tcp))
}

/// 判断一个包归属哪个程序 key（找不到就归 OTHER）
fn classify(
    pm: &proc_map::ProcMap,
    pid_key: &HashMap<(u32, Direction), String>,
    pkt: &[u8],
    dir: Direction,
) -> String {
    let (flow, is_tcp) = match parse_flow(pkt) {
        Some(t) => t,
        None => return OTHER.to_string(),
    };
    let outbound = dir == Direction::Up;
    pm.lookup_pid(flow, is_tcp, outbound)
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

/// 跨网卡去重：同一个 IP 包在短时间内的重复副本只算一次。
///
/// 键是**包内的识别码**（IPv4 用 IP 头里的 identification 字段，IPv6 用流标签），
/// 不是五元组 —— VPN 中转会改写端口甚至地址，但同一份内层载荷的识别码是一致的；
/// 普通直连流量里相邻包的识别码也各不相同，所以不会误判。
///
/// 两个方向 **不能共用一个实例**：入站与出站是两份不同数据，
/// 上行的 ACK 和下行载荷必须各自记账。
struct Dedupe {
    seen: HashMap<u32, Instant>,
    last_gc: Instant,
}

impl Dedupe {
    fn new() -> Self {
        Self { seen: HashMap::new(), last_gc: Instant::now() }
    }

    /// 返回 true 表示「这个包已经处理过」，调用方应直接放行（不计账、不丢包）。
    fn is_duplicate(&mut self, pkt: &[u8], now: Instant) -> bool {
        let Some(id) = dedupe_key(pkt) else {
            return false;
        };
        self.gc(now);
        match self.seen.get(&id) {
            Some(t) if now.saturating_duration_since(*t) < DEDUPE_WINDOW => true,
            _ => {
                self.seen.insert(id, now);
                false
            }
        }
    }

    /// 惰性清理过期条目（每 2 个窗口扫一次，成本与流量无关）
    fn gc(&mut self, now: Instant) {
        if now.saturating_duration_since(self.last_gc) < DEDUPE_WINDOW * 2 {
            return;
        }
        self.last_gc = now;
        self.seen.retain(|_, t| now.saturating_duration_since(*t) < DEDUPE_WINDOW);
    }
}

/// 取包内识别码：IPv4 = identification（含协议位），IPv6 = 流标签。
/// 解析不出（分片、畸形包）返回 None → 不做去重，照常限速。
fn dedupe_key(pkt: &[u8]) -> Option<u32> {
    if pkt.len() < 20 {
        return None;
    }
    match pkt[0] >> 4 {
        4 => {
            let id = u16::from_be_bytes([pkt[4], pkt[5]]) as u32;
            // 加上协议号：同一 identification 的 TCP/UDP 是两个独立包
            Some((id << 8) | pkt[9] as u32)
        }
        6 => {
            if pkt.len() < 8 {
                return None;
            }
            // 流标签 = 低 20 位；再加「协议是否首片」无关，直接用它
            Some(u32::from_be_bytes([pkt[0] & 0x0f, pkt[1], pkt[2], pkt[3]]))
        }
        _ => None,
    }
}

/// 引擎实时诊断计数（用于定位「点了没效果」到底断在哪一环）
#[derive(Debug, Default, Clone)]
pub struct EngineDiag {
    /// 已打开的全局句柄数（0 = 完全没接管流量）
    pub handles: usize,
    /// 当前生效的令牌桶数
    pub buckets: usize,
    /// 累计收到的包数
    pub packets: u64,
    /// 命中限速桶的包数
    pub limited: u64,
    /// 因令牌不足被丢弃的包数
    pub dropped: u64,
    /// 成功识别到具体程序的包数
    pub program_hits: u64,
    /// 连接表条目数（TCP / UDP），v4 与 v6 分开统计
    pub tcp_entries: usize,
    pub udp_entries: usize,
    /// IPv6 连接表条目数（程序级限速要靠它才能覆盖 QUIC / HTTP3）
    pub tcp6_entries: usize,
    pub udp6_entries: usize,
    /// 当前 PID→程序 映射条目数
    pub pid_rules: usize,
    /// 被判定为「同一包的另一张网卡副本」而跳过的包数（跨网卡去重命中）
    pub duplicates: u64,
    /// 当前每个生效令牌桶的速率（人类可读），用于确认「到底限的是多少」
    pub bucket_desc: String,
}

pub struct WinDivertEngine {
    /// 两个全局句柄；为空表示当前没有任何限速规则（不接管流量）
    handles: Vec<DirHandle>,
    diag: EngineDiag,
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
    /// 每个方向一个：跨网卡 / 双向捕获的重复副本只记一次账
    dedupe: HashMap<Direction, Dedupe>,
    /// 已连续多少字节「既没命中限速桶、也没识别出程序」
    bypass_bytes: u64,
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
            diag: EngineDiag::default(),
            buckets: HashMap::new(),
            pid_key: HashMap::new(),
            counters: HashMap::new(),
            stats: Arc::new(Mutex::new(HashMap::new())),
            config: LimitConfig::unlimited(),
            proc_map: proc_map::ProcMap::new(),
            dedupe: HashMap::new(),
            bypass_bytes: 0,
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

    /// 实时诊断快照（句柄数 / 收包 / 命中 / 丢弃 / 连接表规模）
    pub fn diag(&self) -> EngineDiag {
        let mut d = self.diag.clone();
        d.handles = self.handles.len();
        d.buckets = self.buckets.len();
        d.pid_rules = self.pid_key.len();
        d.tcp_entries = self.proc_map.tcp_len();
        d.udp_entries = self.proc_map.udp_len();
        d.tcp6_entries = self.proc_map.tcp6_len();
        d.udp6_entries = self.proc_map.udp6_len();
        let mut parts: Vec<String> = self
            .buckets
            .iter()
            .map(|((k, dir), b)| {
                let name = if k == OTHER {
                    "其他流量".to_string()
                } else {
                    // 程序 key 是完整路径，只显示文件名
                    let s = k.replace('\\', "/");
                    s.rsplit('/').next().unwrap_or(k).to_string()
                };
                format!("{} {} = {}", name, dir.label(), rl_core::format_bps(b.rate()))
            })
            .collect();
        parts.sort();
        d.bucket_desc = parts.join(" · ");
        d
    }

    /// 依据配置重建 pid→key、令牌桶，并按需开关全局句柄。
    /// 当前是否存在**程序级**限速（「其他流量」桶不需要进程归属，不限）
    fn has_active_rule(&self) -> bool {
        self.config.rules.iter().any(|r| r.is_active())
    }

    /// 未识别流量是否已远超被限速的流量。
    ///
    /// 判据：连续 256KB 以上「既未命中限速桶、也未识别出程序」。
    /// 正常场景下即使用户在下载别的程序，未识别量也会被识别成功的流量不断清零，
    /// 只有「这套归属识别对你的网络完全失效」时才会连续累积到这个量级。
    fn bypass_ratio_exceeded(&self) -> bool {
        self.bypass_bytes > 256 * 1024
    }

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
    ///
    /// ## 为什么必须「分方向」而不是「每轮各取一个包」
    ///
    /// 两个方向各只有一个收包请求可以挂在驱动里。旧实现每轮循环对每个句柄
    /// 各取 **1 个**包，然后读一次 out 时间：
    /// 下行方向在高流量下队列里堆着成千上万个包，却和上行方向平分吞吐配额，
    /// 于是下行实际速度被压到「上行包数 × 单个处理能力」——表现为限速值完全不准、
    /// 甚至下载几乎卡死。
    ///
    /// 现在改成：每轮先把一个方向**排空**（在预算内），再去处理另一个方向。
    /// 队列里已有包时用 0 超时立即取走，循环期间绝不阻塞，
    /// 保证 `poll` 的「非阻塞」语义，后台线程不会被卡住（旧注释说的「串联阻塞」根因）。
    fn poll(&mut self, max_packets: usize) -> Result<usize, String> {
        if self.handles.is_empty() {
            return Ok(0);
        }
        self.proc_map.refresh_if_due(Instant::now());

        let mut budget = max_packets;
        let mut handled = 0;
        let mut first_err: Option<String> = None;

        while budget > 0 {
            let mut idle = true;
            for i in 0..self.handles.len() {
                while budget > 0 {
                    let h = &mut self.handles[i];
                    let len = match h.recv(0) {
                        Ok(Some(len)) => len,
                        Ok(None) => break,
                        Err(e) => {
                            first_err.get_or_insert(e);
                            break;
                        }
                    };
                    idle = false;
                    budget -= 1;
                    handled += 1;
                    self.diag.packets += 1;

                    // 跨网卡去重：VPN / 代理中转时同一个 IP 包会在多张网卡上各出现一次，
                    // 只记一次账（副本本身照样放行，绝不因为「重复」去丢包）。
                    let dir = h.dir;
                    let dup = self
                        .dedupe
                        .entry(dir)
                        .or_insert_with(Dedupe::new)
                        .is_duplicate(&h.buf[..len], Instant::now());
                    if dup {
                        self.diag.duplicates += 1;
                        if let Err(e) = h.reinject(len) {
                            first_err.get_or_insert(e);
                            // 重注入失败不能 continue：否则这个方向下一轮还是同一个包，
                            // 循环会一直空转（budget 照样扣，但包永远送不出去）
                            break;
                        }
                        continue;
                    }
                    let key = classify(&self.proc_map, &self.pid_key, &h.buf[..len], dir);
                    if key != OTHER {
                        self.diag.program_hits += 1;
                        // 归属成功说明这套识别链路是通的，之前的「未生效」提示作废
                        self.bypass_bytes = 0;
                    }
                    let bucket_key = pick_bucket(&self.buckets, &key, dir);
                    if bucket_key.is_some() {
                        self.diag.limited += 1;
                    } else if key == OTHER {
                        // 既没进任何桶、也没认出程序：这一段流量对我们完全隐形
                        self.bypass_bytes += len as u64;
                    }
                    let pass = match &bucket_key {
                        Some(bk) => self
                            .buckets
                            .get_mut(bk)
                            .map(|b| b.try_consume(len as u64))
                            .unwrap_or(true),
                        None => true,
                    };
                    if !pass {
                        self.diag.dropped += 1;
                    }

                    if pass {
                        if let Err(e) = h.reinject(len) {
                            // 单个包重注入失败（驱动队列满等）不代表整轮失败：
                            // 记下第一个错误就跳出这个方向，让另一个方向照常处理。
                            first_err.get_or_insert(e);
                            break;
                        }
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
                if budget == 0 {
                    break;
                }
            }
            if idle {
                break;
            }
        }

        // 连接表读取失败属于非致命降级：仍能全局限速，但按进程分派不准
        if let Some(e) = self.proc_map.last_error() {
            self.warning = Some(format!("进程识别不可用（{}），按程序限速可能不准确", e));
        } else if self.has_active_rule() && self.bypass_ratio_exceeded() {
            // 有程序级限速却几乎全是「归不到任何程序」的流量 → 限速注定不生效。
            // 只在真正有流量时提示，避免用户没下载时看到吓人的警告。
            self.warning = Some(
                "有限速流量未被接管：这些流量的归属识别失败（可能是 VPN / 加密代理中转、                 或系统连接表权限不足），对应程序的限速不会生效。诊断里的「程序识别」                 与「命中限速」长期为 0 即为此症状"
                    .to_string(),
            );
        }

        match first_err {
            // 已经处理过包就先把结果交回去，下次 poll 再报错，避免丢掉已完成的统计
            Some(e) if handled == 0 => Err(e),
            Some(_) => Ok(handled),
            None => Ok(handled),
        }
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

    /// 构造一个 IPv6 包（40 字节固定头 + 4 字节伪传输层头）
    fn ipv6(src: [u8; 16], sport: u16, dst: [u8; 16], dport: u16, proto: u8) -> Vec<u8> {
        let mut p = vec![0u8; 44];
        p[0] = 0x60; // version=6
        p[4..6].copy_from_slice(&44u16.to_be_bytes()); // payload length
        p[6] = proto; // next header
        p[8..24].copy_from_slice(&src);
        p[24..40].copy_from_slice(&dst);
        p[40..42].copy_from_slice(&sport.to_be_bytes());
        p[42..44].copy_from_slice(&dport.to_be_bytes());
        p
    }

    #[test]
    fn parses_ipv6_flow() {
        let src = [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
        let dst = [0x26, 0x06, 0x47, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x68, 0x10];
        let p = ipv6(src, 51000, dst, 443, 6);
        let (flow, is_tcp) = parse_flow(&p).unwrap();
        assert!(is_tcp);
        match flow {
            proc_map::Flow::V6 { src: s, sport, dst: d, dport } => {
                assert_eq!(s, src);
                assert_eq!(d, dst);
                assert_eq!(sport, 51000);
                assert_eq!(dport, 443);
            }
            _ => panic!("应为 IPv6 流"),
        }
    }

    #[test]
    fn parses_ipv6_with_extension_header() {
        let mut p = ipv6([0x20, 0x01, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3], 1234,
            [0x20, 0x01, 4, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 6], 443, 6);
        // 插入一个 8 字节的 hop-by-hop 扩展头（next header = TCP）
        p[6] = 0; // hop-by-hop
        let mut ext = vec![0u8; 8];
        ext[0] = 6; // next header = TCP
        ext[1] = 0; // length = 0 → 8 字节
        p.splice(40..40, ext);
        let (flow, is_tcp) = parse_flow(&p).unwrap();
        assert!(is_tcp);
        match flow {
            proc_map::Flow::V6 { sport, dport, .. } => {
                assert_eq!(sport, 1234);
                assert_eq!(dport, 443);
            }
            _ => panic!("应为 IPv6 流"),
        }
    }

    #[test]
    fn ipv4_flow_still_parses() {
        let p = ipv4_tcp([192, 168, 1, 2], 50000, [10, 0, 0, 1], 443, 6);
        let (flow, is_tcp) = parse_flow(&p).unwrap();
        assert!(is_tcp);
        match flow {
            proc_map::Flow::V4 { src, sport, dst, dport } => {
                assert_eq!(src, u32::from_ne_bytes([192, 168, 1, 2]));
                assert_eq!(dst, u32::from_ne_bytes([10, 0, 0, 1]));
                assert_eq!(sport, 50000);
                assert_eq!(dport, 443);
            }
            _ => panic!("应为 IPv4 流"),
        }
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

    /// ★ 回归：同一个包在两张网卡上的副本只记一次账，且副本本身必须放行。
    ///
    /// 用户现场：Clash / TUN 模式下同一份下载走虚拟网卡 + 物理网卡，
    /// 不区分就会扣两次令牌 → 实测速率正好是设定值的一半。
    #[test]
    fn duplicate_copy_is_counted_once() {
        let mut d = Dedupe::new();
        let p = ipv4_tcp([1, 2, 3, 4], 5000, [5, 6, 7, 8], 443, 6);
        let t0 = Instant::now();
        assert!(!d.is_duplicate(&p, t0), "第一个副本要正常限速");
        assert!(d.is_duplicate(&p, t0), "同一包的另一张网卡副本必须被识别为重复");
        assert!(d.is_duplicate(&p, t0 + Duration::from_millis(50)));

        // 过了去重窗口，同样的 identification 视为新包（TCP 重传不能误伤）
        assert!(!d.is_duplicate(&p, t0 + DEDUPE_WINDOW + Duration::from_millis(1)));
    }

    /// 不同包不能互相吃掉：identification / 协议不同即为不同包。
    #[test]
    fn different_packets_are_not_deduped() {
        let mut d = Dedupe::new();
        let t0 = Instant::now();
        let a = ipv4_tcp([1, 2, 3, 4], 5000, [5, 6, 7, 8], 443, 6);
        let mut b = a.clone();
        b[4..6].copy_from_slice(&0x1234u16.to_be_bytes()); // 换个 identification
        let mut c = a.clone();
        c[9] = 17; // 同 identification 的 UDP（协议号不同）
        assert!(!d.is_duplicate(&a, t0));
        assert!(!d.is_duplicate(&b, t0), "识别码不同的包不能被当成副本");
        assert!(!d.is_duplicate(&c, t0), "协议不同的包不能被当成副本");
    }

    /// IPv6 用流标签做键；分片 / 畸形包解析不出键时不做去重（宁可多限不可漏限）。
    #[test]
    fn ipv6_dedupe_uses_flow_label() {
        let mut d = Dedupe::new();
        let t0 = Instant::now();
        let mut p = ipv6([1u8; 16], 5000, [2u8; 16], 443, 6);
        p[0] = 0x60; // 流标签高 4 位
        p[1] = 0xab;
        p[2] = 0xcd;
        p[3] = 0xef;
        assert!(!d.is_duplicate(&p, t0));
        assert!(d.is_duplicate(&p, t0));
        assert!(dedupe_key(&[0u8; 10]).is_none());
    }

    /// ★ 回归：有程序级限速、但流量全都归不到任何程序时，必须明确告警。
    ///
    /// 这就是用户报的「收包数很小、命中限速 0、程序识别 0、丢弃 0，下载却跑满带宽」：
    /// 归属识别对这套网络失效，限速从设计上就不可能生效，不能静默。
    #[test]
    fn warns_when_limited_traffic_is_never_attributed() {
        let mut e = WinDivertEngine::new();
        assert!(!e.has_active_rule(), "默认没有任何限速规则");

        e.config = LimitConfig {
            rules: vec![rl_core::ProgramRule {
                key: "c:\\starward.exe".into(),
                pids: vec![4242],
                down_bps: 5 * 1024 * 1024,
                up_bps: 0,
            }],
            ..Default::default()
        };
        assert!(e.has_active_rule());

        // 只有零星未识别流量：不打扰用户
        e.bypass_bytes = 8 * 1024;
        assert!(!e.bypass_ratio_exceeded());

        // 未识别流量持续累积：判定为「限速完全没接管」
        e.bypass_bytes = 512 * 1024;
        assert!(e.bypass_ratio_exceeded());

        // 一旦有流量成功归属，计数清零、告警作废
        e.bypass_bytes = 0;
        assert!(!e.bypass_ratio_exceeded());
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
