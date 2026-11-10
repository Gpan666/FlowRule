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
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager, State};

mod icons;

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
}

type Shared = Arc<Mutex<Inner>>;

impl Inner {
    fn new() -> Self {
        Self {
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

    /// 分批提取真实 exe 图标，避免首次快照卡住（每轮最多 budget 个）。
    /// 取不到图标的程序缓存 None，前端就不渲染图标位、只显示名称。
    fn fill_icons(&mut self, budget: usize) {
        let todo: Vec<(String, Option<String>)> = self
            .programs
            .iter()
            .filter(|p| !self.icon_cache.contains_key(&p.key))
            .take(budget)
            .map(|p| (p.key.clone(), p.path.clone()))
            .collect();
        for (key, path) in todo {
            let icon = path.as_deref().and_then(icons::exe_icon_base64);
            self.icon_cache.insert(key, icon);
        }
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
            unit: if unit == Unit::Kb { "kb" } else { "mb" }.to_string(),
            unit_label: unit.label().to_string(),
            master: self.master,
            status: self.status.clone(),
            warning: self.warning.clone(),
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

// ────────────────────────────── 前端数据契约 ──────────────────────────────

#[derive(Serialize)]
struct Snapshot {
    unit: String,
    unit_label: String,
    master: bool,
    status: String,
    warning: Option<String>,
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
        let mut g = shared.lock().unwrap();
        g.stats = Some(engine.stats());
        g.status = "引擎已就绪".to_string();
    }

    let mut sys = sysinfo::System::new();
    let mut last_scan = Instant::now().checked_sub(SCAN_INTERVAL).unwrap_or_else(Instant::now);
    let mut last_attempted: Option<LimitConfig> = None;
    let mut last_error: Option<String> = None;

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        // 1) 配置变更 → apply
        let (cfg, master) = {
            let g = shared.lock().unwrap();
            (g.current_config(), g.master)
        };
        if last_attempted.as_ref() != Some(&cfg) {
            last_attempted = Some(cfg.clone());
            match engine.apply(cfg.clone()) {
                Ok(()) => {
                    last_error = None;
                    let mut g = shared.lock().unwrap();
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
                    g.warning = engine.warning().map(|s| s.to_string());
                }
                Err(e) => {
                    last_error = Some(e.clone());
                    let mut g = shared.lock().unwrap();
                    g.status = format!("引擎错误：{e}");
                    if e.contains("权限") || e.contains("管理员") || e.contains("驱动") {
                        g.warning = Some("请以管理员身份运行本程序".to_string());
                    }
                }
            }
        }

        // 2) 进程扫描（3 秒一次，或前端手动点刷新）
        let need_scan = {
            let g = shared.lock().unwrap();
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
            let mut g = shared.lock().unwrap();
            g.processes(list);
            g.scanning = false;
            g.refresh_requested = false;
            last_scan = Instant::now();
        }

        // 3) 100ms 采样 + 曲线
        shared.lock().unwrap().sample();

        // 4) 收包 / 令牌桶判定
        match engine.poll(POLL_BATCH) {
            Ok(0) => thread::sleep(Duration::from_millis(1)),
            Ok(_) => {}
            Err(e) => {
                if last_error.as_deref() != Some(e.as_str()) {
                    last_error = Some(e.clone());
                    let mut g = shared.lock().unwrap();
                    g.status = format!("引擎错误：{e}");
                }
                thread::sleep(Duration::from_millis(250));
            }
        }
    }
}

// ────────────────────────────── Tauri 命令 ──────────────────────────────

#[tauri::command]
fn get_snapshot(state: State<'_, Shared>) -> Snapshot {
    let mut g = state.lock().unwrap();
    g.fill_icons(24);
    g.snapshot()
}

#[tauri::command]
fn set_master(state: State<'_, Shared>, on: bool) {
    state.lock().unwrap().master = on;
}

#[tauri::command]
fn set_unit(state: State<'_, Shared>, unit: String) {
    let mut g = state.lock().unwrap();
    g.unit = if unit == "mb" { Unit::Mb } else { Unit::Kb };
}

#[tauri::command]
fn select_program(state: State<'_, Shared>, key: Option<String>) {
    state.lock().unwrap().selected = key;
}

#[tauri::command]
fn clear_rule(state: State<'_, Shared>, key: String) {
    state.lock().unwrap().rules.remove(&key);
}

#[tauri::command]
fn set_rule(state: State<'_, Shared>, edit: RuleEdit) {
    let mut g = state.lock().unwrap();
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
    if let Some(v) = edit.custom_value {
        ds.custom = v;
    }
}

#[tauri::command]
fn refresh(state: State<'_, Shared>) {
    let mut g = state.lock().unwrap();
    g.refresh_requested = true;
    g.scanning = true;
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
    let shared: Shared = Arc::new(Mutex::new(Inner::new()));

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
            win_action
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
