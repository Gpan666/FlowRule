//! rl-core：限速核心逻辑，纯 Rust、无平台依赖、可独立单元测试。

pub mod bucket;
pub mod process;
pub mod profile;
pub mod stats;
pub mod unit;

pub use bucket::TokenBucket;
pub use process::{program_key, LimitConfig, ProcessInfo, ProgramInfo, ProgramRule};
pub use stats::{ByteCounter, History, RateSampler, HISTORY_POINTS, SAMPLE_INTERVAL};
pub use unit::{format_bps, Rate, Unit};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Direction {
    /// 下行：收到的流量
    Down,
    /// 上行：发出的流量
    Up,
}

impl Direction {
    pub const ALL: [Direction; 2] = [Direction::Down, Direction::Up];

    pub fn label(self) -> &'static str {
        match self {
            Direction::Down => "下行",
            Direction::Up => "上行",
        }
    }

    /// 全局句柄使用的过滤表达式（NETWORK 层，WinDivert 2.2 已离线编译校验通过）。
    ///
    /// ⚠️ NETWORK 层**不支持 `processId` 字段**，绝不能在这里拼进程条件；
    /// 进程归属改由引擎在用户态用五元组 + 系统连接表判定（见 rl-engine-win::proc_map）。
    ///
    /// `not impostor` 用于排除 WinDivertSend 重注入的包，避免自己捕获自己形成死循环。
    ///
    /// ## 为什么**绝对不能在 filter 里按网卡过滤**
    ///
    /// 历史上有两次「限速没效果」，根因都是往 filter 里塞了网卡条件：
    ///
    /// 1. `not loopback`（更早版本）→ Windows 上的 VPN / 代理（Clash、v2rayN、WireGuard…）
    ///    都在本机中转，一份下载会在软件回环口和物理网卡各出现一次，被限两次，
    ///    实测速率正好是设定值的一半。
    /// 2. `not ifIdx != 1`（上一版，等价于「只要物理网卡、丢掉软件回环口」）→
    ///    **直连且开了 VPN 的机器（TUN 模式）上，进程真正的流量走的是虚拟网卡**，
    ///    filter 把它全丢掉，句柄只看到隧道外层那一点点加密流量，外层又归属不到进程，
    ///    于是「收包数很小、命中限速 0、程序识别 0、丢弃 0，下载却跑满带宽」——
    ///    用户设 5 MB/s 完全不生效。
    ///
    /// 结论：**每个包在哪张网卡上只限一次**这件事必须在用户态判断（引擎按 IP 去重，
    /// 见 rl-engine-win 的 `dedupe_key`），绝不能靠 filter 猜网卡 ——
    /// filter 一旦猜错，流量就是「完全不经过我们」，而不是「限多限少」。
    /// 所以这里只按方向 / 协议过滤，句柄必须看到本机全部 TCP/UDP 流量。
    pub fn base_filter(self) -> &'static str {
        match self {
            Direction::Down => "inbound and (ip or ipv6) and (tcp or udp) and not impostor",
            Direction::Up => "outbound and (ip or ipv6) and (tcp or udp) and not impostor",
        }
    }
}

/// 停止信号
pub type StopFlag = Arc<AtomicBool>;

pub fn stop_flag() -> StopFlag {
    Arc::new(AtomicBool::new(false))
}

pub fn is_stopped(flag: &StopFlag) -> bool {
    flag.load(Ordering::Relaxed)
}

/// 引擎抽象。Windows 走 WinDivert，将来可换成 Wintun + smoltcp，GUI 不用改。
///
/// 单线程 reactor：引擎全部状态都在引擎线程内，不需要加锁；
/// GUI 通过 channel 把 `LimitConfig` 送进去。
pub trait Engine: Send {
    /// 热更新限速配置，立即生效
    fn apply(&mut self, config: LimitConfig) -> Result<(), String>;
    /// 处理至多 max_packets 个包后返回，非阻塞语义靠引擎内部超时实现
    fn poll(&mut self, max_packets: usize) -> Result<usize, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_base_filters() {
        assert!(Direction::Down.base_filter().starts_with("inbound"));
        assert!(Direction::Up.base_filter().starts_with("outbound"));
    }

    /// ★ 回归：句柄必须看到全部 TCP/UDP 流量，filter 里**不许出现任何网卡条件**。
    ///
    /// 只要按网卡猜一次，猜错的方向就是「流量根本不进句柄」——
    /// 表现为收包数极小、命中限速 0、丢弃 0，用户设多少都像没限。
    #[test]
    fn base_filter_never_filters_by_interface() {
        for d in Direction::ALL {
            let f = d.base_filter();
            assert!(!f.contains("ifIdx"), "{} filter 不得按网卡过滤：{f}", d.label());
            assert!(!f.contains("loopback"), "{} filter 不得按回环过滤：{f}", d.label());
            assert!(!f.contains("outbound") || f.starts_with("outbound"), "{}", d.label());
        }
    }
}
