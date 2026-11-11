//! Backend: capture, per-process attribution, shaping, and the 1 Hz snapshot
//! feed to the GUI.
//!
//! [`start`] spins up four named threads that communicate through an
//! `Arc<Shared>`:
//!   1. flow-events: WinDivert flow-layer events (PID-attributed) keep the
//!      flow table current; also bootstraps existing TCP connections.
//!   2. engine: WinDivert network-layer capture plus token-bucket shaping.
//!   3. drainer: re-injects packets held by the shaping buckets.
//!   4. aggregator: ticks once per second to build a [`Snapshot`] from
//!      shared state, and applies incoming [`Command`]s (rule edits, shutdown).
//!
//! Shutdown: a [`Command::Shutdown`] (or a disconnected command channel) clears
//! the `running` flag and persists rules. The capture threads block in
//! `WinDivert::recv`, so they observe the flag and exit on the next packet or
//! flow event. The safe `windivert` wrapper takes `&mut self` for `shutdown`,
//! which cannot be called on the `!Send` handle from another thread.

pub mod engine;
pub mod flows;
pub mod rules;
pub mod sockets;
pub mod synthetic;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::Context;
use crossbeam_channel::{Receiver, Sender};
use windivert::prelude::*;

use crate::types::{Command, Diagnostics, Rule, Snapshot};

/// Fallback bucket key: traffic that matches no rule (or no process) is
/// shaped by the "other traffic" limits under this pseudo-exe.
pub const OTHER_EXE: &str = "__other__";

/// Fallback limits applied to traffic that matches no rule.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct OtherLimits {
    pub down_bps: Option<u64>,
    pub up_bps: Option<u64>,
}

/// Atomic engine counters the aggregator reads for the diagnostics bar.
#[derive(Default)]
pub(crate) struct DiagAtomic {
    pub packets_recv: AtomicU64,
    pub shaped: AtomicU64,
    pub dropped: AtomicU64,
    pub attributed: AtomicU64,
    pub tcp4: AtomicUsize,
    pub udp4: AtomicUsize,
    pub tcp6: AtomicUsize,
    pub udp6: AtomicUsize,
}

impl DiagAtomic {
    fn snapshot(&self) -> Diagnostics {
        Diagnostics {
            handles_open: 1,
            packets_recv: self.packets_recv.load(Ordering::Relaxed),
            shaped: self.shaped.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            attributed: self.attributed.load(Ordering::Relaxed),
            tcp4: self.tcp4.load(Ordering::Relaxed),
            udp4: self.udp4.load(Ordering::Relaxed),
            tcp6: self.tcp6.load(Ordering::Relaxed),
            udp6: self.udp6.load(Ordering::Relaxed),
        }
    }
}

/// State shared across all backend threads.
///
/// Locking discipline: every thread holds at most one of `flows`, `rules`,
/// `buckets` at any instant, so no lock-ordering deadlock is possible.
pub(crate) struct Shared {
    pub flows: Mutex<flows::FlowTable>,
    pub rules: Mutex<HashMap<String, Rule>>,
    pub buckets: Mutex<HashMap<engine::BucketKey, engine::ShapedBucket>>,
    pub running: AtomicBool,
    /// Master switch: false => the engine passes every packet through
    /// untouched (no attribution, no shaping).
    pub master: AtomicBool,
    /// Fallback limits for traffic that matches no rule.
    pub other: Mutex<OtherLimits>,
    /// Engine counters for the diagnostics bar.
    pub diag: DiagAtomic,
}

/// Handle to the running backend. The GUI reads snapshots from `snapshot_rx`.
pub struct Backend {
    /// ~1 Hz stream of state snapshots for the GUI.
    pub snapshot_rx: Receiver<Snapshot>,
    /// The aggregator thread: the one that persists rules on shutdown.
    aggregator: JoinHandle<()>,
    /// The capture threads. Kept alive with the backend; not joined on exit
    /// because they block in `WinDivert::recv` until the next packet arrives.
    #[allow(dead_code)]
    capture: Vec<JoinHandle<()>>,
}

impl Backend {
    /// Wait for the aggregator to finish. Call this after sending
    /// [`Command::Shutdown`] (or dropping the command sender) and before the
    /// process exits: the aggregator writes `rules.json` on its way out, and
    /// exiting underneath it leaves the file truncated, silently losing every
    /// rule. The capture threads are left to die with the process.
    pub fn join(self) {
        let _ = self.aggregator.join();
    }
}

