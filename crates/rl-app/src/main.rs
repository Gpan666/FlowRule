//! 流律 GUI（Tauri 2 后端 + Miuix 风格前端）
//!
//! 架构：Rust 侧保留 `rl-core` / `rl-engine-win` 的全部限速逻辑，改为单一后台线程
//! （引擎 poll + 3 秒进程扫描 + 100ms 速率采样），前端通过 `get_snapshot` 拉取状态、
//! 通过 `set_*` 命令下发意图。前端只负责渲染，不持有任何业务状态。
//!
//! 与原 egui 版本相比，限速模型（按程序分组、句柄不重叠、其他流量兜底）完全一致。

// release 走 Windows GUI 子系统（双击不弹控制台），debug 保留 console 便于排错
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager, State};

mod icons;
mod settings;

/// 当前进程是否已提权（UAC 过滤令牌下返回 false，完全提权返回 true）
#[cfg(windows)]
fn is_elevated() -> bool {
    #[link(name = "shell32")]
    extern "system" {
        fn IsUserAnAdmin() -> i32;
    }
    unsafe { IsUserAnAdmin() != 0 }
}

#[cfg(not(windows))]
fn is_elevated() -> bool {
    true
}

use rl_core::stats::{History, RateSampler, SAMPLE_INTERVAL};
use rl_core::{
    format_bps, profile, program_key, Direction, Engine, LimitConfig, ProcessInfo, ProgramInfo,
    ProgramRule, Rate, Unit,
};
use rl_engine_win::{StatKey, StatsMap, WinDivertEngine};

/// 引擎一轮最多处理多少个包
const POLL_BATCH: usize = 256;
/// 进程列表刷新间隔
const SCAN_INTERVAL: Duration = Duration::from_secs(3);
/// 发给前端的曲线点数（内部 600 点降采样）
const GRAPH_POINTS: usize = 120;

// ────────────────────────────── 规则状态（沿用原 GUI 模型） ──────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sel {
    Profile(u8),
    Custom,
}

#[derive(Clone)]
struct DirState {
    enabled: bool,
    sel: Sel,
    custom: String,
}

impl DirState {
    fn new() -> Self {
        Self { enabled: false, sel: Sel::Profile(5), custom: "1".to_string() }
    }

    /// 目标速率（字节/秒），0 表示不限
    fn bytes_per_sec(&self, unit: Unit) -> u64 {
        if !self.enabled {
            return 0;
        }
        match self.sel {
            Sel::Profile(id) => profile::by_id(id).map(|p| p.bytes_per_sec).unwrap_or(0),
            Sel::Custom => Rate::new(parse_num(&self.custom), unit).bytes_per_sec(),
        }
    }

    fn short(&self, unit: Unit) -> String {
        let bps = self.bytes_per_sec(unit);
        if bps == 0 {
            "不限速".to_string()
        } else {
            Rate::auto(bps).display()
        }
    }

    fn dto(&self, unit: Unit) -> DirDto {
        let bytes = self.bytes_per_sec(unit);
        let explain = if bytes == 0 {
            "不限速：该方向不接管流量".to_string()
        } else {
            Rate::new(bytes as f64 / unit.bytes(), unit).explain()
        };
        DirDto {
            enabled: self.enabled,
            sel: match self.sel {
                Sel::Profile(id) => format!("p{id}"),
                Sel::Custom => "custom".to_string(),
            },
            custom: self.custom.clone(),
            short: self.short(unit),
            explain,
            bytes,
        }
    }
}

#[derive(Clone)]
struct RuleState {
    down: DirState,
    up: DirState,
}

impl RuleState {
    fn new() -> Self {
        Self { down: DirState::new(), up: DirState::new() }
    }
}

// ────────────────────────────── 共享状态 ──────────────────────────────

