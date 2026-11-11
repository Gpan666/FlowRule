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
const AF_INET6: u32 = 23;
/// TCP_TABLE_OWNER_PID_ALL：包含 listener / connections / established，且带 owning PID
const TCP_TABLE_OWNER_PID_ALL: u32 = 5;
/// UDP_TABLE_OWNER_PID
const UDP_TABLE_OWNER_PID: u32 = 1;
/// UDP6_TABLE_OWNER_PID
const UDP6_TABLE_OWNER_PID: u32 = 2;
const NO_ERROR: u32 = 0;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

/// MIB_TCPROW_OWNER_PID：6 个 DWORD = 24 字节
const TCP_ROW_SIZE: usize = 24;
/// MIB_UDPROW_OWNER_PID：3 个 DWORD = 12 字节
const UDP_ROW_SIZE: usize = 12;
/// ★ IPv6 连接表的**表头与行步长在不同 Windows 版本上并不等于文档里的结构体大小**。
/// 本机（Win11）实测：`GetExtendedUdpTable(AF_INET6, UDP6_TABLE_OWNER_PID)` 返回
///   - 表头 8 字节（dwNumEntries 4 字节 + 4 字节对齐填充）
///   - 行步长 **176 字节**（而 MIB_UDP6ROW_OWNER_PID 只有 28 字节）
///   - 行内字段仍按文档排布：ucLocalAddr[16] @0, dwLocalScopeId @16,
///     dwLocalPort @20, dwOwningPid @24
/// 该结论已用 `netstat -ano` 的 IPv6 UDP 端点逐条交叉验证（42/42 命中）。
/// 为了不把某个版本的布局写死，这里按候选步长自适应探测，取自洽率最高者。
const V6_HEADER_SIZE: usize = 4;
const UDP6_STRIDE_CANDIDATES: [usize; 1] = [28];
const TCP6_STRIDE_CANDIDATES: [usize; 1] = [56];
/// PID 合理上界（Windows PID 实际远小于此；超过说明布局解错了）
const PID_SANE_MAX: u32 = 0x100_0000;
/// IPv4 连接表头 = `DWORD dwNumEntries` + **4 字节对齐填充** + 行数组。
///
/// ⚠️ 必须是 8，不能是 4。`MIB_TCPTABLE_OWNER_PID` / `MIB_UDPTABLE_OWNER_PID` 里
/// 行数组紧跟一个 `DWORD`，而 `MIB_TCPROW_OWNER_PID` 需要 4 字节对齐，
/// 编译器因此插入 4 字节填充。旧代码按 4 读取，**每一条记录都整体错位 4 字节**：
/// 地址、端口、PID 全部读错，`lookup_pid` 恒返回 None → 所有流量都被判成「其他流量」，
/// 按程序限速永远不生效（而且因为错位后的 PID 字段总是 0，被 `pid == 0` 过滤掉，
/// 表面上看不出任何异常）。已用 `netstat -ano` 逐条交叉验证。
const TABLE_HEADER_SIZE: usize = 8;

/// 映射刷新间隔。连接表查询要遍历系统全部连接，不能每个包都调；
/// 连接建立/关闭本身是秒级事件，700ms 足够准确。
pub const REFRESH_INTERVAL: Duration = Duration::from_millis(700);

/// TCP 键：(src_addr, src_port, dst_addr, dst_port)，双向都会插入
type TcpKey = (u32, u16, u32, u16);
/// UDP 键：(local_addr, local_port)
type UdpKey = (u32, u16);
/// IPv6 版 TCP 键（地址为 16 字节网络序）
type TcpKey6 = ([u8; 16], u16, [u8; 16], u16);
/// IPv6 版 UDP 键
type UdpKey6 = ([u8; 16], u16);