/// Start the backend. Consumes the command receiver (GUI to backend) and returns
/// a [`Backend`] exposing the snapshot receiver (backend to GUI).
///
/// Fails early with a clear message if WinDivert cannot be opened (typically:
/// not running elevated, or `WinDivert.dll`/driver missing).
pub fn start(cmd_rx: Receiver<Command>) -> anyhow::Result<Backend> {
    // Open the single shared network-layer handle up front so failures (not
    // elevated, driver missing) surface as a friendly error. Both the engine
    // and drainer threads use this one handle (see engine::EngineHandle).
    let engine_handle = Arc::new(engine::open().context(
        "failed to open WinDivert. Run Throttle as Administrator and ensure \
         WinDivert.dll and the driver are present next to the executable",
    )?);

    let persisted = rules::load();
    let shared = Arc::new(Shared {
        flows: Mutex::new(flows::FlowTable::new()),
        rules: Mutex::new(persisted.rules),
        buckets: Mutex::new(HashMap::new()),
        running: AtomicBool::new(true),
        master: AtomicBool::new(persisted.master),
        other: Mutex::new(OtherLimits {
            down_bps: persisted.other_down_bps,
            up_bps: persisted.other_up_bps,
        }),
        diag: DiagAtomic::default(),
    });

    let (snapshot_tx, snapshot_rx) = crossbeam_channel::unbounded::<Snapshot>();

    let mut handles = Vec::with_capacity(3);

    handles.push(spawn_named("throttle-flow-events", {
        let shared = shared.clone();
        move || run_flow_events(shared)
    })?);

    handles.push(spawn_named("throttle-engine", {
        let shared = shared.clone();
        let handle = engine_handle.clone();
        move || engine::run_engine(shared, handle)
    })?);

    handles.push(spawn_named("throttle-drainer", {
        let shared = shared.clone();
        let handle = engine_handle.clone();
        move || engine::run_drainer(shared, handle)
    })?);

    let aggregator = spawn_named("throttle-aggregator", {
        let shared = shared.clone();
        move || run_aggregator(shared, cmd_rx, snapshot_tx)
    })?;

    Ok(Backend {
        snapshot_rx,
        aggregator,
        capture: handles,
    })
}

fn spawn_named<F>(name: &str, f: F) -> anyhow::Result<JoinHandle<()>>
where
    F: FnOnce() + Send + 'static,
{
    thread::Builder::new()
        .name(name.to_string())
        .spawn(f)
        .with_context(|| format!("failed to spawn thread {name}"))
}

/// How many flow events to log per protocol at startup (diagnostics).
const FLOW_EVENT_LOG_LIMIT: u32 = 10;
/// How often the aggregator re-reads the kernel socket tables so late-created
/// sockets (especially wildcard UDP/QUIC) get attributed.
const TABLE_REFRESH: Duration = Duration::from_secs(2);

/// Flow-event thread: bootstrap existing connections, then track flow
/// establishment/teardown to keep the 5-tuple to process map current.
fn run_flow_events(shared: Arc<Shared>) {
    // Bootstrap pre-existing TCP connections before we start listening.
    shared.flows.lock().unwrap().bootstrap_tcp();

    let divert = match WinDivert::flow("true", 0, WinDivertFlags::new()) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("flow-events: failed to open WinDivert flow handle: {e}");
            return;
        }
    };
    tracing::info!("flow-events thread started");

    // Diagnostics: log the first few events of each protocol so a protocol
    // whose 5-tuples never match captured packets is visible in the log.
    let mut logged: HashMap<u8, u32> = HashMap::new();

    while shared.running.load(Ordering::Relaxed) {
        let packet = match divert.recv(None) {
            Ok(p) => p,
            Err(WinDivertError::Recv(WinDivertRecvError::NoData)) => break,
            Err(e) => {
                tracing::warn!("flow-events: recv error: {e}");
                continue;
            }
        };

        let addr = &packet.address;
        let protocol = addr.protocol();
        let local = SocketAddr::new(addr.local_address(), addr.local_port());
        let remote = SocketAddr::new(addr.remote_address(), addr.remote_port());
        let pid = addr.process_id();

        let seen = logged.entry(protocol).or_default();
        if *seen < FLOW_EVENT_LOG_LIMIT {
            *seen += 1;
            tracing::info!(
                "flow event: {:?} proto={protocol} local={local} remote={remote} pid={pid}",
                addr.event()
            );
        }

        match addr.event() {
            WinDivertEvent::FlowStablished => {
                shared
                    .flows
                    .lock()
                    .unwrap()
                    .insert_flow(protocol, local, remote, pid);
            }
            WinDivertEvent::FlowDeleted => {
                shared
                    .flows
                    .lock()
                    .unwrap()
                    .remove_flow(protocol, local, remote);
            }
            _ => {}
        }
    }
    tracing::info!("flow-events thread exiting");
}