struct Inner {
    /// 提权状态在进程生命周期内不变，启动时探测一次即可，
    /// 避免每帧快照都调 Win32（也避免持锁做系统调用）
    elevated: bool,
    unit: Unit,
    master: bool,
    other: RuleState,
    rules: HashMap<String, RuleState>,
    /// 程序 key -> 图标 PNG base64（None = 确认取不到图标）
    icon_cache: HashMap<String, Option<String>>,
    /// 程序 key -> (显示名, 路径)，进程退出后仍能显示
    meta: HashMap<String, (String, Option<String>)>,
    programs: Vec<ProgramInfo>,
    scanning: bool,
    refresh_requested: bool,
    selected: Option<String>,

    hist_down: History,
    hist_up: History,

    stats: Option<StatsMap>,
    samplers: HashMap<StatKey, RateSampler>,
    rates: HashMap<StatKey, u64>,
    last_sample: Instant,

    status: String,
    warning: Option<String>,
    /// 引擎实时诊断，供界面显示「点了有没有真的接管流量」
    engine_diag: String,
}

type Shared = Arc<Mutex<Inner>>;

/// 带超时的加锁：最多等 200ms。std::sync::Mutex 没有超时 API，
/// 用 try_lock 轮询实现。命令和后台线程统一走这里，任何一方都不允许
/// 无限期持锁或无限期等待，避免整窗冻结。拿不到锁就跳过本轮（下一轮重试）。
fn lock_mutex(m: &Mutex<Inner>) -> Option<MutexGuard<'_, Inner>> {
    for _ in 0..20 {
        match m.try_lock() {
            Ok(g) => return Some(g),
            Err(std::sync::TryLockError::WouldBlock) => thread::sleep(Duration::from_millis(10)),
            Err(std::sync::TryLockError::Poisoned(e)) => return Some(e.into_inner()),
        }
    }
    None
}

impl Inner {
    fn new() -> Self {
        Self {
            elevated: is_elevated(),
            unit: Unit::Kb,
            master: false,
            other: RuleState::new(),
            rules: HashMap::new(),
            icon_cache: HashMap::new(),
            meta: HashMap::new(),
            programs: Vec::new(),
            scanning: true,
            refresh_requested: false,
            selected: None,
            hist_down: History::default(),
            hist_up: History::default(),
            stats: None,
            samplers: HashMap::new(),
            rates: HashMap::new(),
            last_sample: Instant::now(),
            status: "正在启动引擎…".to_string(),
            warning: None,
            engine_diag: String::new(),
        }
    }

    /// 当前生效的完整配置：把「程序规则」展开成带 pid 快照的 ProgramRule 下发
    fn current_config(&self) -> LimitConfig {
        if !self.master {
            return LimitConfig::unlimited();
        }
        let mut rules = Vec::new();
        for (key, st) in &self.rules {
            let down = st.down.bytes_per_sec(self.unit);
            let up = st.up.bytes_per_sec(self.unit);
            if down == 0 && up == 0 {
                continue;
            }
            let pids = self.live_pids(key);
            if pids.is_empty() {
                continue;
            }
            rules.push(ProgramRule { key: key.clone(), pids, down_bps: down, up_bps: up });
        }
        rules.sort_by(|a, b| a.key.cmp(&b.key));
        LimitConfig {
            rules,
            other_down_bps: self.other.down.bytes_per_sec(self.unit),
            other_up_bps: self.other.up.bytes_per_sec(self.unit),
        }
    }

    fn live_pids(&self, key: &str) -> Vec<u32> {
        self.programs
            .iter()
            .find(|p| p.key == key)
            .map(|p| p.pids.clone())
            .unwrap_or_default()
    }

    fn processes(&mut self, list: Vec<ProcessInfo>) {
        self.programs = ProgramInfo::group(&list);
        for p in &list {
            self.meta.insert(
                program_key(&p.path, &p.name),
                (p.name.clone(), p.path.clone()),
            );
        }
    }

    fn sample(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_sample) < SAMPLE_INTERVAL {
            return;
        }
        self.last_sample = now;

