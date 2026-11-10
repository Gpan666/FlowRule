//! 内置挡位。一组数据两种展示：按当前单位换算显示，切换单位不需要两套表。

use crate::unit::{Rate, Unit};

const KB: u64 = 1024;
const MB: u64 = 1024 * KB;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    pub id: u8,
    pub name: &'static str,
    /// 0 表示不限速
    pub bytes_per_sec: u64,
}

pub const PROFILES: &[Profile] = &[
    Profile { id: 0, name: "极慢", bytes_per_sec: 32 * KB },
    Profile { id: 1, name: "慢速", bytes_per_sec: 64 * KB },
    Profile { id: 2, name: "标准", bytes_per_sec: 128 * KB },
    Profile { id: 3, name: "快", bytes_per_sec: 256 * KB },
    Profile { id: 4, name: "很快", bytes_per_sec: 512 * KB },
    Profile { id: 5, name: "1 兆", bytes_per_sec: MB },
    Profile { id: 6, name: "2 兆", bytes_per_sec: 2 * MB },
    Profile { id: 7, name: "5 兆", bytes_per_sec: 5 * MB },
    Profile { id: 8, name: "不限", bytes_per_sec: 0 },
];

pub fn all() -> &'static [Profile] {
    PROFILES
}

pub fn by_id(id: u8) -> Option<&'static Profile> {
    PROFILES.iter().find(|p| p.id == id)
}

impl Profile {
    /// 在当前单位下的展示数值
    pub fn value_in(self, unit: Unit) -> f64 {
        self.bytes_per_sec as f64 / unit.bytes()
    }

    /// 按钮文字，例如 "512" 或 "1"（MB/s 下的 1 兆）
    pub fn short_label(self, unit: Unit) -> String {
        if self.bytes_per_sec == 0 {
            return "不限".to_string();
        }
        let v = self.value_in(unit);
        if v >= 1.0 {
            format!("{}", v.round() as u64)
        } else {
            format!("{:.2}", v)
        }
    }

    pub fn rate(self, unit: Unit) -> Rate {
        Rate::new(self.value_in(unit), unit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nine_profiles_in_order() {
        assert_eq!(PROFILES.len(), 9);
        for pair in PROFILES.windows(2) {
            if pair[1].bytes_per_sec != 0 {
                assert!(pair[1].bytes_per_sec > pair[0].bytes_per_sec);
            }
        }
    }

    #[test]
    fn labels_in_kb_and_mb() {
        let p = by_id(5).unwrap();
        assert_eq!(p.short_label(Unit::Kb), "1024");
        assert_eq!(p.short_label(Unit::Mb), "1");
        assert_eq!(by_id(8).unwrap().short_label(Unit::Mb), "不限");
    }
}
