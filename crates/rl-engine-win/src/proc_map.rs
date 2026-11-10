//! 进程归属映射：把「网络五元组」映射到 owning PID。
//!
//! ## 为什么需要这个模块
//!
//! WinDivert 2.2 的 `WINDIVERT_LAYER_NETWORK`（layer 0）**不支持 `processId` 字段**：
//! 官方文档明确写道 "this field is not supported by the WINDIVERT_LAYER_NETWORK* layers"，
//! 实测 `WinDivertHelperCompileFilter` 对含 `processId` 的表达式返回
//! "Filter expression contains a bad token for layer"。
//! 原实现给每个程序拼 `processId == N` 的 filter，导致 `WinDivertOpen` 必然失败。
//!
//! 因此改为：NETWORK 层只开全局句柄，收包后自己解析五元组，再用 Windows 的
//! `GetExtendedTcpTable` / `GetExtendedUdpTable`（iphlpapi.dll，`*_OWNER_PID` 表类）
//! 查出 owning PID，用户态决定该包属于哪个程序的限速规则。
//!
//! 用 `libloading` 动态加载 iphlpapi.dll，避免 GNU 工具链链接系统导入库的问题
//! （与本项目加载 WinDivert.dll 的做法一致，DLL 本身 Windows 自带）。
//!
//! ## 字节序约定
//!
//! - 地址：MIB 结构里的 `dw*Addr` 与 IP 包里的 4 字节都是**网络序字节**，
//!   在 x86 上按 `u32::from_le_bytes` / 原生 DWORD 读出的值一致，直接比较即可。
//! - 端口：MIB 的 `dw*Port` 低 16 位是网络序，需 swap 成主机序；
//!   IP 包端口用 `u16::from_be_bytes` 读出，也是主机序。

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use libloading::{Library, Symbol};

const AF_INET: u32 = 2;
/// TCP_TABLE_OWNER_PID_ALL：包含 listener / connections / established，且带 owning PID
const TCP_TABLE_OWNER_PID_ALL: u32 = 5;
/// UDP_TABLE_OWNER_PID
const UDP_TABLE_OWNER_PID: u32 = 1;
const NO_ERROR: u32 = 0;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

/// MIB_TCPROW_OWNER_PID：6 个 DWORD = 24 字节
const TCP_ROW_SIZE: usize = 24;
/// MIB_UDPROW_OWNER_PID：3 个 DWORD = 12 字节
const UDP_ROW_SIZE: usize = 12;
/// 两张表头都是 `DWORD dwNumEntries` + 紧随其后的行数组
const TABLE_HEADER_SIZE: usize = 4;

/// 映射刷新间隔。连接表查询要遍历系统全部连接，不能每个包都调；
/// 连接建立/关闭本身是秒级事件，700ms 足够准确。
pub const REFRESH_INTERVAL: Duration = Duration::from_millis(700);

/// TCP 键：(src_addr, src_port, dst_addr, dst_port)，双向都会插入
type TcpKey = (u32, u16, u32, u16);
/// UDP 键：(local_addr, local_port)
type UdpKey = (u32, u16);

type FnGetTable = unsafe extern "system" fn(
    *mut c_void, // pTable
    *mut u32,    // pdwSize
    i32,         // bOrder
    u32,         // ulAf
    u32,         // TableClass
    u32,         // Reserved
) -> u32;

struct IpHlpApi {
    tcp: FnGetTable,
    udp: FnGetTable,
}

fn api() -> Result<&'static IpHlpApi, String> {
    static API: OnceLock<Result<IpHlpApi, String>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let dll = Library::new("iphlpapi.dll")
            .map_err(|e| format!("加载 iphlpapi.dll 失败：{e}"))?;
        let tcp: Symbol<FnGetTable> = dll.get(b"GetExtendedTcpTable\0")
            .map_err(|e| format!("找不到 GetExtendedTcpTable：{e}"))?;
        let udp: Symbol<FnGetTable> = dll.get(b"GetExtendedUdpTable\0")
            .map_err(|e| format!("找不到 GetExtendedUdpTable：{e}"))?;
        let api = IpHlpApi { tcp: *tcp, udp: *udp };
        std::mem::forget(dll); // 函数指针脱离 Symbol 生命周期，DLL 需常驻
        Ok(api)
    })
    .as_ref()
    .map_err(|e| e.clone())
}

