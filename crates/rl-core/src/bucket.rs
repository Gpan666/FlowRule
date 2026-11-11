//! 令牌桶（**真实丢包式**，不做排队整形）。
//!
//! rate_bps == 0 表示不限速（所有包直接放行）。
//!
//! ## 两条与「限速到底有没有效果」直接相关的硬约束
//!
//! 1. **额度必须够一次 TCP 突发的整串**。接收窗口内的数据是成串到达网卡的，
//!    内核会一口气甩出十几个 1500 字节的包。桶里只有「速率 × 100ms」时，
//!    慢速挡位（32 KB/s 才 3.2 KB）连一串突发都装不下，后面**每一个**包都被丢，
//!    丢包率飙到 50% 以上，TCP 反复超时重传 —— 用户看到的是「网页打不开 /
//!    下载卡死」，而不是「稳在设定的速度」。
//!    因此额度下限取 16 个 MTU，且上限再叠加一次突发的整串。
//! 2. **按时间连续补桶，不按窗口边界截断**。只在每个 100ms 窗口的边界结算时，
//!    窗口末尾的零头会被丢弃，实测速率长期比设定值低一截。
//!    这里按「流逝时间 × 速率」线性进账（单次最多算 EPOCH_SECS，防止空闲攒出突发）。

use std::time::{Duration, Instant};

/// 桶容量时间窗：100ms 突发
pub const BURST_WINDOW: Duration = Duration::from_millis(100);
/// 一个以太网 MTU（含 IP/TCP 头）
pub const MTU_BYTES: u64 = 1500;
/// 额度下限：16 个 MTU ≈ 24 KB。
///
/// 低于一个 MTU 时，**任何速率下的第一个整包都过不去**（表现为「一开限速网络就断」）；
/// 只按「速率 × 100ms」算时，128 KB/s 也就 12.8 KB，一个接收窗口的突发就能打空，
/// 于是慢速挡位丢包严重、TCP 卡死。16 个 MTU 只相当于把突发窗放宽到几十毫秒，
/// 对平均速率没有影响。
pub const MIN_CAPACITY: u64 = MTU_BYTES * 16;

/// refill 时最多补 EPOCH_SECS 的令牌（防止长时间空闲后一次性补满形成巨量突发）
const EPOCH_SECS: f64 = 0.1;

#[derive(Debug)]
pub struct TokenBucket {
    rate_bps: u64,
    capacity: u64,
    tokens: f64,
    last: Instant,
    /// 突发额度：一次 TCP 突发的整串（16 个 MTU，且不超过 50ms 的流量）
    burst: u64,
}

