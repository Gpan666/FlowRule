//! 进程级限速的数据模型与过滤规则构造。
//!
//! 一个"程序"（同一个可执行文件）通常有多个进程：浏览器一个标签页一个进程，
//! 用户要限的是「这个程序」，不是其中某一个 pid。
//! 因此模型以**程序**为单位：用可执行文件路径作为分组键，限速规则携带该程序当前的
//! 所有 pid，引擎用 `(processId == A or processId == B ...)` 把它们合成一个句柄、共用
//! 一个令牌桶 —— 限速值就是整个程序的带宽预算。
//!
//! 关键约束：**多个句柄的 filter 不能重叠**，否则同一个包会被多个句柄各重注入一次。
//! 因此「其他流量」的 filter 必须**逐方向**排除掉「该方向已被单独限速的程序」覆盖的 pid。

use crate::Direction;
use std::collections::HashMap;

/// WinDivert filter 表达式的实测上限（保守取 1024）
pub const FILTER_MAX_LEN: usize = 1024;
/// 「其他流量」最多能排除多少个进程（超出则表达式会超长）
pub const MAX_EXCLUDES: usize = 24;

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

/// 构造「其他流量」的 filter：`base and processId != p1 and processId != p2 ...`
pub fn build_filter(dir: Direction, pid: Option<u32>, exclude: &[u32]) -> Result<String, String> {
    let _ = pid; // 保留参数以兼容旧签名；pid 维度已交给 build_program_filter
    let mut f = dir.base_filter().to_string();

    for p in exclude.iter().take(MAX_EXCLUDES) {
        f.push_str(&format!(" and processId != {}", p));
    }
    if exclude.len() > MAX_EXCLUDES {
        return Err(format!(
            "单独限速的进程超过 {} 个，无法再对「其他流量」限速。请减少程序规则数量。",
            MAX_EXCLUDES
        ));
    }

    if f.len() > FILTER_MAX_LEN {
        return Err(format!("filter 表达式过长（{} > {}）", f.len(), FILTER_MAX_LEN));
    }
    Ok(f)
}

/// 把一个程序的多个进程合成一个 filter：
/// `base and (processId == A or processId == B ...)`
///
/// 由于 filter 长度有上限，会按给定顺序（调用方应为升序）尽量多覆盖，
/// 返回**实际覆盖**的 pid 列表 —— 调用方必须把这份列表同步排除到「其他流量」的
/// filter 里，否则会出现句柄重叠、重复注入。
pub fn build_program_filter(dir: Direction, pids: &[u32]) -> Result<(String, Vec<u32>), String> {
    let mut f = dir.base_filter().to_string();
    f.push_str(" and (");

    let mut covered: Vec<u32> = Vec::new();
    for p in pids {
        let piece = if covered.is_empty() {
            format!("processId == {}", p)
        } else {
            format!(" or processId == {}", p)
        };
        // 预留一个字符给收尾的 ')'
        if f.len() + piece.len() + 1 > FILTER_MAX_LEN {
            break;
        }
        f.push_str(&piece);
        covered.push(*p);
    }
    f.push(')');

    if covered.is_empty() {
        return Err(format!(
            "程序的进程数过多，{} 方向无法构造过滤器",
            dir.label()
        ));
    }
    if f.len() > FILTER_MAX_LEN {
        return Err(format!("filter 表达式过长（{} > {}）", f.len(), FILTER_MAX_LEN));
    }
    Ok((f, covered))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_process_filter_contains_pid() {
        let (f, cov) = build_program_filter(Direction::Down, &[4242]).unwrap();
        assert!(f.starts_with("inbound"));
        assert!(f.contains("processId == 4242"));
        assert_eq!(cov, vec![4242]);
    }

    #[test]
    fn program_filter_unions_pids() {
        let (f, cov) = build_program_filter(Direction::Up, &[10, 20, 30]).unwrap();
        assert!(f.starts_with("outbound"));
        assert!(f.contains("processId == 10 and".to_string().as_str()) || f.contains("processId == 10 or"));
        assert!(f.contains("processId == 20"));
        assert!(f.contains("processId == 30"));
        assert_eq!(cov, vec![10, 20, 30]);
    }

    #[test]
    fn program_filter_covered_within_limit() {
        // 7 位 pid 的最坏情况也要控制在 filter 上限内，且覆盖数尽量多
        let many: Vec<u32> = (0..200).map(|i| 9_000_000 - i * 7).collect();
        let (f, cov) = build_program_filter(Direction::Down, &many).unwrap();
        assert!(f.len() <= FILTER_MAX_LEN);
        assert!(!cov.is_empty());
        // 覆盖的一定是传入列表的前缀
        assert_eq!(cov, many[..cov.len()].to_vec());
    }

    #[test]
    fn program_filter_empty_is_error() {
        assert!(build_program_filter(Direction::Down, &[]).is_err());
    }

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
    fn other_filter_excludes_pids() {
        let f = build_filter(Direction::Up, None, &[10, 20]).unwrap();
        assert!(f.starts_with("outbound"));
        assert!(f.contains("processId != 10"));
        assert!(f.contains("processId != 20"));
        assert!(!f.contains("processId =="));
    }

    #[test]
    fn too_many_excludes_is_error() {
        let many: Vec<u32> = (0..MAX_EXCLUDES as u32 + 1).collect();
        assert!(build_filter(Direction::Down, None, &many).is_err());
    }

    #[test]
    fn filter_length_within_limit() {
        let many: Vec<u32> = (0..MAX_EXCLUDES as u32).collect();
        let f = build_filter(Direction::Down, None, &many).unwrap();
        assert!(f.len() <= FILTER_MAX_LEN);
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