/// 调用 GetExtended*Table 的两步法：先问大小，再取数据。
fn read_table(f: FnGetTable, class: u32) -> Result<Vec<u8>, String> {
    unsafe {
        let mut size: u32 = 0;
        let r = f(std::ptr::null_mut(), &mut size, 0, AF_INET, class, 0);
        if r != NO_ERROR && r != ERROR_INSUFFICIENT_BUFFER {
            return Err(format!("查询连接表大小失败，Win32 错误 {r}"));
        }
        if size == 0 {
            return Ok(Vec::new());
        }
        // 表可能在两次调用之间增长；失败时按新 size 重试一次
        for _ in 0..2 {
            let mut buf = vec![0u8; size as usize];
            let r = f(buf.as_mut_ptr() as *mut c_void, &mut size, 0, AF_INET, class, 0);
            if r == NO_ERROR {
                return Ok(buf);
            }
            if r != ERROR_INSUFFICIENT_BUFFER {
                return Err(format!("读取连接表失败，Win32 错误 {r}"));
            }
        }
        Err("读取连接表失败：表持续增大".to_string())
    }
}

#[inline]
fn rd_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

/// MIB 端口 DWORD 的低 16 位是网络序 → 转主机序
#[inline]
pub fn mib_port(dw: u32) -> u16 {
    ((dw & 0xffff) as u16).swap_bytes()
}

/// 从 TCP owner-pid 表解析出双向五元组 → PID
pub fn parse_tcp_table(buf: &[u8]) -> HashMap<TcpKey, u32> {
    let mut map = HashMap::new();
    if buf.len() < TABLE_HEADER_SIZE {
        return map;
    }
    let n = rd_u32(buf, 0) as usize;
    for i in 0..n {
        let off = TABLE_HEADER_SIZE + i * TCP_ROW_SIZE;
        if off + TCP_ROW_SIZE > buf.len() {
            break;
        }
        // dwState(0), dwLocalAddr(4), dwLocalPort(8),
        // dwRemoteAddr(12), dwRemotePort(16), dwOwningPid(20)
        let la = rd_u32(buf, off + 4);
        let lp = mib_port(rd_u32(buf, off + 8));
        let ra = rd_u32(buf, off + 12);
        let rp = mib_port(rd_u32(buf, off + 16));
        let pid = rd_u32(buf, off + 20);
        if pid == 0 {
            continue;
        }
        // 正向：出站包 (src=local, dst=remote)
        map.insert((la, lp, ra, rp), pid);
        // 反向：入站包 (src=remote, dst=local)
        map.insert((ra, rp, la, lp), pid);
    }
    map
}

/// 从 UDP owner-pid 表解析出 (local_addr, local_port) → PID
pub fn parse_udp_table(buf: &[u8]) -> HashMap<UdpKey, u32> {
    let mut map = HashMap::new();
    if buf.len() < TABLE_HEADER_SIZE {
        return map;
    }
    let n = rd_u32(buf, 0) as usize;
    for i in 0..n {
        let off = TABLE_HEADER_SIZE + i * UDP_ROW_SIZE;
        if off + UDP_ROW_SIZE > buf.len() {
            break;
        }
        let la = rd_u32(buf, off);
        let lp = mib_port(rd_u32(buf, off + 4));
        let pid = rd_u32(buf, off + 8);
        if pid != 0 {
            map.insert((la, lp), pid);
        }
    }
    map
}

/// 五元组 → PID 的缓存映射
pub struct ProcMap {
    tcp: HashMap<TcpKey, u32>,
    udp: HashMap<UdpKey, u32>,
    last_refresh: Option<Instant>,
    /// 最近一次刷新是否成功（失败时给 UI 一个明确提示，而不是静默降级）
    last_error: Option<String>,
}

impl Default for ProcMap {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcMap {
    pub fn new() -> Self {
        Self { tcp: HashMap::new(), udp: HashMap::new(), last_refresh: None, last_error: None }
    }

    /// 按间隔刷新；返回是否需要把错误展示给用户。
    pub fn refresh_if_due(&mut self, now: Instant) {
        let due = self.last_refresh
            .map(|t| now.duration_since(t) >= REFRESH_INTERVAL)
            .unwrap_or(true);
        if !due {
            return;
        }
        self.last_refresh = Some(now);
        match self.load() {
            Ok(()) => self.last_error = None,
            Err(e) => self.last_error = Some(e),
        }
    }

