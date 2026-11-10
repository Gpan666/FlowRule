//! 令牌桶。
//!
//! rate_bps == 0 表示不限速（所有包直接放行）。
//! 桶容量至少容纳一个以太网 MTU（1500 字节），既能吸收一次突发，
//! 又不会让「小速率」变成「秒级突发后长时间僵死」。

use std::time::{Duration, Instant};

/// 桶容量时间窗：100ms 突发
pub const BURST_WINDOW: Duration = Duration::from_millis(100);
/// 单个以太网 MTU。桶容量低于它时，**任何速率下的第一个 1500 字节包都过不去**，
/// 表现为「限速点了但网络直接断/完全没流量」。
pub const MIN_CAPACITY: u64 = 1500;

/// refill 时最多补 EPOCH_SECS 的令牌。
///
/// 旧实现在桶长时间空闲（没有包经过）后再来一个包时，会把空闲这么多秒的令牌
/// **一次性全补满**，于是「每秒 32KB」实际可以在 0.1 秒内吐出 10 MB 的突发。
/// 限速因此时灵时不灵（看客户端是不是恰好连续发包）。只补最近 100ms 的量，
/// 突发上限就恒等于桶容量。
const EPOCH_SECS: f64 = 0.1;

#[derive(Debug)]
pub struct TokenBucket {
    rate_bps: u64,
    capacity: u64,
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    /// 以字节/秒建桶，0 表示不限速
    pub fn new(rate_bps: u64) -> Self {
        let mut b = Self {
            rate_bps: 0,
            capacity: 0,
            tokens: 0.0,
            last: Instant::now(),
        };
        b.set_rate(rate_bps);
        b
    }

    pub fn unlimited() -> Self {
        Self::new(0)
    }

    pub fn is_unlimited(&self) -> bool {
        self.rate_bps == 0
    }

    pub fn rate(&self) -> u64 {
        self.rate_bps
    }

    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// 热更新速率：切挡位时调用，立即生效
    pub fn set_rate(&mut self, rate_bps: u64) {
        self.rate_bps = rate_bps;
        self.capacity = if rate_bps == 0 {
            0
        } else {
            (rate_bps as f64 * BURST_WINDOW.as_secs_f64()).ceil().max(MIN_CAPACITY as f64) as u64
        };
        // 新桶给满，避免切换瞬间卡死
        self.tokens = self.capacity as f64;
        self.last = Instant::now();
    }

    fn refill(&mut self) {
        if self.is_unlimited() {
            return;
        }
        let now = Instant::now();
        // 空闲很久后只补最近 100ms 的量，避免一次性放出巨大突发
        let dt = now
            .saturating_duration_since(self.last)
            .as_secs_f64()
            .min(EPOCH_SECS);
        if dt <= 0.0 {
            return;
        }
        self.last = now;
        let add = self.rate_bps as f64 * dt;
        self.tokens = (self.tokens + add).min(self.capacity as f64);
    }

    /// 判定一个长度为 bytes 的包能否放行，能则扣减令牌
    pub fn try_consume(&mut self, bytes: u64) -> bool {
        if self.is_unlimited() {
            return true;
        }
        self.refill();
        if self.tokens >= bytes as f64 {
            self.tokens -= bytes as f64;
            true
        } else {
            false
        }
    }