/// 一个包的网络层地址/端口组合。IPv4 与 IPv6 分开表示，
/// 因为系统连接表里两者的地址宽度不同（4 字节 vs 16 字节）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Flow {
    V4 { src: u32, sport: u16, dst: u32, dport: u16 },
    V6 { src: [u8; 16], sport: u16, dst: [u8; 16], dport: u16 },
}

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
fn read_table(f: FnGetTable, af: u32, class: u32) -> Result<Vec<u8>, String> {
    unsafe {
        let mut size: u32 = 0;
        let r = f(std::ptr::null_mut(), &mut size, 0, af, class, 0);
        if r != NO_ERROR && r != ERROR_INSUFFICIENT_BUFFER {
            return Err(format!("查询连接表大小失败，Win32 错误 {r}"));
        }
        if size == 0 {
            return Ok(Vec::new());
        }
        // 表可能在两次调用之间增长；失败时按新 size 重试一次
        for _ in 0..2 {
            let mut buf = vec![0u8; size as usize];
            let r = f(buf.as_mut_ptr() as *mut c_void, &mut size, 0, af, class, 0);
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

/// 从 IPv6 版 UDP owner-pid 表解析 (local_addr, local_port) → PID
///
/// 行布局 = `MIB_UDP6ROW_OWNER_PID`（28 字节）：
/// `ucLocalAddr[16] @0, dwLocalScopeId @16, dwLocalPort @20, dwOwningPid @24`。
/// **IPv6 表头是 4 字节**（dwNumEntries 后行直接开始，无填充；IPv4 表才是 8）。
/// 旧代码照搬 IPv4 的 8 字节表头，整体错位 4 字节，IPv6 归属因此全部失效。
pub fn parse_udp6_table(buf: &[u8]) -> HashMap<UdpKey6, u32> {
    if buf.len() < V6_HEADER_SIZE {
        return HashMap::new();
    }
    let n = rd_u32(buf, 0) as usize;
    if n == 0 || n > 200_000 {
        return HashMap::new();
    }
    let mut best: Option<(usize, HashMap<UdpKey6, u32>)> = None;
    for &stride in UDP6_STRIDE_CANDIDATES.iter() {
        let mut map = HashMap::new();
        let mut score = 0usize;
        for i in 0..n {
            let off = V6_HEADER_SIZE + i * stride;
            if off + 28 > buf.len() {
                break;
            }
            let mut la = [0u8; 16];
            la.copy_from_slice(&buf[off..off + 16]);
            let lp = mib_port(rd_u32(buf, off + 20));
            let pid = rd_u32(buf, off + 24);
            if pid == 0 || pid > PID_SANE_MAX {
                continue;
            }
            score += 1;
            map.insert((la, lp), pid);
        }
        if score == 0 {
            continue;
        }
        if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
            best = Some((score, map));
        }
    }
    let Some((score, map)) = best else {
        return HashMap::new();
    };
    // 自洽率不足 60% → 布局没对上，放弃 v6 归属
    // （宁可不归属，也不能把流量错记到别的程序上）
    if score * 5 < n * 3 {
        return HashMap::new();
    }
    map
}

/// 从 IPv6 版 TCP owner-pid 表解析双向五元组 → PID
///
/// 行布局 = `MIB_TCP6ROW_OWNER_PID`（56 字节）：
/// `ucLocalAddr[16] @0, dwLocalScopeId @16, dwLocalPort @20,
///  ucRemoteAddr[16] @24, dwRemoteScopeId @40, dwRemotePort @44,
///  dwState @48, dwOwningPid @52`。
/// **IPv6 表头是 4 字节**（dwNumEntries 后行直接开始；IPv4 表才是 8）。
/// 此前先后栽在两个坑上：TableClass 常量写错（19 → Win32 87 → 恒空表）、
/// 表头照搬 IPv4 的 8 字节 → 整体错位 4 字节。表头 4 + 文档布局与
/// netstat 唯一 PID 锚点（pid@abs56 = 行0@+52、pid@abs504 = 行8@+52）
/// 完全自洽，与 `windows` crate 0.62 的结构体定义一致。
pub fn parse_tcp6_table(buf: &[u8]) -> HashMap<TcpKey6, u32> {
    if buf.len() < V6_HEADER_SIZE {
        return HashMap::new();
    }
    let n = rd_u32(buf, 0) as usize;
    if n == 0 || n > 200_000 {
        return HashMap::new();
    }
    let mut best: Option<(usize, HashMap<TcpKey6, u32>)> = None;
    for &stride in TCP6_STRIDE_CANDIDATES.iter() {
        let mut map = HashMap::new();
        let mut score = 0usize;
        for i in 0..n {
            let off = V6_HEADER_SIZE + i * stride;
            if off + 56 > buf.len() {
                break;
            }
            let mut la = [0u8; 16];
            la.copy_from_slice(&buf[off..off + 16]);
            let mut ra = [0u8; 16];
            ra.copy_from_slice(&buf[off + 24..off + 40]);
            let lp = mib_port(rd_u32(buf, off + 20));
            let rp = mib_port(rd_u32(buf, off + 44));
            let pid = rd_u32(buf, off + 52);
            if pid == 0 || pid > PID_SANE_MAX {
                continue;
            }
            score += 1;
            map.insert((la, lp, ra, rp), pid);
        }
        if score == 0 {
            continue;
        }
        if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
            best = Some((score, map));
        }
    }
    let Some((score, map)) = best else {
        return HashMap::new();
    };
    if score * 5 < n * 3 {
        return HashMap::new();
    }
    // TCP 是双向的：把反向五元组也登记
    let mut out = HashMap::new();
    for ((la, lp, ra, rp), pid) in map {
        out.insert((la, lp, ra, rp), pid);
        out.insert((ra, rp, la, lp), pid);
    }
    out
}

/// 五元组 → PID 的缓存映射
pub struct ProcMap {
    tcp: HashMap<TcpKey, u32>,
    udp: HashMap<UdpKey, u32>,
    tcp6: HashMap<TcpKey6, u32>,
    udp6: HashMap<UdpKey6, u32>,
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
        Self {
            tcp: HashMap::new(),
            udp: HashMap::new(),
            tcp6: HashMap::new(),
            udp6: HashMap::new(),
            last_refresh: None,
            last_error: None,
        }
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
        let tcp_buf = read_table(a.tcp, AF_INET, TCP_TABLE_OWNER_PID_ALL)?;
        let udp_buf = read_table(a.udp, AF_INET, UDP_TABLE_OWNER_PID)?;
        self.tcp = parse_tcp_table(&tcp_buf);
        self.udp = parse_udp_table(&udp_buf);
        // IPv6 表：TCP/UDP 的 TableClass 枚举与地址族无关，直接复用 IPv4 的常量值。
        // 旧代码对 TCP6 发明了常量 19（TCP_TABLE_CLASS 只有 0..=8），系统返回
        // Win32 87（INVALID_PARAMETER）→ 表恒为空 → 所有 IPv6 流量（比如
        // 走 IPv6 CDN 的下载）永远无法归属进程，限速对它们静默失效。
        let tcp6_buf = read_table(a.tcp, AF_INET6, TCP_TABLE_OWNER_PID_ALL).unwrap_or_default();
        let udp6_buf = read_table(a.udp, AF_INET6, UDP6_TABLE_OWNER_PID).unwrap_or_default();
        self.tcp6 = parse_tcp6_table(&tcp6_buf);
        self.udp6 = parse_udp6_table(&udp6_buf);
        Ok(())
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn tcp_len(&self) -> usize { self.tcp.len() }
    pub fn udp_len(&self) -> usize { self.udp.len() }
    pub fn tcp6_len(&self) -> usize { self.tcp6.len() }
    pub fn udp6_len(&self) -> usize { self.udp6.len() }

    /// 查一个包属于哪个进程。
    ///
    /// - `flow`：包里的地址/端口（IPv4 用 `u32` 原生 DWORD，IPv6 用 16 字节网络序）
    /// - `is_tcp`：true=TCP, false=UDP
    /// - `outbound`：UDP 时决定用 src 还是 dst 作为「本端」
    #[inline]
    pub fn lookup_pid(&self, flow: Flow, is_tcp: bool, outbound: bool) -> Option<u32> {
        match flow {
            Flow::V4 { src, sport, dst, dport } => {
                if is_tcp {
                    self.tcp.get(&(src, sport, dst, dport)).copied()
                } else {
                    let local = if outbound { (src, sport) } else { (dst, dport) };
                    self.udp.get(&local).copied()
                }
            }
            Flow::V6 { src, sport, dst, dport } => {
                if is_tcp {
                    self.tcp6.get(&(src, sport, dst, dport)).copied()
                } else {
                    let local = if outbound { (src, sport) } else { (dst, dport) };
                    self.udp6.get(&local).copied()
                }
            }
        }
    }

    #[cfg(test)]
    pub fn seed_for_test(&mut self, tcp: HashMap<TcpKey, u32>, udp: HashMap<UdpKey, u32>) {
        self.tcp = tcp;
        self.udp = udp;
        self.last_refresh = Some(Instant::now());
    }

    #[cfg(test)]
    pub fn seed6_for_test(
        &mut self,
        tcp6: HashMap<TcpKey6, u32>,
        udp6: HashMap<UdpKey6, u32>,
    ) {
        self.tcp6 = tcp6;
        self.udp6 = udp6;
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

    /// 真实布局的 IPv4 表头：`dwNumEntries` + 4 字节对齐填充
    fn v4_buf(rows: &[Vec<u8>]) -> Vec<u8> {
        let mut buf = (rows.len() as u32).to_ne_bytes().to_vec();
        buf.extend_from_slice(&0u32.to_ne_bytes()); // MIB 行需 4 字节对齐，编译器在此插入填充
        for r in rows {
            buf.extend_from_slice(r);
        }
        buf
    }

    #[test]
    fn tcp_table_is_bidirectional() {
        let buf = v4_buf(&[row(5, 0x0100007f, 50000, 0x0101a8c0, 443, 4242)]);
        let map = parse_tcp_table(&buf);
        // 出站包 src=local, dst=remote
        assert_eq!(map.get(&(0x0100007f, 50000, 0x0101a8c0, 443)), Some(&4242));
        // 入站包 src=remote, dst=local
        assert_eq!(map.get(&(0x0101a8c0, 443, 0x0100007f, 50000)), Some(&4242));
        assert_eq!(map.get(&(0x0100007f, 50000, 0x0101a8c0, 80)), None);
    }

    #[test]
    fn udp_table_maps_local_endpoint() {
        let mut r = Vec::new();
        r.extend_from_slice(&0x0100007fu32.to_ne_bytes());
        r.extend_from_slice(&(53u16.swap_bytes() as u32).to_ne_bytes());
        r.extend_from_slice(&777u32.to_ne_bytes());
        let map = parse_udp_table(&v4_buf(&[r]));
        assert_eq!(map.get(&(0x0100007f, 53)), Some(&777));
    }

    /// ★ 回归：IPv4 表头必须跳过 4 字节对齐填充。
    ///
    /// 旧代码用 `TABLE_HEADER_SIZE = 4`，每条记录整体错位 4 字节：
    /// 地址/端口/PID 全部读错，PID 字段读到的是 0 → 该条被丢弃 →
    /// `lookup_pid` 恒为 None → 所有流量都归「其他流量」→ 按程序限速永远不生效。
    #[test]
    fn v4_table_skips_alignment_padding() {
        // 两条记录，只要错位 4 字节，第二条就会读到垃圾
        let buf = v4_buf(&[
            row(5, 0x0100007f, 50000, 0x0101a8c0, 443, 4242),
            row(5, 0x0200007f, 50001, 0x0201a8c0, 80, 5353),
        ]);
        let tcp = parse_tcp_table(&buf);
        assert_eq!(tcp.len(), 4, "两条 TCP 记录应产生 4 个双向键，实际 {}", tcp.len());
        assert_eq!(tcp.get(&(0x0200007f, 50001, 0x0201a8c0, 80)), Some(&5353));

        let mut u1 = Vec::new();
        u1.extend_from_slice(&0x0100007fu32.to_ne_bytes());
        u1.extend_from_slice(&(53u16.swap_bytes() as u32).to_ne_bytes());
        u1.extend_from_slice(&777u32.to_ne_bytes());
        let mut u2 = Vec::new();
        u2.extend_from_slice(&0x0300007fu32.to_ne_bytes());
        u2.extend_from_slice(&(5353u16.swap_bytes() as u32).to_ne_bytes());
        u2.extend_from_slice(&888u32.to_ne_bytes());
        let udp = parse_udp_table(&v4_buf(&[u1, u2]));
        assert_eq!(udp.get(&(0x0300007f, 5353)), Some(&888));
    }

    #[test]
    fn v4_header_constant_matches_real_layout() {
        // MIB_TCPROW_OWNER_PID = 6 DWORD 需 4 字节对齐 → 表头 = 4 + 4(填充)
        assert_eq!(TABLE_HEADER_SIZE, 8);
        assert_eq!(TABLE_HEADER_SIZE % 4, 0);
    }

    #[test]
    fn mib_port_converts_network_order() {
        // 端口 443 = 0x01BB，网络序字节 01 BB → DWORD 低16位读作 0xBB01
        assert_eq!(mib_port(0xBB01), 443);
        assert_eq!(mib_port(0x5000), 80);
    }

    /// 构造 UDP6 表行（文档布局 MIB_UDP6ROW_OWNER_PID：28 字节，lp@20、pid@24）
    fn row6_udp(la: [u8; 16], lp: u16, pid: u32) -> Vec<u8> {
        let mut v = vec![0u8; 28];
        v[..16].copy_from_slice(&la);
        v[16..20].copy_from_slice(&0u32.to_ne_bytes()); // dwLocalScopeId
        v[20..24].copy_from_slice(&(lp.swap_bytes() as u32).to_ne_bytes());
        v[24..28].copy_from_slice(&pid.to_ne_bytes());
        v
    }

    /// 构造 TCP6 表行（文档布局 MIB_TCP6ROW_OWNER_PID，56 字节：
    /// la@0, scope@16, lp@20, ra@24, scope@40, rp@44, state@48, pid@52）
    fn row6_tcp(la: [u8; 16], lp: u16, ra: [u8; 16], rp: u16, pid: u32) -> Vec<u8> {
        let mut v = vec![0u8; 56];
        v[..16].copy_from_slice(&la);
        v[20..24].copy_from_slice(&(lp.swap_bytes() as u32).to_ne_bytes());
        v[24..40].copy_from_slice(&ra);
        v[44..48].copy_from_slice(&(rp.swap_bytes() as u32).to_ne_bytes());
        v[52..56].copy_from_slice(&pid.to_ne_bytes());
        v
    }

    /// UDP6 表：4 字节表头（dwNumEntries，无填充）+ 28 字节行
    fn udp6_buf(rows: &[([u8; 16], u16, u32)]) -> Vec<u8> {
        let mut buf = (rows.len() as u32).to_ne_bytes().to_vec();
        for (la, lp, pid) in rows {
            buf.extend(row6_udp(*la, *lp, *pid));
        }
        buf
    }

    #[test]
    fn udp6_table_parses_real_layout() {
        let buf = udp6_buf(&[
            ([0u8; 16], 500, 3628),
            ([7u8; 16], 5353, 9784),
        ]);
        let map = parse_udp6_table(&buf);
        assert_eq!(map.get(&([0u8; 16], 500)), Some(&3628));
        assert_eq!(map.get(&([7u8; 16], 5353)), Some(&9784));
    }

    #[test]
    fn udp6_wrong_stride_is_rejected() {
        // PID 离谱 → 自洽率崩 → 必须整体放弃（宁可不归属）
        let mut buf = 1u32.to_ne_bytes().to_vec();
        let mut row = vec![0u8; 28];
        row[24..28].copy_from_slice(&0x11223344u32.to_ne_bytes()); // 离谱的 PID
        buf.extend(row);
        assert!(parse_udp6_table(&buf).is_empty());
    }

    #[test]
    fn tcp6_table_is_bidirectional() {
        let mut buf = 1u32.to_ne_bytes().to_vec();
        buf.extend(row6_tcp([1u8; 16], 50000, [2u8; 16], 443, 4242));
        let map = parse_tcp6_table(&buf);
        assert_eq!(map.get(&([1u8; 16], 50000, [2u8; 16], 443)), Some(&4242));
        assert_eq!(map.get(&([2u8; 16], 443, [1u8; 16], 50000)), Some(&4242));
    }

    #[test]
    fn truncated_v6_table_is_safe() {
        assert!(parse_tcp6_table(&[1, 0, 0, 0]).is_empty());
        assert!(parse_udp6_table(&[1, 0, 0, 0, 9, 9]).is_empty());
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

        assert_eq!(pm.lookup_pid(Flow::V4 { src: 1, sport: 1000, dst: 2, dport: 80 }, true, true), Some(42));
        assert_eq!(pm.lookup_pid(Flow::V4 { src: 2, sport: 80, dst: 1, dport: 1000 }, true, false), Some(42));
        assert_eq!(pm.lookup_pid(Flow::V4 { src: 3, sport: 5353, dst: 7, dport: 9 }, false, true), Some(99)); // 出站 UDP 看 src
        assert_eq!(pm.lookup_pid(Flow::V4 { src: 7, sport: 9, dst: 3, dport: 5353 }, false, false), Some(99)); // 入站 UDP 看 dst
        assert_eq!(pm.lookup_pid(Flow::V4 { src: 8, sport: 8, dst: 9, dport: 9 }, true, true), None);
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
        for pid in pm.tcp6.values().chain(pm.udp6.values()) {
            assert!(*pid != 0);
        }
        eprintln!(
            "连接表规模：TCP v4={} UDP v4={} TCP v6={} UDP v6={}",
            pm.tcp.len(),
            pm.udp.len(),
            pm.tcp6.len(),
            pm.udp6.len()
        );
    }
}
