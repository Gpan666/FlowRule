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
    /// ⚠️ **必须是 `not ifIdx != 1`，不能是 `not loopback`。**
    /// WinDivert 的 `loopback` 是「两端都是 127.0.0.1 / ::1」的应用层回环包；
    /// 而 Windows 上几乎所有 VPN / 代理（Clash、v2rayN、WireGuard…）都在本机中转：
    /// 流量经 Wintun / TAP 虚拟网卡进来，再在物理网卡出去 —— 这两段的中转包
    /// 一端是 127.0.0.1 或虚拟网卡地址、另一端是正常地址，`loopback` 判定为 false，
    /// 于是一份下载在 `ifIdx` 1→N、N→1 上各被我们限了一次，
    /// **用户设 1 MB/s 实测卡在 500 KB/s 左右，看起来像「限速不准 / 不生效」**。
    /// `ifIdx == 1` 是 Windows 的软件回环接口（本地回环路径），跳过它即「每个包只限一次」。
    pub fn base_filter(self) -> &'static str {
        match self {
            Direction::Down => "inbound and (ip or ipv6) and (tcp or udp) and not ifIdx != 1 and not impostor",
            Direction::Up => "outbound and (ip or ipv6) and (tcp or udp) and not ifIdx != 1 and not impostor",
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
}
