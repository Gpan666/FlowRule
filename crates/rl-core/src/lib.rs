//! rl-core：限速核心逻辑，纯 Rust、无平台依赖、可独立单元测试。

pub mod bucket;
pub mod process;
pub mod profile;
pub mod stats;
pub mod unit;

pub use bucket::TokenBucket;
pub use process::{
    build_filter, build_program_filter, program_key, LimitConfig, ProcessInfo, ProgramInfo,
    ProgramRule,
};
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

    /// 不带进程条件的基础表达式，供 build_filter 继续拼
    pub fn base_filter(self) -> &'static str {
        match self {
            Direction::Down => "inbound and ip and (tcp or udp) and not loopback",
            Direction::Up => "outbound and ip and (tcp or udp) and not loopback",
        }
    }

    /// 全局（不限进程）的过滤表达式
    pub fn filter(self) -> &'static str {
        self.base_filter()
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
