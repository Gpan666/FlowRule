//! 令牌桶。
//!
//! rate_bps == 0 表示不限速（所有包直接放行）。
//! 桶容量 = 速率 × 100ms，允许短突发，避免小流量场景被过度压制。

use std::time::{Duration, Instant};

/// 桶容量时间窗：100ms 突发
pub const BURST_WINDOW: Duration = Duration::from_millis(100);

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
            (rate_bps as f64 * BURST_WINDOW.as_secs_f64()).ceil().max(1500.0) as u64
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
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
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
        let mut b = TokenBucket::new(1024); // 容量 = 103（不小于 1 个 MTU 1500）
        assert!(b.try_consume(1000));
        assert!(!b.try_consume(1000));
    }

    #[test]
    fn refills_over_time() {
        let mut b = TokenBucket::new(100_000); // 容量 = 10000（100ms 突发）
        assert!(b.try_consume(10_000));
        assert!(!b.try_consume(1));
        std::thread::sleep(Duration::from_millis(150)); // 补约 15000，封顶 10000
        assert!(b.try_consume(10_000));
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

    #[test]
    fn wait_time_zero_when_tokens_enough() {
        let mut b = TokenBucket::new(10_000);
        let _ = b.try_consume(1000);
        assert_eq!(b.wait_time(100), Some(Duration::ZERO));
        assert!(b.wait_time(9_000_000).unwrap() > Duration::ZERO);
    }
}