        if let Some(stats) = &self.stats {
            let guard = stats.lock().unwrap();
            for (key, counter) in guard.iter() {
                let sampler = self
                    .samplers
                    .entry(key.clone())
                    .or_insert_with(|| RateSampler::new(counter.clone()));
                self.rates.insert(key.clone(), sampler.sample());
            }
            self.samplers.retain(|k, _| guard.contains_key(k));
            self.rates.retain(|k, _| guard.contains_key(k));
        }

        self.hist_down.push(self.total(Direction::Down) as f32);
        self.hist_up.push(self.total(Direction::Up) as f32);
    }

    fn total(&self, dir: Direction) -> u64 {
        self.rates
            .iter()
            .filter(|((_, d), _)| *d == dir)
            .map(|(_, v)| *v)
            .sum()
    }

    fn rate_of(&self, key: &str, dir: Direction) -> u64 {
        self.rates.get(&(key.to_string(), dir)).copied().unwrap_or(0)
    }

    fn is_limited(&self, key: &str) -> bool {
        self.rules
            .get(key)
            .map(|r| {
                r.down.bytes_per_sec(self.unit) > 0 || r.up.bytes_per_sec(self.unit) > 0
            })
            .unwrap_or(false)
    }

    fn rule_of(&self, key: &str) -> RuleState {
        self.rules.get(key).cloned().unwrap_or_else(RuleState::new)
    }

    // ── 快照：前端唯一数据源 ──

    fn snapshot(&self) -> Snapshot {
        let unit = self.unit;
        let profiles = profile::all()
            .iter()
            .map(|p| ProfileDto {
                id: p.id,
                name: p.name.to_string(),
                label: p.short_label(unit),
                explain: format!("{}：{}", p.name, p.rate(unit).explain()),
            })
            .collect();

        let mut programs: Vec<ProgramDto> = self
            .programs
            .iter()
            .map(|p| {
                let d = self.rate_of(&p.key, Direction::Down);
                let u = self.rate_of(&p.key, Direction::Up);
                ProgramDto {
                    key: p.key.clone(),
                    name: p.name.clone(),
                    path: p.path.clone().unwrap_or_default(),
                    pids: p.pids.len(),
                    pid_list: p.pids.clone(),
                    down: d,
                    down_text: if d > 0 { format_bps(d) } else { "—".to_string() },
                    up: u,
                    up_text: if u > 0 { format_bps(u) } else { "—".to_string() },
                    limited: self.is_limited(&p.key),
                    icon: self.icon_cache.get(&p.key).and_then(|v| v.clone()),
                }
            })
            .collect();
        programs.sort_by(|a, b| {
            b.limited
                .cmp(&a.limited)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });

        let detail = self.selected.as_deref().map(|key| {
            let live = self.programs.iter().find(|p| p.key == key);
            let (name, path) = match self.meta.get(key) {
                Some((n, p)) => (
                    n.clone(),
                    p.clone().or_else(|| live.and_then(|l| l.path.clone())),
                ),
                None => (
                    live.map(|l| l.name.clone()).unwrap_or_else(|| key.to_string()),
                    live.and_then(|l| l.path.clone()),
                ),
            };
            let rule = self.rule_of(key);
            let d = self.rate_of(key, Direction::Down);
            let u = self.rate_of(key, Direction::Up);
            DetailDto {
                key: key.to_string(),
                name,
                path: path.unwrap_or_default(),
                icon: self.icon_cache.get(key).and_then(|v| v.clone()),
                pids: live.map(|l| l.pids.len()).unwrap_or(0),
                running: live.is_some(),
                down_text: format_bps(d),
                up_text: format_bps(u),
                down_rule: rule.down.dto(unit),
                up_rule: rule.up.dto(unit),
            }
        });

        let other = RuleDto {
            down: self.other.down.dto(unit),
            up: self.other.up.dto(unit),
        };

        let td = self.total(Direction::Down);
        let tu = self.total(Direction::Up);
        let (down_num, down_unit) = split_rate(td);
        let (up_num, up_unit) = split_rate(tu);

        let down = downsample(&self.hist_down, GRAPH_POINTS);
        let up = downsample(&self.hist_up, GRAPH_POINTS);
        let peak = self.hist_down.max().max(self.hist_up.max());

        Snapshot {
            elevated: self.elevated,
            unit: if unit == Unit::Kb { "kb" } else { "mb" }.to_string(),
            unit_label: unit.label().to_string(),
            master: self.master,
            status: self.status.clone(),
            warning: self.warning.clone(),
            engine_diag: self.engine_diag.clone(),
            scanning: self.scanning,
            program_count: programs.len(),
            process_count: programs.iter().map(|p| p.pids).sum(),
            profiles,
            programs,
            selected: self.selected.clone(),
            detail,
            other,
            totals: TotalsDto {
                down_num,
                down_unit,
                down_text: format_bps(td),
                up_num,
                up_unit,
                up_text: format_bps(tu),
            },
            graph: GraphDto { down, up, peak, peak_text: format_bps(peak as u64) },
        }
    }
}