    fn load(&mut self) -> Result<(), String> {
        let a = api()?;
        let tcp_buf = read_table(a.tcp, TCP_TABLE_OWNER_PID_ALL)?;
        let udp_buf = read_table(a.udp, UDP_TABLE_OWNER_PID)?;
        self.tcp = parse_tcp_table(&tcp_buf);
        self.udp = parse_udp_table(&udp_buf);
        Ok(())
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn tcp_len(&self) -> usize { self.tcp.len() }
    pub fn udp_len(&self) -> usize { self.udp.len() }

    /// 查一个包属于哪个进程。
    ///
    /// - `src/dst`：IP 包里的地址（按 `u32::from_le_bytes` 读出，与 MIB DWORD 一致）
    /// - `sport/dport`：主机序端口（`u16::from_be_bytes` 读出）
    /// - `is_tcp`：true=TCP, false=UDP
    /// - `outbound`：UDP 时决定用 src 还是 dst 作为「本端」
    #[inline]
    pub fn lookup_pid(
        &self,
        src: u32,
        sport: u16,
        dst: u32,
        dport: u16,
        is_tcp: bool,
        outbound: bool,
    ) -> Option<u32> {
        if is_tcp {
            self.tcp.get(&(src, sport, dst, dport)).copied()
        } else {
            let local = if outbound { (src, sport) } else { (dst, dport) };
            self.udp.get(&local).copied()
        }
    }

    #[cfg(test)]
    pub fn seed_for_test(&mut self, tcp: HashMap<TcpKey, u32>, udp: HashMap<UdpKey, u32>) {
        self.tcp = tcp;
        self.udp = udp;
        self.last_refresh = Some(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(state: u32, la: u32, lp: u16, ra: u32, rp: u16, pid: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity(TCP_ROW_SIZE);
        // MIB 端口字段：网络序放在 DWORD 低 16 位
        let enc_port = |p: u16| -> u32 { p.swap_bytes() as u32 };
        for x in [state, la, enc_port(lp), ra, enc_port(rp), pid] {
            v.extend_from_slice(&x.to_ne_bytes());
        }
        v
    }

    #[test]
    fn tcp_table_is_bidirectional() {
        let mut buf = 1u32.to_ne_bytes().to_vec();
        buf.extend(row(5, 0x0100007f, 50000, 0x0101a8c0, 443, 4242));
        let map = parse_tcp_table(&buf);
        // 出站包 src=local, dst=remote
        assert_eq!(map.get(&(0x0100007f, 50000, 0x0101a8c0, 443)), Some(&4242));
        // 入站包 src=remote, dst=local
        assert_eq!(map.get(&(0x0101a8c0, 443, 0x0100007f, 50000)), Some(&4242));
        assert_eq!(map.get(&(0x0100007f, 50000, 0x0101a8c0, 80)), None);
    }

    #[test]
    fn udp_table_maps_local_endpoint() {
        let mut buf = 1u32.to_ne_bytes().to_vec();
        let mut r = Vec::new();
        r.extend_from_slice(&0x0100007fu32.to_ne_bytes());
        r.extend_from_slice(&(53u16.swap_bytes() as u32).to_ne_bytes());
        r.extend_from_slice(&777u32.to_ne_bytes());
        buf.extend(r);
        let map = parse_udp_table(&buf);
        assert_eq!(map.get(&(0x0100007f, 53)), Some(&777));
    }

    #[test]
    fn mib_port_converts_network_order() {
        // 端口 443 = 0x01BB，网络序字节 01 BB → DWORD 低16位读作 0xBB01
        assert_eq!(mib_port(0xBB01), 443);
        assert_eq!(mib_port(0x5000), 80);
    }

    #[test]
    fn lookup_tcp_and_udp() {
        let mut pm = ProcMap::new();
        let mut tcp = HashMap::new();
        tcp.insert((1, 1000, 2, 80), 42);
        tcp.insert((2, 80, 1, 1000), 42);
        let mut udp = HashMap::new();
        udp.insert((3, 5353), 99);
        pm.seed_for_test(tcp, udp);

        assert_eq!(pm.lookup_pid(1, 1000, 2, 80, true, true), Some(42));
        assert_eq!(pm.lookup_pid(2, 80, 1, 1000, true, false), Some(42));
        assert_eq!(pm.lookup_pid(3, 5353, 7, 9, false, true), Some(99)); // 出站 UDP 看 src
        assert_eq!(pm.lookup_pid(7, 9, 3, 5353, false, false), Some(99)); // 入站 UDP 看 dst
        assert_eq!(pm.lookup_pid(8, 8, 9, 9, true, true), None);
    }

    #[test]
    fn truncated_table_is_safe() {
        let buf = vec![0u8; 9];
        assert!(parse_tcp_table(&buf).is_empty() || rd_u32(&buf, 0) as usize == 0);
        assert!(parse_udp_table(&[1, 0, 0, 0, 9, 9]).is_empty());
    }

    /// 真实 FFI 冒烟：GetExtendedTcpTable/UdpTable 不需要管理员权限，
    /// 用它验证表类常量、结构体偏移、动态加载签名都正确（否则上面的纯解析测试发现不了）。
    #[cfg(windows)]
    #[test]
    fn real_connection_tables_load() {
        let mut pm = ProcMap::new();
        pm.refresh_if_due(Instant::now());
        assert!(
            pm.last_error().is_none(),
            "读取系统连接表失败：{:?}",
            pm.last_error()
        );
        // 不要求一定有连接，但解析必须能跑通且 PID 合法
        for pid in pm.tcp.values().chain(pm.udp.values()) {
            assert!(*pid != 0);
        }
    }
}
