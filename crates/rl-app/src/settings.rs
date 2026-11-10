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
    pub const VERSION: u32 = 1;

    pub fn load() -> Self {
        if let Some(path) = settings_path() {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(s) = serde_json::from_str::<Settings>(&text) {
                    if s.version == Self::VERSION {
                        return s;
                    }
                }
            }
        }
        Settings::default()
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