fn split_rate(bps: u64) -> (String, String) {
    if bps == 0 {
        return ("0".to_string(), "KB/s".to_string());
    }
    let s = format_bps(bps);
    let mut it = s.split_whitespace();
    let n = it.next().unwrap_or("0").to_string();
    let u = it.next().unwrap_or("KB/s").to_string();
    (n, u)
}

fn downsample(h: &History, n: usize) -> Vec<f32> {
    let v: Vec<f32> = h.as_slice().copied().collect();
    if v.len() <= n {
        return v;
    }
    (0..n)
        .map(|i| {
            let a = i * v.len() / n;
            let b = ((i + 1) * v.len() / n).max(a + 1).min(v.len());
            v[a..b].iter().copied().fold(0.0f32, f32::max)
        })
        .collect()
}

fn parse_num(s: &str) -> f64 {
    s.trim().replace(',', ".").parse::<f64>().unwrap_or(0.0)
}

// ────────────────────────────── 设置持久化映射 ──────────────────────────────

fn sel_to_str(sel: Sel) -> String {
    match sel {
        Sel::Profile(n) => format!("p{n}"),
        Sel::Custom => "custom".to_string(),
    }
}

fn str_to_sel(s: &str) -> Sel {
    if s == "custom" {
        Sel::Custom
    } else if let Some(rest) = s.strip_prefix('p') {
        Sel::Profile(rest.parse().unwrap_or(5))
    } else {
        Sel::Profile(5)
    }
}

fn dir_to_saved(d: &DirState) -> settings::SavedDir {
    settings::SavedDir { enabled: d.enabled, sel: sel_to_str(d.sel), custom: d.custom.clone() }
}

fn saved_to_dir(s: &settings::SavedDir) -> DirState {
    DirState { enabled: s.enabled, sel: str_to_sel(&s.sel), custom: s.custom.clone() }
}

fn rule_to_saved(r: &RuleState) -> settings::SavedRule {
    settings::SavedRule { down: dir_to_saved(&r.down), up: dir_to_saved(&r.up) }
}

fn saved_to_rule(s: &settings::SavedRule) -> RuleState {
    RuleState { down: saved_to_dir(&s.down), up: saved_to_dir(&s.up) }
}

/// 把磁盘上的设置恢复到运行时状态（启动时使用）
fn apply_settings(g: &mut Inner, s: &settings::Settings) {
    g.unit = if s.unit == "mb" { Unit::Mb } else { Unit::Kb };
    g.master = s.master;
    g.other = saved_to_rule(&s.other);
    g.rules = s.rules.iter().map(|(k, v)| (k.clone(), saved_to_rule(v))).collect();
    g.selected = s.selected.clone();
}

/// 把当前运行时状态收集成可序列化结构
fn collect_settings(g: &Inner) -> settings::Settings {
    settings::Settings {
        version: settings::Settings::VERSION,
        unit: if g.unit == Unit::Mb { "mb".to_string() } else { "kb".to_string() },
        master: g.master,
        other: rule_to_saved(&g.other),
        rules: g.rules.iter().map(|(k, v)| (k.clone(), rule_to_saved(v))).collect(),
        selected: g.selected.clone(),
    }
}