impl TokenBucket {
    /// 以字节/秒建桶，0 表示不限速
    pub fn new(rate_bps: u64) -> Self {
        let mut b = Self {
            rate_bps: 0,
            capacity: 0,
            tokens: 0.0,
            last: Instant::now(),
            burst: 0,
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

    /// 令牌上限 = 一次突发额度的整串 + 一个时间片进账。
    ///
    /// 只按速率算上限的话，慢速挡位（32 KB/s 一个时间片才 3.2 KB）连一串突发都装不下，
    /// 丢包会失控；上限太大又会把后续几秒的额度提前透支出去。
    fn bucket_ceiling(&self) -> f64 {
        self.burst as f64 + self.capacity as f64
    }

    /// 热更新速率：切挡位时调用，立即生效
    pub fn set_rate(&mut self, rate_bps: u64) {
        self.rate_bps = rate_bps;
        self.capacity = if rate_bps == 0 {
            0
        } else {
            (rate_bps as f64 * BURST_WINDOW.as_secs_f64()).ceil().max(MIN_CAPACITY as f64) as u64
        };
        // 突发额度 = 一个时间片的量，但不低于一次 TCP 突发的整串（16 个 MTU），
        // 也不超过 50ms 的流量（大速率下别让突发太大）。
        let window_bytes = (rate_bps as f64 * BURST_WINDOW.as_secs_f64()).round() as u64;
        self.burst = window_bytes
            .max(MIN_CAPACITY)
            .min((rate_bps / 20).max(MIN_CAPACITY));
        // 新桶给满，避免切换瞬间卡死
        self.tokens = self.burst as f64;
        self.last = Instant::now();
    }

    /// 突发额度（不含一个时间片进账）
    pub fn burst(&self) -> u64 {
        self.burst
    }

    /// 测试专用：在「虚拟时间轴」上判定一个包能否放行
    #[cfg(test)]
    fn try_consume_at(&mut self, now: Instant, bytes: u64) -> bool {
        if self.is_unlimited() {
            return true;
        }
        self.refill_at(now);
        self.consume_tokens(bytes)
    }

    fn refill(&mut self) {
        let now = Instant::now();
        self.refill_at(now);
    }

    /// 以 `now` 作为当前时刻补桶（生产代码走 [`Self::refill`]，测试直接喂时间点）。
    ///
    /// 规则只有两条，但都是「限速到底准不准」的关键：
    ///
    /// 1. **连续补桶**：额度 = 流逝时间 × 速率，按时间线性进账，
    ///    不被「窗口边界」截断（截断会让每个窗口的零头凭空消失，实测速率长期偏低）。
    /// 2. **只看最近一个时间片**：一次流逝超过 EPOCH_SECS 就当只过了 EPOCH_SECS，
    ///    长时间空闲不会攒出巨量突发。令牌上限 = 一次突发的整串 + 一个片段的额度。
    fn refill_at(&mut self, now: Instant) {
        if self.is_unlimited() {
            return;
        }
        let raw_dt = now.saturating_duration_since(self.last).as_secs_f64();
        if raw_dt <= 0.0 {
            return;
        }
        self.last = now;
        let dt = raw_dt.min(EPOCH_SECS);
        let ceiling = self.bucket_ceiling();
        self.tokens = (self.tokens + self.rate_bps as f64 * dt).min(ceiling);
    }

    /// 判定一个长度为 bytes 的包能否放行，能则扣减令牌
    pub fn try_consume(&mut self, bytes: u64) -> bool {
        if self.is_unlimited() {
            return true;
        }
        self.refill();
        self.consume_tokens(bytes)
    }

    #[inline]
    fn consume_tokens(&mut self, bytes: u64) -> bool {
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
        let mut b = TokenBucket::new(1024);
        let cap = b.capacity() as u64;
        assert!(b.try_consume(cap));
        assert!(!b.try_consume(1000), "桶打空后连一个小包都不能再放行");
    }

    #[test]
    fn refills_over_time() {
        let mut b = TokenBucket::new(1_024_000);
        let burst = b.burst() as u64; // 开桶即满（= 一串 TCP 突发的量）
        assert!(b.try_consume(burst));
        assert!(!b.try_consume(burst), "突发额度用完后必须先被挡住");
        // 时间往前走一个片段，补进来的额度要能立刻用上
        let mut clock = Instant::now();
        clock += Duration::from_millis(100);
        assert!(b.try_consume_at(clock, 100_000));
        clock += Duration::from_millis(100);
        assert!(b.try_consume_at(clock, 100_000));
    }

    #[test]
    fn set_rate_takes_effect_immediately() {
        let mut b = TokenBucket::new(1024);
        assert!(!b.try_consume(500_000));
        b.set_rate(10_000);
        assert!(b.try_consume(1000));
        b.set_rate(0);
        assert!(b.try_consume(1_000_000));
    }

    /// ★ 回归：小速率下第一个整包必须能通过。
    /// 桶容量小于 MTU 时，任何包都过不去 → 表现为「限速一开，网络直接断」。
    #[test]
    fn first_full_mtu_packet_passes_at_any_rate() {
        for rate in [1u64, 1024, 8 * 1024, 32 * 1024, 1024 * 1024] {
            let mut b = TokenBucket::new(rate);
            assert!(b.capacity() >= MTU_BYTES, "rate={rate} 桶容量不足一个 MTU");
            assert!(b.try_consume(MTU_BYTES), "rate={rate} 首包必须放行");
        }
    }

    /// ★ 回归：桶空闲很久后**不能**把空闲期间的令牌一次性补满。
    ///
    /// 32 KB/s 的桶闲 10 秒，能攒出的额度上限只能是「一次突发的整串 + 一个窗口进账」，
    /// 而不是 10 × 32 KB = 320 KB。
    #[test]
    fn idle_gap_does_not_burst_full_idle_allowance() {
        let rate = 32 * 1024u64;
        let mut b = TokenBucket::new(rate);
        let mut clock = Instant::now();
        clock += Duration::from_secs(10);
        // 空闲结束后只放一个小包，把 refill 触发一次
        assert!(b.try_consume_at(clock, 1));
        let ceiling_one_burst = (MIN_CAPACITY + rate / 10) as f64;
        assert!(
            b.tokens <= ceiling_one_burst,
            "空闲 10 秒不该攒出 {} 字节的额度",
            b.tokens
        );
    }

    /// ★ 回归：TCP 突发的**整串**包必须能放行，否则慢速挡位下丢包率飙升、TCP 卡死。
    #[test]
    fn tcp_burst_batch_passes_then_throttles() {
        let mut b = TokenBucket::new(128 * 1024);
        for _ in 0..16 {
            assert!(b.try_consume(MTU_BYTES), "16 个连续满包应整串放行");
        }
        assert!(!b.try_consume(MTU_BYTES), "突发额度用完后下一个包必须被挡");
    }

    #[test]
    fn wait_time_zero_when_tokens_enough() {
        let mut b = TokenBucket::new(10_000);
        let _ = b.try_consume(1000);
        assert_eq!(b.wait_time(100), Some(Duration::ZERO));
        assert!(b.wait_time(9_000_000).unwrap() > Duration::ZERO);
    }

    /// ★ 回归：**实测速率必须贴着设定值**。
    ///
    /// 供给是配额的数倍时，每秒实际放行的字节必须落在设定值的 ±20% 以内。
    /// 这条把住「限速到底准不准」：偏低 → 用户觉得限得太狠 / 卡；
    /// 偏高 → 用户觉得限速根本没生效。
    #[test]
    fn throughput_tracks_configured_rate() {
        for rate in [32 * 1024u64, 128 * 1024, 1024 * 1024] {
            let mut b = TokenBucket::new(rate);
            // 虚拟时间轴：从当前时刻起推进，不依赖真实 sleep
            let mut clock = Instant::now();
            let step = Duration::from_micros(500); // 每 0.5ms 到一个包（供给是配额的数倍）
            let mut worst = 0f64;
            // 预热 2 秒：让桶从「开桶即满」进入稳态
            for _ in 0..4000 {
                clock += step;
                let _ = b.try_consume_at(clock, 500);
            }
            for _ in 0..4 {
                let mut passed = 0u64;
                for _ in 0..2000 {
                    clock += step;
                    // 供给是「混合大小的段」：真实下载不会清一色 1500 字节满包
                    if b.try_consume_at(clock, 500) {
                        passed += 500;
                    }
                }
                let err = (passed as f64 - rate as f64).abs() / rate as f64;
                worst = worst.max(err);
            }
            assert!(worst < 0.20, "rate={rate} 每秒偏差 {:.1}% 超出 ±20%", worst * 100.0);
        }
    }

    /// 桶容量必须有界：不能攒出远超一个突发窗口的额度
    #[test]
    fn capacity_stays_bounded() {
        let b = TokenBucket::new(128 * 1024);
        assert!(b.capacity() <= 128 * 1024 / 4, "容量 {} 超过 250ms 的量", b.capacity());
        assert!(b.capacity() >= MIN_CAPACITY);
    }
}
