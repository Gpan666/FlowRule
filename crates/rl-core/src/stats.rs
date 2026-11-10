//! 速率统计：引擎线程只做原子累加，GUI 线程定时采样。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 100ms 采样一次
pub const SAMPLE_INTERVAL: Duration = Duration::from_millis(100);
/// 保留 600 点 ≈ 60 秒曲线
pub const HISTORY_POINTS: usize = 600;

/// 字节累加器，跨线程共享
#[derive(Debug, Default)]
pub struct ByteCounter(AtomicU64);

impl ByteCounter {
    pub fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    /// 取出并清零，得到采样窗口内的字节数
    pub fn take(&self) -> u64 {
        self.0.swap(0, Ordering::Relaxed)
    }

    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }
}

/// 把累加器换算成瞬时速率（字节/秒）
pub struct RateSampler {
    counter: Arc<ByteCounter>,
    last: Instant,
}

impl RateSampler {
    pub fn new(counter: Arc<ByteCounter>) -> Self {
        Self { counter, last: Instant::now() }
    }

    /// 返回字节/秒；调用间隔过短时返回上一次窗口的折算值
    pub fn sample(&mut self) -> u64 {
        let now = Instant::now();
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        let bytes = self.counter.take();
        if dt <= 0.0 {
            return 0;
        }
        self.last = now;
        (bytes as f64 / dt).round() as u64
    }
}

/// 定长环形缓冲，存速率历史（KB/s）
#[derive(Debug)]
pub struct History {
    data: VecDeque<f32>,
    cap: usize,
}

impl History {
    pub fn new(cap: usize) -> Self {
        Self { data: VecDeque::with_capacity(cap), cap }
    }

    pub fn push(&mut self, v: f32) {
        if self.data.len() == self.cap {
            self.data.pop_front();
        }
        self.data.push_back(v);
    }

    pub fn as_slice(&self) -> impl Iterator<Item = &f32> {
        self.data.iter()
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn latest(&self) -> f32 {
        self.data.back().copied().unwrap_or(0.0)
    }

    pub fn max(&self) -> f32 {
        self.data.iter().copied().fold(0.0f32, f32::max)
    }
}

impl Default for History {
    fn default() -> Self {
        Self::new(HISTORY_POINTS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_take_resets() {
        let c = ByteCounter::new();
        c.add(100);
        c.add(50);
        assert_eq!(c.take(), 150);
        assert_eq!(c.take(), 0);
    }

    #[test]
    fn history_keeps_last_n() {
        let mut h = History::new(3);
        for i in 0..10 {
            h.push(i as f32);
        }
        assert_eq!(h.len(), 3);
        assert_eq!(h.latest(), 9.0);
        assert_eq!(h.max(), 9.0);
    }

    #[test]
    fn sampler_returns_rate() {
        let c = ByteCounter::new().shared();
        let mut s = RateSampler::new(c.clone());
        c.add(8192);
        std::thread::sleep(Duration::from_millis(100));
        let rate = s.sample();
        assert!(rate > 40_000 && rate < 200_000, "rate = {}", rate);
        assert_eq!(s.sample(), 0);
    }
}