// ⚠️ `std::sync::Mutex` 不可重入：任何持有 `Shared` 锁的作用域里都绝不能再次 `lock()`。
// 曾经 `set_unit` 持着命名 guard 调 `persist()`（内部又 lock 一次），造成自死锁：
// 该命令线程永久阻塞且锁永不释放，随后后台线程与每 220ms 的 `get_snapshot` 全部排队阻塞，
// 表现为「点一下限速控件，整个软件卡死」。
// 统一约定：设置变更一律「锁内改状态 + 锁内 collect_settings 取快照 → 出锁后 s.save()」。
// save() 是磁盘 IO，也绝不放在持锁作用域里。

// ────────────────────────────── 前端数据契约 ──────────────────────────────

#[derive(Serialize)]
struct Snapshot {
    elevated: bool,
    unit: String,
    unit_label: String,
    master: bool,
    status: String,
    warning: Option<String>,
    engine_diag: String,
    scanning: bool,
    program_count: usize,
    process_count: usize,
    profiles: Vec<ProfileDto>,
    programs: Vec<ProgramDto>,
    selected: Option<String>,
    detail: Option<DetailDto>,
    other: RuleDto,
    totals: TotalsDto,
    graph: GraphDto,
}

#[derive(Serialize)]
struct ProfileDto {
    id: u8,
    name: String,
    label: String,
    explain: String,
}

#[derive(Serialize)]
struct ProgramDto {
    key: String,
    name: String,
    path: String,
    pids: usize,
    pid_list: Vec<u32>,
    down: u64,
    down_text: String,
    up: u64,
    up_text: String,
    limited: bool,
    icon: Option<String>,
}

#[derive(Serialize)]
struct DirDto {
    enabled: bool,
    sel: String,
    custom: String,
    short: String,
    explain: String,
    bytes: u64,
}

#[derive(Serialize)]
struct RuleDto {
    down: DirDto,
    up: DirDto,
}

#[derive(Serialize)]
struct DetailDto {
    key: String,
    name: String,
    path: String,
    icon: Option<String>,
    pids: usize,
    running: bool,
    down_text: String,
    up_text: String,
    down_rule: DirDto,
    up_rule: DirDto,
}

#[derive(Serialize)]
struct TotalsDto {
    down_num: String,
    down_unit: String,
    down_text: String,
    up_num: String,
    up_unit: String,
    up_text: String,
}

#[derive(Serialize)]
struct GraphDto {
    down: Vec<f32>,
    up: Vec<f32>,
    peak: f32,
    peak_text: String,
}

#[derive(Deserialize)]
struct RuleEdit {
    target: String,
    key: Option<String>,
    direction: String,
    enabled: Option<bool>,
    profile_id: Option<u8>,
    custom: Option<bool>,
    custom_value: Option<String>,
}

// ────────────────────────────── 后台线程 ──────────────────────────────