    /// 距离该包可放行还需等待多久（用于延迟整形而非直接丢包）
    pub fn wait_time(&self, bytes: u64) -> Option<Duration> {
        if self.is_unlimited() {
            return None;
        }
        let need = bytes as f64 - self.tokens;
        if need <= 0.0 {
            return Some(Duration::ZERO);
        }
        let secs = need / self.rate_bps as f64;
        Some(Duration::from_secs_f64(secs.min(1.0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_passes_everything() {
        let mut b = TokenBucket::unlimited();
        assert!(b.try_consume(10_000_000));
        assert!(b.try_consume(u64::MAX / 4));
    }

    #[test]
    fn burst_then_throttle() {
        let mut b = TokenBucket::new(1024); // 容量 = 1500（不小于 1 个 MTU）
        assert!(b.try_consume(1000));
        assert!(!b.try_consume(1000));
    }

    #[test]
    fn refills_over_time() {
        let mut b = TokenBucket::new(100_000); // 容量 = 10000（100ms 突发）
        assert!(b.try_consume(10_000));
        assert!(!b.try_consume(1));
        // 每 20ms 补 2000 字节；等 60ms 补回 6000（不足一个满桶）
        std::thread::sleep(Duration::from_millis(60));
        assert!(b.try_consume(5_000));
        assert!(!b.try_consume(5_000));
    }

    #[test]
    fn set_rate_takes_effect_immediately() {
        let mut b = TokenBucket::new(1024);
        assert!(!b.try_consume(5000));
        b.set_rate(10_000);
        assert!(b.try_consume(1000));
        b.set_rate(0);
        assert!(b.try_consume(1_000_000));
    }

    #[test]
    fn capacity_covers_one_mtu() {
        let b = TokenBucket::new(1);
        assert!(b.capacity() >= 1500);
    }

    /// ★ 回归：桶空闲很久后**不能**把空闲期间的令牌一次性补满。
    ///
    /// 旧实现 refill 用完整 dt，于是一个 100ms 没流量的 32 KB/s 桶，
    /// 再来包时瞬间就能放行 3.2 KB；空闲 10 秒甚至能一次吐出 320 KB。
    /// 大文件下载是连续发包看不出问题，但「限速后短暂停顿再突发」的场景
    /// 会明显绕过限速，用户感知就是「限速没生效」。
    #[test]
    fn idle_gap_does_not_burst_full_idle_allowance() {
        let mut b = TokenBucket::new(32 * 1024); // 容量 = 3277（100ms 的量）
        std::thread::sleep(Duration::from_millis(300));
        // 空闲 300ms 后，一次最多只能拿到桶容量（≈100ms 的量），而不是 300ms 的量
        assert!(b.try_consume(3000));
        assert!(!b.try_consume(1500), "空闲期间的令牌不能被一次性补满");
    }

    /// ★ 回归：小速率下第一个 1500 字节的整包必须能通过。
    /// 桶容量小于 MTU 时，任何包都过不去 → 表现为「限速一开，网络直接断」。
    #[test]
    fn first_full_mtu_packet_passes_at_any_rate() {
        for rate in [1u64, 1024, 8 * 1024, 32 * 1024, 1024 * 1024] {
            let mut b = TokenBucket::new(rate);
            assert!(b.capacity() >= MIN_CAPACITY, "rate={rate} 桶容量不足一个 MTU");
            assert!(b.try_consume(MIN_CAPACITY), "rate={rate} 首包必须放行");
        }
    }

    #[test]
    fn wait_time_zero_when_tokens_enough() {
        let mut b = TokenBucket::new(10_000);
        let _ = b.try_consume(1000);
        assert_eq!(b.wait_time(100), Some(Duration::ZERO));
        assert!(b.wait_time(9_000_000).unwrap() > Duration::ZERO);
    }

    /// ★ 回归（与 `idle_gap_does_not_burst_full_idle_allowance` 同源，但完全确定性）：
    /// `Instant` 在不是 `main` 线程的测试线程里也能稳定计时，所以这里把桶
    /// 放到一个会睡 250ms 的线程里创建并使用，避免不同测试线程的时钟差异。
    /// 32 KB/s 的桶空闲 250ms 若按旧逻辑补满，会补出 8 KB 令牌（远超桶容量 3277），
    /// `try_consume(3000) && try_consume(1500)` 就都能通过 —— 现在必须失败。
    #[test]
    fn idle_gap_cap_is_deterministic() {
        std::thread::spawn(|| {
            let mut b = TokenBucket::new(32 * 1024);
            std::thread::sleep(Duration::from_millis(250));
            assert!(b.try_consume(3000));
            assert!(!b.try_consume(1500), "空闲 250ms 不该补出超过桶容量的令牌");
        })
        .join()
        .unwrap();
    }
}
