//! 速率单位与换算。
//!
//! 全工程内部的速率真值唯一：**字节/秒（u64）**。
//! 界面层只在显示和输入时用 `Unit` 换算，避免出现「Kb 是比特还是字节」的 8 倍歧义。

/// 界面单位。Kb = KB/s，Mb = MB/s，均按字节计（1 KB = 1024 B）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Unit {
    #[default]
    Kb,
    Mb,
}

impl Unit {
    pub const ALL: [Unit; 2] = [Unit::Kb, Unit::Mb];

    pub fn label(self) -> &'static str {
        match self {
            Unit::Kb => "KB/s",
            Unit::Mb => "MB/s",
        }
    }

    /// 一个单位等于多少字节
    pub fn bytes(self) -> f64 {
        match self {
            Unit::Kb => 1024.0,
            Unit::Mb => 1024.0 * 1024.0,
        }
    }

    /// 输入单位对应的比特单位名（用于附带显示）
    pub fn bit_label(self) -> &'static str {
        match self {
            Unit::Kb => "Kbps",
            Unit::Mb => "Mbps",
        }
    }
}

/// 一个「数值 + 单位」的速率。value <= 0 表示不限速。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rate {
    pub value: f64,
    pub unit: Unit,
}

impl Rate {
    pub const UNLIMITED: Rate = Rate { value: 0.0, unit: Unit::Kb };

    pub fn new(value: f64, unit: Unit) -> Self {
        Self { value, unit }
    }

    pub fn is_unlimited(self) -> bool {
        !self.value.is_finite() || self.value <= 0.0
    }

    /// 转成内部真值；不限速返回 0
    pub fn bytes_per_sec(self) -> u64 {
        if self.is_unlimited() {
            return 0;
        }
        let raw = self.value * self.unit.bytes();
        if raw <= 0.0 {
            1
        } else if raw >= u64::MAX as f64 {
            u64::MAX
        } else {
            raw.round() as u64
        }
    }

    pub fn bits_per_sec(self) -> u64 {
        self.bytes_per_sec().saturating_mul(8)
    }

    /// 换算到指定单位下的数值
    pub fn as_unit(self, unit: Unit) -> f64 {
        self.bytes_per_sec() as f64 / unit.bytes()
    }

    /// 换算成 Mbps（界面附带显示用）
    pub fn mbps(self) -> f64 {
        self.bits_per_sec() as f64 / 1_000_000.0
    }

    /// 自动挑一个好看的单位：>= 1 MB/s 用 MB/s，否则 KB/s
    pub fn auto(bytes_per_sec: u64) -> Self {
        if bytes_per_sec >= 1024 * 1024 {
            Self::new(bytes_per_sec as f64 / (1024.0 * 1024.0), Unit::Mb)
        } else {
            Self::new(bytes_per_sec as f64 / 1024.0, Unit::Kb)
        }
    }

    pub fn display(self) -> String {
        if self.is_unlimited() {
            return "不限速".to_string();
        }
        format!("{} {}", trim(self.value), self.unit.label())
    }

    /// 完整口径说明： "1 MB/s = 1024 KB/s = 8.00 Mbps"
    pub fn explain(self) -> String {
        if self.is_unlimited() {
            return "不限速".to_string();
        }
        format!(
            "{} = {} KB/s = {:.2} Mbps",
            self.display(),
            trim(self.as_unit(Unit::Kb)),
            self.mbps()
        )
    }
}

/// 把字节/秒格式化成人类可读（用于实时速率显示）
pub fn format_bps(bytes_per_sec: u64) -> String {
    Rate::auto(bytes_per_sec).display()
}

fn trim(v: f64) -> String {
    if v >= 100.0 {
        format!("{:.0}", v)
    } else if v >= 1.0 {
        trim_zeros(format!("{:.2}", v))
    } else {
        trim_zeros(format!("{:.3}", v))
    }
}

fn trim_zeros(s: String) -> String {
    let s = s.trim_end_matches('0').to_string();
    s.trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mb_is_1024_kb() {
        assert_eq!(Rate::new(1.0, Unit::Mb).bytes_per_sec(), 1024 * 1024);
        assert_eq!(Rate::new(1024.0, Unit::Kb).bytes_per_sec(), 1024 * 1024);
    }

    #[test]
    fn eight_times_bits() {
        let r = Rate::new(1.0, Unit::Mb);
        assert_eq!(r.bits_per_sec(), 8 * 1024 * 1024);
        assert!((r.mbps() - 8.388608).abs() < 1e-6);
    }

    #[test]
    fn unit_conversion_roundtrip() {
        let r = Rate::new(512.0, Unit::Kb);
        assert!((r.as_unit(Unit::Mb) - 0.5).abs() < 1e-9);
        assert!((r.as_unit(Unit::Kb) - 512.0).abs() < 1e-9);
    }

    #[test]
    fn unlimited_is_zero() {
        assert!(Rate::UNLIMITED.is_unlimited());
        assert_eq!(Rate::UNLIMITED.bytes_per_sec(), 0);
        assert_eq!(Rate::new(0.0, Unit::Mb).bytes_per_sec(), 0);
    }

    #[test]
    fn auto_picks_unit() {
        assert_eq!(Rate::auto(2048 * 1024).unit, Unit::Mb);
        assert_eq!(Rate::auto(512 * 1024).unit, Unit::Kb);
    }

    #[test]
    fn format_bps_readable() {
        assert_eq!(format_bps(1024), "1 KB/s");
        assert_eq!(format_bps(1024 * 1024), "1 MB/s");
    }
}