fn backend(shared: Shared, stop: Arc<AtomicBool>) {
    let mut engine = WinDivertEngine::new();
    {
        if let Some(mut g) = lock_mutex(&shared) {
            g.stats = Some(engine.stats());
            g.status = "引擎已就绪".to_string();
        }
    }

    let mut sys = sysinfo::System::new();
    let mut last_scan = Instant::now().checked_sub(SCAN_INTERVAL).unwrap_or_else(Instant::now);
    let mut last_attempted: Option<LimitConfig> = None;
    let mut last_error: Option<String> = None;
    let mut last_warning: Option<String> = None;
    let mut last_diag_at = Instant::now()
        .checked_sub(Duration::from_millis(500))
        .unwrap_or_else(Instant::now);

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        // 1) 配置变更 → apply
        let (cfg, master) = {
            let Some(g) = lock_mutex(&shared) else {
                thread::sleep(Duration::from_millis(10));
                continue;
            };
            (g.current_config(), g.master)
        };
        if last_attempted.as_ref() != Some(&cfg) {
            last_attempted = Some(cfg.clone());
            match engine.apply(cfg.clone()) {
                Ok(()) => {
                    last_error = None;
                    let Some(mut g) = lock_mutex(&shared) else { continue };
                    g.status = if !master {
                        "已暂停 · 所有流量恢复正常".to_string()
                    } else if cfg.is_all_unlimited() {
                        "已开启 · 尚未设置限速".to_string()
                    } else {
                        let n = cfg.active_rules().len();
                        if n > 0 {
                            format!("限速中 · {} 个程序", n)
                        } else {
                            "限速中 · 其他流量".to_string()
                        }
                    };
                    // warning 交给循环末尾统一同步（poll 里也可能产生新警告）
                }
                Err(e) => {
                    last_error = Some(e.clone());
                    // 失败要允许重试：否则 last_attempted 一直等于 cfg，用户修好环境后
                    // 不重启程序就永远无法恢复（比如刚点完提权、或刚放行安全软件）。
                    last_attempted = None;
                    let Some(mut g) = lock_mutex(&shared) else { continue };
                    g.status = format!("引擎错误：{e}");
                    // 只在错误明确来自「权限不足」时才提示提权。
                    // 旧判断用 e.contains("管理员")，而错误文案里硬编码了这几个字，
                    // 导致打开句柄的任何失败（filter 非法、驱动被拦截…）都误报成权限问题。
                    g.warning = if e.contains("权限不足") || e.contains("Win32 错误 5") {
                        Some("请以管理员身份运行本程序".to_string())
                    } else {
                        Some(e.clone())
                    };
                    last_warning = g.warning.clone();
                    // 失败也要刷新诊断（否则诊断恒为 0，会误导排查）
                    let d = engine.diag();
                    g.engine_diag = format!(
                        "句柄 {}/2 · 桶 {} · 收包 {} · 命中限速 {} · 丢弃 {} · 程序识别 {} · 连接表 TCP {}/UDP {}",
                        d.handles, d.buckets, d.packets, d.limited, d.dropped,
                        d.program_hits, d.tcp_entries, d.udp_entries
                    );
                    last_diag_at = Instant::now();
                    // 失败后不要高频重试打开驱动句柄
                    drop(g);
                    thread::sleep(Duration::from_millis(500));
                    continue;
                }
            }
        }

        // 2) 进程扫描（3 秒一次，或前端手动点刷新）
        let need_scan = {
            let Some(g) = lock_mutex(&shared) else {
                thread::sleep(Duration::from_millis(10));
                continue;
            };
            g.refresh_requested
        };
        if need_scan || last_scan.elapsed() >= SCAN_INTERVAL {
            sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
            let list: Vec<ProcessInfo> = sys
                .processes()
                .iter()
                .map(|(pid, p)| {
                    ProcessInfo::new(
                        pid.as_u32(),
                        p.name().to_string_lossy().into_owned(),
                        p.exe().map(|e| e.to_string_lossy().into_owned()),
                    )
                })
                .collect();
            let Some(mut g) = lock_mutex(&shared) else { continue };
            g.processes(list);
            g.scanning = false;
            g.refresh_requested = false;
            last_scan = Instant::now();
        }

        // 3) 100ms 采样 + 曲线
        if let Some(mut g) = lock_mutex(&shared) {
            g.sample();
        }

        // 4) 收包 / 令牌桶判定
        match engine.poll(POLL_BATCH) {
            // 无论本轮有没有包都让出 1ms：管理员下持续有流量时若全速空转，
            // 后台线程会高频抢 shared 锁，前端 get_snapshot 容易饥饿、界面像假死。
            Ok(_) => thread::sleep(Duration::from_millis(1)),
            Err(e) => {
                if last_error.as_deref() != Some(e.as_str()) {
                    last_error = Some(e.clone());
                    if let Some(mut g) = lock_mutex(&shared) {
                        g.status = format!("引擎错误：{e}");
                    }
                }
                thread::sleep(Duration::from_millis(250));
            }
        }

        // 5) 同步引擎的非致命警告与实时诊断。
        // 诊断里的收包计数在高流量下每轮都变，若每轮都抢锁会加剧锁竞争 → 最多 500ms 刷一次。
        let w = engine.warning().map(|s| s.to_string());
        let now = Instant::now();
        if w != last_warning || now.duration_since(last_diag_at) >= Duration::from_millis(500) {
            let d = engine.diag();
            let diag_text = format!(
                "句柄 {}/2 · 桶 {} · 收包 {} · 命中限速 {} · 丢弃 {} · 程序识别 {} · 连接表 TCP {}/UDP {}",
                d.handles, d.buckets, d.packets, d.limited, d.dropped,
                d.program_hits, d.tcp_entries, d.udp_entries
            );
            // 开了总开关却没有任何桶 = 根本没接管流量，明确提示，避免「点了没效果」
            let hint = if d.handles == 0 && d.buckets == 0 {
                Some("未接管流量：请选择挡位或开启一个方向的限速开关".to_string())
            } else {
                None
            };
            if let Some(mut g) = lock_mutex(&shared) {
                g.warning = w.clone().or(hint);
                g.engine_diag = diag_text.clone();
            }
            last_warning = w;
            last_diag_at = now;
        }
    }
}

