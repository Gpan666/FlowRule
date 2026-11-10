//! 进程级限速的数据模型（纯逻辑，无平台依赖）。
//!
//! 一个「程序」（同一个可执行文件）通常有多个进程：浏览器一个标签页一个进程，
//! 用户要限的是「这个程序」，不是其中某一个 pid。
//! 因此模型以**程序**为单位：用可执行文件路径作为分组键，限速规则携带该程序当前的
//! 所有 pid，每个方向一个令牌桶 —— 限速值就是整个程序的带宽预算。
//!
//! 早期版本会给每个程序拼 `processId == N` 的 WinDivert filter，但 WinDivert 2.2 的
//! NETWORK 层不支持该字段（见 rl-engine-win::proc_map 的说明），现已改为：
//! 引擎只开两个全局句柄，收包后用五元组 + 系统连接表在**用户态**判定归属程序。
//! 所以本模块只负责数据模型，不再构造任何过滤表达式。

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub path: Option<String>,
}

impl ProcessInfo {
    pub fn new(pid: u32, name: String, path: Option<String>) -> Self {
        Self { pid, name, path }
    }

    /// 列表里的显示名：同名进程附 pid 便于区分
    pub fn display(&self) -> String {
        format!("{} ({})", self.name, self.pid)
    }
}

/// 稳定的程序分组键：优先用可执行文件路径（不区分大小写），拿不到路径时退回进程名。
pub fn program_key(path: &Option<String>, name: &str) -> String {
    match path {
        Some(p) => p.to_lowercase(),
        None => format!("name::{}", name.to_lowercase()),
    }
}

/// 一个「程序」：同一个可执行文件，可能有多个进程。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramInfo {
    /// 分组键，见 [`program_key`]
    pub key: String,
    /// 显示名
    pub name: String,
    pub path: Option<String>,
    /// 当前活着的进程 id（升序去重）
    pub pids: Vec<u32>,
}

impl ProgramInfo {
    /// 把进程列表按程序分组。
    pub fn group(procs: &[ProcessInfo]) -> Vec<ProgramInfo> {
        let mut map: HashMap<String, (String, Option<String>, Vec<u32>)> = HashMap::new();
        for p in procs {
            let key = program_key(&p.path, &p.name);
            let e = map
                .entry(key)
                .or_insert_with(|| (p.name.clone(), p.path.clone(), Vec::new()));
            e.2.push(p.pid);
        }
        map.into_iter()
            .map(|(key, (name, path, mut pids))| {
                pids.sort_unstable();
                pids.dedup();
                ProgramInfo { key, name, path, pids }
            })
            .collect()
    }
}

