//! 流律 GUI(Tauri 2 后端,Throttle 形态前端)
//!
//! 架构:rl-engine 提供四线程引擎(flow-events / engine / drainer / aggregator),
//! 负责捕获、进程归属、令牌桶整形与 1 Hz 快照。本文件只做三件事:
//!   1. 启动引擎(失败时把原因带给前端展示错误条);
//!   2. 把 Snapshot 转成 JSON DTO 供前端 220ms 轮询;
//!   3. 把前端操作转成 Command 发给引擎聚合线程。
//! 前端只渲染,不持有任何业务状态;规则持久化由引擎负责(%APPDATA%\rl-app\rules.json)。

// release 走 Windows GUI 子系统(双击不弹控制台),debug 保留 console 便于排错
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::Serialize;
use std::collections::HashMap;
use std::sync::Mutex;

use tauri::{AppHandle, Manager, State};

mod icons;

use rl_engine::backend;
use rl_engine::single_instance;
use rl_engine::types::{Command, Rule, Snapshot};

/// Tauri managed state:引擎句柄 + 最近一次快照。
struct App {
    /// GUI → 引擎命令通道(引擎启动失败时为 None,所有 set_* 静默丢弃)。
    cmd_tx: Option<crossbeam_channel::Sender<Command>>,
    /// 引擎 → GUI 快照流(约 1 Hz)。
    snapshot_rx: crossbeam_channel::Receiver<Snapshot>,
    /// 最近一次快照缓存:前端轮询比引擎节拍快,两次推送之间返回缓存。
    latest: Mutex<Option<Snapshot>>,
    /// 聚合线程句柄,退出时 join 以保证 rules.json 落盘。
    backend: Mutex<Option<backend::Backend>>,
    /// 引擎启动失败原因(未提权 / WinDivert 驱动缺失),前端展示错误条。
    engine_error: Option<String>,
    /// 当前进程是否已提权(决定前端是否显示「以管理员重启」按钮)。
    elevated: bool,
    /// exe 图标缓存:路径 → base64 PNG(失败也缓存,避免反复打开进程)。
    icon_cache: Mutex<HashMap<String, Option<String>>>,
}

impl App {
    fn send(&self, cmd: Command) {
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.send(cmd);
        }
    }
}

// ────────────────────────────── 快照 DTO ──────────────────────────────

#[derive(Serialize)]
struct ProcessDto {
    pid: u32,
    exe_path: String,
    name: String,
    down_rate: u64,
    up_rate: u64,
    down_total: u64,
    up_total: u64,
    down_history: Vec<u64>,
    up_history: Vec<u64>,
    flow_count: usize,
}

#[derive(Serialize)]
struct SnapshotDto {
    processes: Vec<ProcessDto>,
    total_down_rate: u64,
    total_up_rate: u64,
    rules: HashMap<String, Rule>,
    master: bool,
    other_down_bps: Option<u64>,
    other_up_bps: Option<u64>,
    engine_error: Option<String>,
    elevated: bool,
    diagnostics: rl_engine::types::Diagnostics,
}

fn snapshot_dto(snap: &Snapshot, app: &App) -> SnapshotDto {
    let mut processes: Vec<ProcessDto> = snap
        .processes
        .values()
        .map(|p| ProcessDto {
            pid: p.pid,
            exe_path: p.exe_path.clone(),
            name: p.name.clone(),
            down_rate: p.down_rate,
            up_rate: p.up_rate,
            down_total: p.down_total,
            up_total: p.up_total,
            down_history: p.down_history.clone(),
            up_history: p.up_history.clone(),
            flow_count: p.flow_count,
        })
        .collect();
    // 速率高的排前面,同速率按名字稳定排序;前端直接按序渲染。
    processes.sort_by(|a, b| {
        (b.down_rate + b.up_rate)
            .cmp(&(a.down_rate + a.up_rate))
            .then_with(|| a.name.cmp(&b.name))
    });

    SnapshotDto {
        processes,
        total_down_rate: snap.total_down_rate,
        total_up_rate: snap.total_up_rate,
        rules: snap.rules.clone(),
        // 引擎未启动时展示 master=on,避免前端把「引擎没跑」误渲染成「总开关关」。
        master: if app.engine_error.is_some() { true } else { snap.master },
        other_down_bps: snap.other_down_bps,
        other_up_bps: snap.other_up_bps,
        engine_error: app.engine_error.clone(),
        elevated: app.elevated,
        diagnostics: snap.diagnostics,
    }
}