// ────────────────────────────── Tauri 命令 ──────────────────────────────

#[tauri::command]
fn get_snapshot(state: State<'_, Shared>) -> Option<Snapshot> {
    // 图标提取走 Win32 Shell API（每个程序一次 SHGetFileInfoW/GetDIBits），可能较慢。
    // 绝不能持锁做：先在锁内取出待办清单，出锁后提取，再回锁内写缓存。
    let todo: Vec<(String, Option<String>)> = {
        let g = lock_mutex(&state)?;
        g.programs
            .iter()
            .filter(|p| !g.icon_cache.contains_key(&p.key))
            .take(24)
            .map(|p| (p.key.clone(), p.path.clone()))
            .collect()
    };
    if !todo.is_empty() {
        let mut fetched = Vec::with_capacity(todo.len());
        for (key, path) in todo {
            let icon = path.as_deref().and_then(icons::exe_icon_base64);
            fetched.push((key, icon));
        }
        let mut g = lock_mutex(&state)?;
        for (k, v) in fetched {
            g.icon_cache.entry(k).or_insert(v);
        }
    }
    Some(lock_mutex(&state)?.snapshot())
}

#[tauri::command]
fn set_master(state: State<'_, Shared>, on: bool) {
    let s = {
        let Some(mut g) = lock_mutex(&state) else { return };
        g.master = on;
        if on {
            // 「开启总开关」却一条生效规则都没有 → LimitConfig 全 0，引擎不会打开任何
            // 句柄，用户会看到「开关亮了但完全没限速」。默认挡位在界面上高亮、
            // 但 DirState.enabled 初始为 false，正是这个陷阱的来源。
            // 此时自动启用「其他流量」下行的当前选中挡位，让开启立即有可见效果。
            let any = g.other.down.enabled || g.other.up.enabled
                || g.rules.values().any(|r| r.down.enabled || r.up.enabled);
            if !any {
                g.other.down.enabled = true;
            }
        }
        collect_settings(&g)
    };
    s.save();
}

#[tauri::command]
fn set_unit(state: State<'_, Shared>, unit: String) {
    let s = {
        let Some(mut g) = lock_mutex(&state) else { return };
        g.unit = if unit == "mb" { Unit::Mb } else { Unit::Kb };
        collect_settings(&g)
    };
    s.save();
}

#[tauri::command]
fn select_program(state: State<'_, Shared>, key: Option<String>) {
    let s = {
        let Some(mut g) = lock_mutex(&state) else { return };
        g.selected = key;
        collect_settings(&g)
    };
    s.save();
}

