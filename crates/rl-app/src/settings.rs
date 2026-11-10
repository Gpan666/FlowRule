//! 设置持久化：把用户改过的配置（单位 / 总开关 / 规则 / 选中程序）存到 exe 同目录的
//! `config/settings.json`，下次启动自动恢复。这样「单位切换」「限速规则」不会一重启就没了。
//!
//! 规则按程序 key（可执行文件完整路径）保存，进程重启后只要程序还在，规则会自动重新生效
//! （后端 `current_config` 每轮都按当前活着的 pid 重建句柄）。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct SavedDir {
    pub enabled: bool,
    pub sel: String, // "p0".."p8" 或 "custom"
    pub custom: String,
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct SavedRule {
    pub down: SavedDir,
    pub up: SavedDir,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Settings {
    pub version: u32,
    pub unit: String, // "kb" / "mb"
    pub master: bool,
    pub other: SavedRule,
    pub rules: Vec<(String, SavedRule)>,
    pub selected: Option<String>,
}

impl Settings {
    pub const VERSION: u32 = 4;

    pub fn load() -> Self {
        if let Some(path) = settings_path() {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(mut s) = serde_json::from_str::<Settings>(&text) {
                    let migrated = s.version < Self::VERSION;
                    if migrated {
                        Self::migrate(&mut s);
                        // 立刻把清理后的配置落盘，避免下次启动又读到旧的「其他流量」限速
                        s.save();
                        return s;
                    }
                    if s.version == Self::VERSION {
                        return s;
                    }
                }
            }
        }
        Settings::default()
    }

    /// 把旧版本遗留的配置升级到当前版本。
    ///
    /// # 关键修复（v4）
    /// 旧版（v1/v2/v3）有个「点总开关 / 单限程序时自动给『其他流量』套 1 MB/s 默认挡位」
    /// 的逻辑，它把**未识别归属的流量**（含 IPv6、连接表刷新窗口、本机其它程序）全压到 1 MB/s，
    /// 表现就是「单限某程序却整台电脑被限」「设 20MB 实测只有 1MB」——用户报的 bug。
    ///
    /// 旧迁移（`rules.is_empty()` 守卫）漏掉了「既有程序规则、又有遗留『其他流量』」这种真实场景，
    /// 于是这个 1 MB 默认挡位被原样存成 v3 保留下来。
    ///
    /// 新规则：**任何 < v4 的配置，只要『其他流量』处于启用状态且没有用户输入过自定义速率
    /// （即没有 `custom` 文本），就一律视为遗留自动默认并清除**。这样：
    /// - 遗留自动默认（enabled + 默认挡位 + 无 custom）→ 清除 ✓
    /// - 用户手动点挡位但没打字（gear 选择）→ 也清除（2 秒可重设，可接受）
    /// - 用户**真的输入过自定义速率**（custom 非空）→ 视为明确意图，保留 ✓
    /// - 已经手动开启过『其他流量』做全局限速的用户，重设一次即可，不会整台被偷偷限。
    fn migrate(s: &mut Settings) {
        if s.version < Self::VERSION {
            let has_custom = !s.other.down.custom.is_empty() || !s.other.up.custom.is_empty();
            if (s.other.down.enabled || s.other.up.enabled) && !has_custom {
                s.other = SavedRule::default();
            }
            s.version = Self::VERSION;
        }
    }

    pub fn save(&self) {
        if let Some(path) = settings_path() {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(json) = serde_json::to_string_pretty(self) {
                let _ = std::fs::write(&path, json);
            }
        }
    }
}

/// exe 同目录下的 `config/settings.json`（便携化：随包走，不写系统盘）
pub fn settings_path() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.join("config").join("settings.json")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_other(down_p: &str, up_p: &str, rules: usize) -> Settings {
        let mut s = Settings {
            version: 2,
            unit: "kb".into(),
            master: true,
            other: SavedRule {
                down: SavedDir { enabled: true, sel: down_p.into(), custom: String::new() },
                up: SavedDir { enabled: !up_p.is_empty(), sel: up_p.into(), custom: String::new() },
            },
            rules: vec![],
            selected: None,
        };
        for i in 0..rules {
            s.rules.push((format!("prog{i}"), SavedRule::default()));
        }
        s
    }

    #[test]
    fn migrate_clears_legacy_other_even_with_program_rules() {
        // 用户真实场景：旧版自动给「其他流量」套了 1MB，后来又加了程序规则。
        // 旧迁移（rules.is_empty 守卫）漏掉这种情况，本测试锁定新行为：必须清除。
        let mut s = legacy_other("p5", "", 1);
        Settings::migrate(&mut s);
        assert_eq!(s.version, Settings::VERSION);
        assert!(!s.other.down.enabled);
        assert!(!s.other.up.enabled);
        assert_eq!(s.rules.len(), 1); // 程序规则原样保留
    }

    #[test]
    fn migrate_clears_legacy_other_without_rules() {
        let mut s = legacy_other("p5", "", 0);
        Settings::migrate(&mut s);
        assert!(!s.other.down.enabled);
        assert!(!s.other.up.enabled);
    }

    #[test]
    fn migrate_preserves_user_typed_custom_other() {
        // 用户真的输入过自定义速率（custom 非空）→ 视为明确意图，保留
        let mut s = legacy_other("custom", "", 0);
        s.other.down.custom = "20".into();
        Settings::migrate(&mut s);
        assert_eq!(s.version, Settings::VERSION);
        assert!(s.other.down.enabled);
        assert_eq!(s.other.down.custom, "20");
    }
}