/// 展示排序：有限速的排前面，其余按名字
pub fn sort_programs<F: Fn(&str) -> bool>(progs: &mut [ProgramInfo], is_limited: F) {
    progs.sort_by(|a, b| {
        is_limited(&b.key)
            .cmp(&is_limited(&a.key))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// 单个程序的限速规则：该程序的所有进程共享一个带宽预算（上下行各自一个）。
/// 上下行都为 0 表示不限。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramRule {
    pub key: String,
    /// 采样这一刻的进程集合；进程数变化会改变 filter，从而重开句柄
    pub pids: Vec<u32>,
    pub down_bps: u64,
    pub up_bps: u64,
}

impl ProgramRule {
    pub fn is_active(&self) -> bool {
        self.down_bps > 0 || self.up_bps > 0
    }

    /// 去重 + 升序：同样的进程集合得到同样的配置，便于 diff 和生成稳定的 filter
    pub fn normalized_pids(&self) -> Vec<u32> {
        let mut v = self.pids.clone();
        v.sort_unstable();
        v.dedup();
        v
    }
}

/// 完整限速配置：程序规则 + 「其他流量」（未被单独限速的所有进程）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LimitConfig {
    pub rules: Vec<ProgramRule>,
    pub other_down_bps: u64,
    pub other_up_bps: u64,
}

impl LimitConfig {
    pub fn unlimited() -> Self {
        Self::default()
    }

    /// 没有任何限速生效
    pub fn is_all_unlimited(&self) -> bool {
        self.other_down_bps == 0
            && self.other_up_bps == 0
            && self.rules.iter().all(|r| !r.is_active())
    }

    pub fn rule_for(&self, key: &str) -> Option<&ProgramRule> {
        self.rules.iter().find(|r| r.key == key)
    }

    /// 真正需要开句柄的程序（按 key 排序，保证每次生成同样的配置便于 diff）
    pub fn active_rules(&self) -> Vec<&ProgramRule> {
        let mut v: Vec<&ProgramRule> = self.rules.iter().filter(|r| r.is_active()).collect();
        v.sort_by(|a, b| a.key.cmp(&b.key));
        v
    }

    pub fn other_active(&self) -> bool {
        self.other_down_bps > 0 || self.other_up_bps > 0
    }
}

// ⚠️ 曾经这里有 build_filter / build_program_filter，会给每个程序拼
// `processId == N` 的 WinDivert filter。但 WinDivert 2.2 的 NETWORK 层根本不支持
// processId，这些表达式一律编译失败、WinDivertOpen 报 87。
// 现已整体移除：进程归属改由 rl-engine-win::proc_map 在用户态用五元组判定，
// 引擎只开两个不含任何进程条件的全局句柄。相关常量 FILTER_MAX_LEN/MAX_EXCLUDES
// 也随之废弃。回归防线见 tools/check_filter.py 与 rl-engine-win 的
// `base_filters_never_contain_process_id` 单测。

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_key_prefers_path_and_lowercases() {
        assert_eq!(
            program_key(&Some("C:\\App\\Chrome.exe".into()), "chrome.exe"),
            "c:\\app\\chrome.exe"
        );
        assert_eq!(program_key(&None, "Some.App"), "name::some.app");
    }

    #[test]
    fn group_merges_same_executable() {
        let procs = vec![
            ProcessInfo::new(1, "chrome.exe".into(), Some("C:\\chrome.exe".into())),
            ProcessInfo::new(2, "chrome.exe".into(), Some("C:\\chrome.exe".into())),
            ProcessInfo::new(3, "a.exe".into(), None),
        ];
        let mut groups = ProgramInfo::group(&procs);
        assert_eq!(groups.len(), 2);
        sort_programs(&mut groups, |_| false);
        let chrome = groups.iter().find(|g| g.name == "chrome.exe").unwrap();
        assert_eq!(chrome.pids, vec![1, 2]);
        let a = groups.iter().find(|g| g.name == "a.exe").unwrap();
        assert_eq!(a.key, "name::a.exe");
    }

    #[test]
    fn active_rules_sorted_by_key() {
        let mk = |key: &str, active: bool| ProgramRule {
            key: key.to_string(),
            pids: vec![1],
            down_bps: if active { 1024 } else { 0 },
            up_bps: 0,
        };
        let cfg = LimitConfig {
            rules: vec![mk("c.exe", true), mk("a.exe", false), mk("b.exe", true)],
            other_down_bps: 0,
            other_up_bps: 0,
        };
        let active = cfg.active_rules();
        assert_eq!(active.len(), 2);
        assert_eq!(active[0].key, "b.exe");
        assert_eq!(active[1].key, "c.exe");
        assert_eq!(cfg.active_rules()[0].normalized_pids(), vec![1]);
    }

    #[test]
    fn is_all_unlimited() {
        assert!( LimitConfig::unlimited().is_all_unlimited());
        let cfg = LimitConfig {
            rules: vec![ProgramRule { key: "a".into(), pids: vec![1], down_bps: 0, up_bps: 0 }],
            ..Default::default()
        };
        assert!(cfg.is_all_unlimited());
        assert!(!cfg.other_active());
    }
}