#[tauri::command]
fn clear_rule(state: State<'_, Shared>, key: String) {
    let s = {
        let Some(mut g) = lock_mutex(&state) else { return };
        g.rules.remove(&key);
        collect_settings(&g)
    };
    s.save();
}

#[tauri::command]
fn set_rule(state: State<'_, Shared>, edit: RuleEdit) {
    let s = {
        let Some(mut g) = lock_mutex(&state) else { return };
        let rule: &mut RuleState = match (edit.target.as_str(), edit.key.as_deref()) {
            ("other", _) => &mut g.other,
            ("program", Some(k)) => g.rules.entry(k.to_string()).or_insert_with(RuleState::new),
            _ => return,
        };
        let ds = if edit.direction == "up" { &mut rule.up } else { &mut rule.down };

        if let Some(v) = edit.enabled {
            ds.enabled = v;
        }
        if let Some(id) = edit.profile_id {
            ds.sel = Sel::Profile(id);
            ds.enabled = true; // 选挡位即视为启用该方向
        }
        if edit.custom == Some(true) {
            ds.sel = Sel::Custom;
            ds.enabled = true;
        }
        if let Some(ref v) = edit.custom_value {
            ds.custom = v.clone();
        }
        // 用户主动设挡位/开关/自定义速率，意图就是「开始限速」。
        // 若总开关还关着，current_config() 会返回 unlimited()、引擎根本不打开句柄，
        // 表现为「点了挡位高亮了却完全没效果」。这里自动开启总开关，
        // 用户仍可随时手动关掉总开关来全局暂停。
        let activating = edit.enabled == Some(true)
            || edit.profile_id.is_some()
            || edit.custom == Some(true)
            || edit.custom_value.is_some();
        if activating {
            g.master = true;
        }
        collect_settings(&g)
    };
    s.save();
}

#[tauri::command]
fn refresh(state: State<'_, Shared>) {
    let Some(mut g) = lock_mutex(&state) else { return };
    g.refresh_requested = true;
    g.scanning = true;
}

#[tauri::command]
fn relaunch_admin() {
    // 以管理员身份重新启动自身（触发 UAC），随后退出当前（未提权）实例
    if let Ok(exe) = std::env::current_exe() {
        let path = exe.to_string_lossy().replace('\'', "''");
        let _ = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-WindowStyle",
                "Hidden",
                "-Command",
                &format!("Start-Process -Verb RunAs -FilePath '{path}'"),
            ])
            .spawn();
    }
    std::process::exit(0);
}

#[tauri::command]
fn win_action(app: AppHandle, action: String) {
    let Some(w) = app.get_webview_window("main") else { return };
    match action.as_str() {
        "min" => { let _ = w.minimize(); }
        "max" => {
            if w.is_maximized().unwrap_or(false) {
                let _ = w.unmaximize();
            } else {
                let _ = w.maximize();
            }
        }
        "close" => { let _ = app.exit(0); }
        _ => {}
    }
}

// ────────────────────────────── 入口 ──────────────────────────────

fn main() {
    let stop = Arc::new(AtomicBool::new(false));
    let mut inner = Inner::new();
    apply_settings(&mut inner, &settings::Settings::load());
    let shared: Shared = Arc::new(Mutex::new(inner));

    let backend_shared = shared.clone();
    let backend_stop = stop.clone();
    thread::spawn(move || backend(backend_shared, backend_stop));

    let exit_stop = stop.clone();
    tauri::Builder::default()
        .manage(shared)
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            set_master,
            set_unit,
            select_program,
            clear_rule,
            set_rule,
            refresh,
            win_action,
            relaunch_admin
        ])
        .build(tauri::generate_context!())
        .expect("启动 Tauri 应用失败")
        .run(move |_app, event| {
            if matches!(
                event,
                tauri::RunEvent::Exit | tauri::RunEvent::ExitRequested { .. }
            ) {
                exit_stop.store(true, Ordering::Relaxed);
            }
        });
}