// ────────────────────────────── Tauri commands ──────────────────────────────

#[tauri::command]
fn get_snapshot(state: State<'_, App>) -> SnapshotDto {
    // 排空通道只留最新一帧,避免轮询慢时堆积。
    let mut newest = None;
    while let Ok(snap) = state.snapshot_rx.try_recv() {
        newest = Some(snap);
    }
    if let Some(s) = newest {
        *state.latest.lock().unwrap() = Some(s);
    }
    let snap = state.latest.lock().unwrap().clone().unwrap_or_default();
    snapshot_dto(&snap, &state)
}

/// 新增或替换某进程的规则(前端传完整 Rule JSON)。
#[tauri::command]
fn set_rule(state: State<'_, App>, rule: Rule) {
    state.send(Command::SetRule(rule));
}

/// 清除某进程的规则(exe 路径,大小写不敏感,统一转小写)。
#[tauri::command]
fn clear_rule(state: State<'_, App>, exe: String) {
    state.send(Command::RemoveRule(exe.to_lowercase()));
}

/// 总开关:关闭后引擎对所有流量直通(但仍统计速率供 UI 显示)。
#[tauri::command]
fn set_master(state: State<'_, App>, on: bool) {
    state.send(Command::SetMaster(on));
}

/// 「其他流量」兜底限速(None = 该方向不限)。
#[tauri::command]
fn set_other_limits(state: State<'_, App>, down_bps: Option<u64>, up_bps: Option<u64>) {
    state.send(Command::SetOtherLimits { down_bps, up_bps });
}

/// 提取 exe 图标 → base64 PNG(带缓存,进程列表按需调用)。
#[tauri::command]
fn get_icon(state: State<'_, App>, exe_path: String) -> Option<String> {
    let mut cache = state.icon_cache.lock().unwrap();
    if let Some(hit) = cache.get(&exe_path) {
        return hit.clone();
    }
    let icon = icons::exe_icon_base64(&exe_path);
    cache.insert(exe_path, icon.clone());
    icon
}