/// Aggregator thread: emit a snapshot every second and apply commands.
fn run_aggregator(shared: Arc<Shared>, cmd_rx: Receiver<Command>, snapshot_tx: Sender<Snapshot>) {
    tracing::info!("aggregator thread started");
    let ticker = crossbeam_channel::tick(Duration::from_secs(1));
    let refresh = crossbeam_channel::tick(TABLE_REFRESH);

    loop {
        crossbeam_channel::select! {
            recv(cmd_rx) -> msg => {
                match msg {
                    Ok(Command::SetRule(mut rule)) => {
                        rule.exe_path = rule.exe_path.to_lowercase();
                        let key = rule.exe_path.clone();
                        let disabled = rule.disabled;
                        let snapshot = {
                            let mut rules = shared.rules.lock().unwrap();
                            rules.insert(key.clone(), rule);
                            rules.clone()
                        };
                        // A disabled rule must stop shaping at once: drop its
                        // buckets so held packets are released, as for a removal.
                        if disabled {
                            shared
                                .buckets
                                .lock()
                                .unwrap()
                                .retain(|(exe, _), _| exe != &key);
                        }
                        if let Err(e) = rules::save(&rules::persisted_from(
                            &snapshot,
                            shared.master.load(Ordering::Relaxed),
                            &shared.other.lock().unwrap(),
                        )) {
                            tracing::warn!("failed to persist rules: {e}");
                        }
                    }
                    Ok(Command::RemoveRule(path)) => {
                        let key = path.to_lowercase();
                        let snapshot = {
                            let mut rules = shared.rules.lock().unwrap();
                            rules.remove(&key);
                            rules.clone()
                        };
                        // A removed rule may have left shaping buckets behind; drop them
                        // so held packets are released and no stale limit lingers.
                        shared
                            .buckets
                            .lock()
                            .unwrap()
                            .retain(|(exe, _), _| exe != &key);
                        if let Err(e) = rules::save(&rules::persisted_from(
                            &snapshot,
                            shared.master.load(Ordering::Relaxed),
                            &shared.other.lock().unwrap(),
                        )) {
                            tracing::warn!("failed to persist rules: {e}");
                        }
                    }
                    Ok(Command::SetMaster(on)) => {
                        shared.master.store(on, Ordering::Relaxed);
                        // Turning the master off must release everything the
                        // buckets are holding right now.
                        if !on {
                            shared.buckets.lock().unwrap().clear();
                        }
                        if let Err(e) = rules::save(&rules::persisted_from(
                            &shared.rules.lock().unwrap().clone(),
                            on,
                            &shared.other.lock().unwrap(),
                        )) {
                            tracing::warn!("failed to persist rules: {e}");
                        }
                    }
                    Ok(Command::SetOtherLimits { down_bps, up_bps }) => {
                        {
                            let mut other = shared.other.lock().unwrap();
                            other.down_bps = down_bps;
                            other.up_bps = up_bps;
                        }
                        // Release anything the fallback bucket is holding so the
                        // new limits (or their removal) apply at once.
                        shared
                            .buckets
                            .lock()
                            .unwrap()
                            .retain(|(exe, _), _| exe != OTHER_EXE);
                        if let Err(e) = rules::save(&rules::persisted_from(
                            &shared.rules.lock().unwrap().clone(),
                            shared.master.load(Ordering::Relaxed),
                            &shared.other.lock().unwrap(),
                        )) {
                            tracing::warn!("failed to persist rules: {e}");
                        }
                    }
                    Ok(Command::Shutdown) | Err(_) => {
                        // Explicit shutdown, or the GUI dropped the command sender.
                        tracing::info!("aggregator: shutting down");
                        shared.running.store(false, Ordering::Relaxed);
                        let persisted = rules::persisted_from(
                            &shared.rules.lock().unwrap().clone(),
                            shared.master.load(Ordering::Relaxed),
                            &shared.other.lock().unwrap(),
                        );
                        if let Err(e) = rules::save(&persisted) {
                            tracing::warn!("failed to persist rules on shutdown: {e}");
                        }
                        break;
                    }
                }
            }
            recv(refresh) -> _ => {
                // Re-read the TCP/UDP owner tables: sockets created after the
                // bootstrap (or never announced via a usable flow event) become
                // attributable here. The four Win32 queries and any PID
                // resolution they trigger run with no lock held - the engine
                // locks `flows` on the packet hot path - and only the finished
                // snapshot is applied under the lock.
                let known = shared.flows.lock().unwrap().known_pids();
                let snap = sockets::query_socket_tables(&known);
                shared.diag.tcp4.store(snap.counts.tcp4, Ordering::Relaxed);
                shared.diag.udp4.store(snap.counts.udp4, Ordering::Relaxed);
                shared.diag.tcp6.store(snap.counts.tcp6, Ordering::Relaxed);
                shared.diag.udp6.store(snap.counts.udp6, Ordering::Relaxed);
                shared.flows.lock().unwrap().apply_socket_snapshot(snap);
            }
            recv(ticker) -> _ => {
                let (processes, total_down_rate, total_up_rate) =
                    shared.flows.lock().unwrap().snapshot();
                let rules = shared.rules.lock().unwrap().clone();
                let other = *shared.other.lock().unwrap();
                let snapshot = Snapshot {
                    processes,
                    total_down_rate,
                    total_up_rate,
                    rules,
                    master: shared.master.load(Ordering::Relaxed),
                    other_down_bps: other.down_bps,
                    other_up_bps: other.up_bps,
                    diagnostics: shared.diag.snapshot(),
                };
                if snapshot_tx.send(snapshot).is_err() {
                    // GUI receiver gone: nothing left to feed.
                    tracing::info!("aggregator: snapshot receiver dropped, stopping");
                    shared.running.store(false, Ordering::Relaxed);
                    break;
                }
            }
        }
    }
    tracing::info!("aggregator thread exiting");
}