#[tauri::command]
fn relaunch_admin() {
    // 以管理员身份重新启动自身(触发 UAC),随后退出当前(未提权)实例
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

/// 当前进程是否已提权(UAC 过滤令牌下返回 false,完全提权返回 true)
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

// ────────────────────────────── 入口 ──────────────────────────────

fn main() {
    // 单实例:第二个实例直接退出,由正在运行的实例弹出主窗口。
    // 两个实例会各自打开 WinDivert 句柄,把流量拆成两半,规则互相失效。
    if single_instance::acquire().is_none() {
        return;
    }

    // debug 构建把引擎日志打到 console,便于排错;release 无 console 不初始化。
    #[cfg(debug_assertions)]
    {
        use tracing_subscriber::filter::LevelFilter;
        let _ = tracing_subscriber::fmt()
            .with_max_level(LevelFilter::INFO)
            .try_init();
    }

    let elevated = is_elevated();

    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
    // WinDivert 打不开(未提权/驱动缺失)时不阻断 GUI:错误条 + 空快照。
    let (backend_opt, snapshot_rx, engine_error) = match backend::start(cmd_rx) {
        Ok(b) => {
            let rx = b.snapshot_rx.clone();
            (Some(b), rx, None)
        }
        Err(e) => (None, crossbeam_channel::unbounded().1, Some(format!("{e:#}"))),
    };
    let cmd_tx = if engine_error.is_none() { Some(cmd_tx) } else { None };

    tauri::Builder::default()
        .manage(App {
            cmd_tx,
            snapshot_rx,
            latest: Mutex::new(None),
            backend: Mutex::new(backend_opt),
            engine_error,
            elevated,
            icon_cache: Mutex::new(HashMap::new()),
        })
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            set_rule,
            clear_rule,
            set_master,
            set_other_limits,
            get_icon,
            win_action,
            relaunch_admin
        ])
        .setup(|app| {
            // 第二个实例被拉起时,把主窗口带回前台。
            let handle = app.handle().clone();
            single_instance::on_show_request(move || {
                if let Some(w) = handle.get_webview_window("main") {
                    let _ = w.unminimize();
                    let _ = w.set_focus();
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("启动 Tauri 应用失败")
        .run(|app_handle, event| {
            if matches!(
                event,
                tauri::RunEvent::Exit | tauri::RunEvent::ExitRequested { .. }
            ) {
                // 聚合线程退出时写 rules.json;不等它就退出会截断文件丢规则。
                if let Some(state) = app_handle.try_state::<App>() {
                    if let Some(tx) = &state.cmd_tx {
                        let _ = tx.send(Command::Shutdown);
                    }
                    if let Ok(mut guard) = state.backend.try_lock() {
                        if let Some(b) = guard.take() {
                            b.join();
                        }
                    }
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use rl_engine::types::{Diagnostics, ProcessStats};

    /// 进程列表必须按总速率降序:前端直接按序渲染,后端不排序会导致
    /// 高速进程淹没在长列表底部。
    #[test]
    fn processes_are_sorted_by_total_rate_desc() {
        let mut snap = Snapshot::default();
        for (name, down) in [("a.exe", 100u64), ("b.exe", 5000), ("c.exe", 5000), ("d.exe", 0)] {
            snap.processes.insert(
                format!("c:\\{name}"),
                ProcessStats {
                    pid: 1,
                    exe_path: format!("c:\\{name}"),
                    name: name.to_string(),
                    down_rate: down,
                    ..Default::default()
                },
            );
        }

        let app = App {
            cmd_tx: None,
            snapshot_rx: crossbeam_channel::unbounded().1,
            latest: Mutex::new(None),
            backend: Mutex::new(None),
            engine_error: None,
            elevated: true,
            icon_cache: Mutex::new(HashMap::new()),
        };
        let dto = snapshot_dto(&snap, &app);

        let rates: Vec<u64> = dto.processes.iter().map(|p| p.down_rate).collect();
        assert_eq!(rates, vec![5000, 5000, 100, 0]);
        // 同速率按名字稳定排序,避免每秒抖动
        assert_eq!(dto.processes[0].name, "b.exe");
        assert_eq!(dto.processes[1].name, "c.exe");
    }

    /// 引擎启动失败时,DTO 必须带出错误信息,且 master 展示为开
    /// (否则用户会把「引擎没跑」误读成「总开关关了所以全不限速」)。
    #[test]
    fn engine_error_is_surfaced_and_master_shows_on() {
        let snap = Snapshot::default(); // master 默认 false
        let app = App {
            cmd_tx: None,
            snapshot_rx: crossbeam_channel::unbounded().1,
            latest: Mutex::new(None),
            backend: Mutex::new(None),
            engine_error: Some("failed to open WinDivert".into()),
            elevated: false,
            icon_cache: Mutex::new(HashMap::new()),
        };
        let dto = snapshot_dto(&snap, &app);
        assert_eq!(dto.engine_error.as_deref(), Some("failed to open WinDivert"));
        assert!(dto.master);
        assert!(!dto.elevated);
        assert!(dto.diagnostics == Diagnostics::default());
    }
}
