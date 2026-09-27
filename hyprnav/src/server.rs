use crate::db::{
    environment_chain, environment_has_parent, EnvironmentRecord, SlotBindingRecord, StateStore,
    StickRecord, TempSlotMeta, TEMP_SLOT_START,
};

/// A temporary slot whose workspace has been empty this long is released.
const TEMP_SLOT_GRACE_SECS: i64 = 30;
use crate::protocol::{
    read_request, write_response, AgentSnapshot, BatchMutationOperationResult, BatchMutationRequest,
    BatchMutationResponse, GridCellSnapshot, GridChainLevel, GridSnapshot, NavigationLaunchResult,
    NavigationLaunchSkippedReason, Request, Response, SlotAssignmentMode, SlotResolution,
    SpawnPrepared, SpawnStarted, StatusSnapshot, SwitcherSnapshot, WorkspaceCardSnapshot,
    WorkspaceNavigationResult,
};
use crate::events::{start_event_server, EventBus};

use crate::runtime_paths::{
    append_switch_log, ensure_parent_dir, resolve_runtime_paths, RuntimePaths,
};
use crate::spawn::{
    now_ms, parse_spawn_focus_policy, parse_spawn_target, SpawnFocusPolicy, SpawnOperationState,
    SpawnOriginSnapshot, SpawnRegistry,
};
use crate::workspace_utils::{build_workspace_descriptors, initial_selection_index};
use anyhow::{anyhow, Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tracing::{debug, warn};

const LAUNCH_PENDING_TTL_MS: u64 = 30_000;

#[derive(Clone, Debug)]
struct WorkspaceCardData {
    workspace_id: i32,
    slot_index: i32,
    slot_display_name: String,
    workspace_name: String,
    subtitle: String,
    app_class: String,
    window_count: i32,
    active: bool,
}

#[derive(Debug, Deserialize)]
struct WorkspaceInfo {
    #[serde(default)]
    id: i32,
}

#[derive(Debug, Default, Deserialize)]
struct WorkspaceRef {
    #[serde(default)]
    id: i32,
}

#[derive(Debug, Deserialize)]
struct MonitorInfo {
    #[serde(default)]
    id: i32,
    #[serde(default)]
    focused: bool,
    #[serde(default, rename = "activeWorkspace")]
    active_workspace: WorkspaceRef,
}

#[derive(Debug, Default, Deserialize)]
struct ActiveWindowInfo {
    #[serde(default)]
    address: String,
}

#[derive(Debug, Default, Deserialize)]
struct ClientInfo {
    #[serde(default)]
    mapped: bool,
    #[serde(default)]
    workspace: WorkspaceRef,
}

#[derive(Debug)]
struct ServerRuntime {
    paths: RuntimePaths,
    store: StateStore,
    spawn_registry: Mutex<SpawnRegistry>,
    pending_launches: Mutex<PendingLaunchRegistry>,
    /// Plugin instance id that last received a full stick replay.
    plugin_instance: Mutex<Option<String>>,
    /// Live agents, by agent id. In memory only; their slots persist.
    agents: Mutex<HashMap<String, AgentSnapshot>>,
    /// Push notifications for agent and slot state.
    events: EventBus,
}

impl ServerRuntime {
    /// The `agents` event payload: the same JSON `agents_list` returns.
    fn agents_snapshot(&self) -> serde_json::Value {
        let Ok(agents) = self.agents.lock() else {
            return json!([]);
        };
        let mut list: Vec<&AgentSnapshot> = agents.values().collect();
        list.sort_by_key(|agent| agent.created_at_ms);
        serde_json::to_value(list).unwrap_or_else(|_| json!([]))
    }
}

/// Finished agents are forgotten after this long.
const AGENT_FINISHED_TTL_MS: u64 = 60_000;

fn agent_state_valid(state: &str) -> bool {
    matches!(state, "working" | "waiting_for_user" | "idle" | "finished")
}

/// Mark agents whose process is gone as finished, forget old finished ones.
fn reap_agents(runtime: &ServerRuntime) {
    let changed = {
        let Ok(mut agents) = runtime.agents.lock() else { return };
        let now = now_ms();
        let mut changed = false;
        agents.retain(|_, agent| {
            if agent.state != "finished" && !crate::spawn::pid_exists(agent.pid) {
                agent.state = "finished".to_owned();
                agent.last_beat_ms = now;
                changed = true;
            }
            let keep = !(agent.state == "finished"
                && now.saturating_sub(agent.last_beat_ms) > AGENT_FINISHED_TTL_MS);
            changed |= !keep;
            keep
        });
        changed
    };
    if changed {
        runtime.events.agents_changed();
    }
}

#[derive(Debug, Deserialize)]
struct PluginSpawnResponse {
    ok: bool,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<PluginSpawnError>,
}

#[derive(Debug, Deserialize, Default)]
struct PluginSyncResult {
    #[serde(default)]
    instance: String,
    #[serde(default)]
    active: Vec<String>,
    #[serde(default)]
    dropped: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PluginSpawnError {
    message: String,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct LaunchSlotKey {
    environment_id: String,
    slot_index: i32,
}

#[derive(Debug, Default)]
struct PendingLaunchRegistry {
    entries: HashMap<LaunchSlotKey, PendingLaunchEntry>,
}

#[derive(Debug)]
struct PendingLaunchEntry {
    started_at_ms: u64,
    workspace_id: i32,
}

impl PendingLaunchRegistry {
    fn purge_expired(&mut self, now_ms: u64) {
        self.entries
            .retain(|_, entry| now_ms.saturating_sub(entry.started_at_ms) < LAUNCH_PENDING_TTL_MS);
    }

    fn contains(&self, key: &LaunchSlotKey) -> bool {
        self.entries.contains_key(key)
    }

    fn insert(&mut self, key: LaunchSlotKey, workspace_id: i32, now_ms: u64) {
        self.entries.insert(
            key,
            PendingLaunchEntry {
                started_at_ms: now_ms,
                workspace_id,
            },
        );
    }

    fn remove(&mut self, key: &LaunchSlotKey) {
        self.entries.remove(key);
    }

    fn has_entries(&self) -> bool {
        !self.entries.is_empty()
    }

    fn keys_for_workspaces(&self, workspace_ids: &HashSet<i32>) -> Vec<LaunchSlotKey> {
        self.entries
            .iter()
            .filter(|(_, entry)| workspace_ids.contains(&entry.workspace_id))
            .map(|(key, _)| key.clone())
            .collect()
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum PluginSpawnRequest<'a> {
    Ping,
    Sync,
    List,
    Stick {
        stick_id: &'a str,
        workspace_id: i32,
        root_pid: u32,
        target_monitor_id: i32,
        focus_policy: &'a str,
        origin_monitor_id: i32,
        origin_workspace_id: i32,
        #[serde(skip_serializing_if = "Option::is_none")]
        origin_window_address: Option<&'a str>,
        replay: i32,
    },
    Unstick {
        stick_id: &'a str,
    },
    Move {
        stick_id: &'a str,
        workspace_id: i32,
    },
}

fn stick_request<'a>(record: &'a StickRecord, replay: bool) -> PluginSpawnRequest<'a> {
    PluginSpawnRequest::Stick {
        stick_id: &record.stick_id,
        workspace_id: record.workspace_id,
        root_pid: record.root_pid,
        target_monitor_id: record.monitor_id,
        focus_policy: &record.focus_policy,
        origin_monitor_id: record.origin_monitor_id,
        origin_workspace_id: record.origin_workspace_id,
        origin_window_address: record.origin_window_address.as_deref(),
        replay: if replay { 1 } else { 0 },
    }
}

/// Replay every persisted stick into a freshly (re)loaded plugin, then
/// reconcile: rows the plugin has released are deleted.
fn sync_sticks_with_plugin(runtime: &Arc<ServerRuntime>) {
    let result = match send_plugin_spawn_value(&runtime.paths, &PluginSpawnRequest::Sync) {
        Ok(value) => serde_json::from_value::<PluginSyncResult>(value).unwrap_or_default(),
        Err(_) => return, // plugin not loaded
    };
    let mut known = match runtime.plugin_instance.lock() {
        Ok(guard) => guard,
        Err(_) => return,
    };
    if known.as_deref() != Some(result.instance.as_str()) {
        let records = runtime.store.list_sticks().unwrap_or_default();
        debug!(instance = %result.instance, count = records.len(), "replaying sticks into plugin");
        for record in &records {
            if let Err(error) = send_plugin_spawn_request(&runtime.paths, &stick_request(record, true)) {
                warn!("stick replay failed for {}: {error}", record.stick_id);
            }
        }
        *known = Some(result.instance);
        return;
    }
    for id in &result.dropped {
        let _ = runtime.store.delete_stick(id);
    }
    if let Ok(records) = runtime.store.list_sticks() {
        let active: HashSet<&str> = result.active.iter().map(String::as_str).collect();
        for record in records {
            if !active.contains(record.stick_id.as_str()) && !crate::spawn::pid_exists(record.root_pid) {
                let _ = runtime.store.delete_stick(&record.stick_id);
            }
        }
    }
}

pub fn run_server() -> Result<()> {
    let paths = resolve_runtime_paths();
    let runtime = Arc::new(ServerRuntime {
        store: StateStore::new(paths.state_db_path.clone())?,
        paths,
        spawn_registry: Mutex::new(SpawnRegistry::new()),
        pending_launches: Mutex::new(PendingLaunchRegistry::default()),
        plugin_instance: Mutex::new(None),
        agents: Mutex::new(HashMap::new()),
        events: EventBus::new(),
    });
    let listener = bind_listener(&runtime.paths.server_socket_path)?;
    let events_listener = bind_listener(&runtime.paths.events_socket_path)?;
    let frames_listener = bind_listener(&runtime.paths.frames_socket_path)?;
    {
        let snapshot_runtime = runtime.clone();
        let lock_runtime = runtime.clone();
        start_event_server(
            runtime.events.clone(),
            events_listener,
            move || snapshot_runtime.agents_snapshot(),
            move || {
                let locked = lock_runtime.store.locked_environment().ok().flatten();
                let environment = lock_environment_json(&lock_runtime.store, locked.as_deref());
                (locked, environment)
            },
        );
    }
    // Frame streaming lives in a `hyprnav-capture` child: the daemon only
    // brokers clients and fans its JPEGs out, so it needs no Wayland or JPEG
    // crates of its own.
    crate::frames::start(
        frames_listener,
        runtime.paths.instance_signature.clone(),
        &runtime.paths.spawn_socket_path,
    );
    start_spawn_cleanup_thread(runtime.clone());
    start_stick_sync_thread(runtime.clone());
    start_hypr_event_thread(runtime.clone());

    loop {
        let (stream, _) = listener.accept().context("accepting server connection")?;
        handle_stream(stream, runtime.clone());
    }
}

fn start_hypr_event_thread(runtime: Arc<ServerRuntime>) {
    thread::spawn(move || {
        let mut previous_workspace_id = current_active_workspace_id(&runtime.paths).unwrap_or(-1);

        loop {
            match UnixStream::connect(&runtime.paths.hypr_event_socket_path) {
                Ok(stream) => {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();

                    loop {
                        line.clear();
                        match reader.read_line(&mut line) {
                            Ok(0) => break,
                            Ok(_) => {
                                let trimmed = line.trim();
                                let Some((event_name, payload)) = trimmed.split_once(">>") else {
                                    continue;
                                };

                                if !matches!(event_name, "workspacev2" | "focusedmonv2") {
                                    continue;
                                }

                                let next_workspace_id = payload
                                    .split(',')
                                    .find_map(|segment| segment.trim().parse::<i32>().ok())
                                    .unwrap_or(-1);

                                if next_workspace_id > 0
                                    && next_workspace_id != previous_workspace_id
                                {
                                    previous_workspace_id = next_workspace_id;
                                    record_workspace_focus(&runtime, next_workspace_id);
                                }
                            }
                            Err(error) => {
                                warn!("hypr event socket read failed: {error}");
                                break;
                            }
                        }
                    }
                }
                Err(error) => warn!("failed to connect to hypr event socket: {error}"),
            }

            thread::sleep(Duration::from_secs(1));
        }
    });
}

/// Hyprland focused `workspace_id`: move the lock to its concrete owner, if it
/// has exactly one. Ambiguous or unbound workspaces leave the lock alone.
fn record_workspace_focus(runtime: &ServerRuntime, workspace_id: i32) {
    let Ok(Some(environment_id)) =
        resolve_focus_environment_for_physical_workspace(&runtime.store, workspace_id)
    else {
        return;
    };
    let previous = runtime.store.locked_environment();
    match runtime.store.record_environment_focus(&environment_id) {
        Ok(()) => {
            if let Ok(previous) = previous {
                if previous.as_deref() != Some(environment_id.as_str()) {
                    announce_lock_change(
                        runtime,
                        Some(&environment_id),
                        previous.as_deref(),
                        "focus",
                        None,
                    );
                }
            }
            // Row order in the grid follows focus, so this is a slot change.
            runtime.events.slots_changed();
        }
        Err(error) => warn!(
            "failed to record environment focus for {}: {error}",
            environment_id
        ),
    }
}

fn announce_lock_change(
    runtime: &ServerRuntime,
    locked: Option<&str>,
    previous: Option<&str>,
    cause: &str,
    origin: Option<&str>,
) {
    runtime
        .events
        .lock_changed(locked, previous, cause, origin, || {
            lock_environment_json(&runtime.store, locked)
        });
}

/// The `environment` object of a `locked` event; null when nothing is locked.
/// `title` is the environment's own, `cwd` the nearest source path up its
/// chain (a thread env has none, its worktree does), `chain` the existing
/// levels root first, as in the grid.
fn lock_environment_json(store: &StateStore, locked: Option<&str>) -> serde_json::Value {
    let Some(env_id) = locked else {
        return serde_json::Value::Null;
    };
    let levels = store.environment_levels(env_id).unwrap_or_default();
    let title = levels
        .last()
        .filter(|level| level.env_id == env_id)
        .and_then(|level| level.title.as_deref());
    let cwd = levels
        .iter()
        .rev()
        .find_map(|level| level.source_path.as_deref());
    let chain: Vec<&str> = levels.iter().map(|level| level.env_id.as_str()).collect();
    json!({"title": title, "cwd": cwd, "chain": chain})
}

fn bind_listener(path: &Path) -> Result<UnixListener> {
    ensure_parent_dir(path)?;
    if path.exists() {
        let _ = fs::remove_file(path);
    }

    UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))
}

fn start_stick_sync_thread(runtime: Arc<ServerRuntime>) {
    thread::spawn(move || loop {
        sync_sticks_with_plugin(&runtime);
        thread::sleep(Duration::from_millis(2000));
    });
}

/// Release temporary slots whose workspace has been empty for the grace period.
fn reap_temp_slots(runtime: &ServerRuntime) {
    let temps = match runtime.store.list_temp_slots() {
        Ok(temps) if !temps.is_empty() => temps,
        _ => return,
    };
    let occupied = match mapped_workspace_ids(&runtime.paths) {
        Ok(ids) => ids,
        Err(error) => {
            warn!("temp slot reaper: cannot list clients: {error}");
            return;
        }
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    for temp in temps {
        let has_windows = temp
            .workspace_id
            .map(|id| occupied.contains(&id))
            .unwrap_or(false);
        match (has_windows, temp.empty_since) {
            (true, Some(_)) => {
                let _ = runtime
                    .store
                    .set_temp_slot_empty_since(&temp.env_id, temp.slot_index, None);
            }
            (false, None) => {
                let _ = runtime
                    .store
                    .set_temp_slot_empty_since(&temp.env_id, temp.slot_index, Some(now));
            }
            (false, Some(since)) if now - since >= TEMP_SLOT_GRACE_SECS => {
                debug!(env = %temp.env_id, slot = temp.slot_index, "releasing temporary slot");
                match runtime.store.clear_slot(&temp.env_id, temp.slot_index) {
                    Ok(_) => runtime.events.slots_changed(),
                    Err(error) => warn!("temp slot reaper: clear failed: {error}"),
                }
            }
            _ => {}
        }
    }
}

fn start_spawn_cleanup_thread(runtime: Arc<ServerRuntime>) {
    thread::spawn(move || {
        let mut tick: u32 = 0;
        loop {
        thread::sleep(Duration::from_millis(250));
        tick = tick.wrapping_add(1);
        if tick % 8 == 0 {
            reap_temp_slots(&runtime);
            reap_agents(&runtime);
        }

        let expired = {
            let registry = match runtime.spawn_registry.lock() {
                Ok(registry) => registry,
                Err(error) => {
                    warn!("spawn registry poisoned: {error}");
                    continue;
                }
            };
            registry.expired_operations(now_ms())
        };

        if !expired.is_empty() {
            let ids = expired
                .iter()
                .map(|operation| operation.operation_id.clone())
                .collect::<HashSet<_>>();
            let removed = {
                let mut registry = match runtime.spawn_registry.lock() {
                    Ok(registry) => registry,
                    Err(error) => {
                        warn!("spawn registry poisoned during cleanup: {error}");
                        continue;
                    }
                };
                registry.remove_many(&ids)
            };

            for operation in removed {
                if operation.state == SpawnOperationState::Active && !operation.stick {
                    let _ = send_plugin_spawn_request(
                        &runtime.paths,
                        &PluginSpawnRequest::Unstick {
                            stick_id: &operation.operation_id,
                        },
                    );
                }
            }
        }

        let has_pending_launches = match runtime.pending_launches.lock() {
            Ok(pending) => pending.has_entries(),
            Err(error) => {
                warn!("pending launch registry poisoned: {error}");
                false
            }
        };
        if !has_pending_launches {
            continue;
        }

        let occupied_workspace_ids = match mapped_workspace_ids(&runtime.paths) {
            Ok(ids) => ids,
            Err(error) => {
                warn!("failed to inspect mapped clients for pending launches: {error}");
                continue;
            }
        };
        if occupied_workspace_ids.is_empty() {
            continue;
        }

        let keys_to_clear = match runtime.pending_launches.lock() {
            Ok(pending) => pending.keys_for_workspaces(&occupied_workspace_ids),
            Err(error) => {
                warn!("pending launch registry poisoned during cleanup: {error}");
                continue;
            }
        };
        if keys_to_clear.is_empty() {
            continue;
        }

        let mut pending = match runtime.pending_launches.lock() {
            Ok(pending) => pending,
            Err(error) => {
                warn!("pending launch registry poisoned during removal: {error}");
                continue;
            }
        };
        for key in keys_to_clear {
            pending.remove(&key);
        }
    }});
}

fn handle_stream(stream: UnixStream, runtime: Arc<ServerRuntime>) {
    let clone = match stream.try_clone() {
        Ok(stream) => stream,
        Err(error) => {
            warn!("failed to clone server stream: {error}");
            return;
        }
    };

    let mut reader = BufReader::new(clone);
    let mut writer = stream;
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return,
            Ok(_) => {
                let response = match read_request(line.trim()) {
                    Ok(request) => handle_request(&runtime, request),
                    Err(error) => Response::error("invalid_request", error.to_string()),
                };

                if let Err(error) = write_response(&mut writer, &response) {
                    warn!("failed to write response: {error}");
                    return;
                }
            }
            Err(error) => {
                warn!("failed to read request: {error}");
                return;
            }
        }
    }
}

fn handle_request(
    runtime: &Arc<ServerRuntime>,
    mut request: Request,
) -> Response<serde_json::Value> {
    // Classify before the request is consumed: every mutating op announces
    // itself on the event bus once it succeeded, so subscribers never poll.
    let effects = request_effects(&request);
    // Lock watch: only the ops that can move the lock pay for a read before
    // and after. Checked on failure too, a request may fail after moving it.
    let lock_watch = lock_cause(&mut request)
        .map(|(cause, origin)| (cause, origin, runtime.store.locked_environment()));
    let result = try_handle_request(runtime, request);
    if let Some((cause, origin, Ok(previous))) = lock_watch {
        if let Ok(locked) = runtime.store.locked_environment() {
            if locked != previous {
                announce_lock_change(
                    runtime,
                    locked.as_deref(),
                    previous.as_deref(),
                    cause,
                    origin.as_deref(),
                );
            }
        }
    }
    match result {
        Ok(value) => {
            match effects {
                RequestEffects { agents: true, slots: true } => runtime.events.both_changed(),
                RequestEffects { agents: true, slots: false } => runtime.events.agents_changed(),
                RequestEffects { agents: false, slots: true } => runtime.events.slots_changed(),
                _ => {}
            }
            Response::ok(value)
        }
        Err(error) => Response::error("request_failed", error.to_string()),
    }
}

/// For requests that can move the lock: the `locked` event cause, and the
/// request's `origin` tag (taken out, the handlers do not need it).
fn lock_cause(request: &mut Request) -> Option<(&'static str, Option<String>)> {
    match request {
        Request::LockSet { origin, .. } => Some(("lock_set", origin.take())),
        Request::LockClear { origin } => Some(("lock_clear", origin.take())),
        Request::WorkspaceGoto { origin, .. } => Some(("workspace_goto", origin.take())),
        Request::WorkspaceGotoPhysical { origin, .. } => {
            Some(("workspace_goto_physical", origin.take()))
        }
        Request::BatchMutate { origin, .. } => Some(("batch_mutate", origin.take())),
        Request::EnvDelete { .. } => Some(("env_delete", None)),
        _ => None,
    }
}

/// Which event streams a request touches when it succeeds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RequestEffects {
    agents: bool,
    slots: bool,
}

impl RequestEffects {
    const NONE: Self = Self { agents: false, slots: false };
    const SLOTS: Self = Self { agents: false, slots: true };
    const AGENTS: Self = Self { agents: true, slots: false };
    const BOTH: Self = Self { agents: true, slots: true };
}

fn request_effects(request: &Request) -> RequestEffects {
    match request {
        // Agent registry only.
        Request::AgentBeat { .. } | Request::AgentFinish { .. } => RequestEffects::AGENTS,
        // Registering an agent also carves out its temporary slot; labelling
        // one renames that slot.
        Request::AgentRegister { .. } | Request::AgentLabel { .. } => RequestEffects::BOTH,
        // Anything that can change `ui_snapshot_grid` output.
        Request::EnvEnsure { .. }
        | Request::EnvDelete { .. }
        | Request::EnvTitleSet { .. }
        | Request::EnvTitleClear { .. }
        | Request::ClientEnsure { .. }
        | Request::SlotAssign { .. }
        | Request::SlotClear { .. }
        | Request::SlotCommandSet { .. }
        | Request::SlotCommandClear { .. }
        | Request::SlotNameSet { .. }
        | Request::SlotNameClear { .. }
        | Request::SlotTempCreate { .. }
        | Request::SlotRemove { .. }
        | Request::LockSet { .. }
        | Request::LockClear { .. }
        | Request::BrowserSlotSet { .. }
        | Request::BrowserSlotClear { .. }
        | Request::WorkspaceGoto { .. }
        | Request::WorkspaceGotoPhysical { .. }
        | Request::SpawnStart { .. }
        | Request::SpawnFinish { .. }
        | Request::StickAdd { .. }
        | Request::StickRelease { .. }
        | Request::StickMove { .. }
        | Request::BatchMutate { .. } => RequestEffects::SLOTS,
        // Reads, and spawn preparation, which only reserves an id in memory.
        Request::Ping
        | Request::StatusGet { .. }
        | Request::SlotResolve { .. }
        | Request::SlotTempList
        | Request::StickList
        | Request::AgentsList
        | Request::SpawnPrepare { .. }
        | Request::WorkspaceRun { .. }
        | Request::UiSnapshotSwitcher { .. }
        | Request::UiSnapshotGrid { .. } => RequestEffects::NONE,
    }
}

fn try_handle_request(runtime: &Arc<ServerRuntime>, request: Request) -> Result<serde_json::Value> {
    match request {
        Request::Ping => Ok(json!({"pong": true})),
        Request::StatusGet { cwd } => Ok(serde_json::to_value(StatusSnapshot {
            locked_environment_id: runtime.store.locked_environment()?,
            current_environment_id: cwd
                .as_deref()
                .map(resolve_environment_from_cwd)
                .transpose()?
                .filter(|value| !value.is_empty()),
            sticks: runtime.store.list_sticks().map(|s| s.len()).unwrap_or(0),
        })?),
        Request::EnvEnsure {
            env,
            cwd,
            client,
            title,
        } => apply_mutation_request(
            runtime,
            BatchMutationRequest::EnvEnsure {
                env,
                cwd,
                client,
                title,
            },
        ),
        Request::EnvDelete { env } => {
            apply_mutation_request(runtime, BatchMutationRequest::EnvDelete { env })
        }
        Request::EnvTitleSet { env, title } => {
            apply_mutation_request(runtime, BatchMutationRequest::EnvTitleSet { env, title })
        }
        Request::EnvTitleClear { env } => {
            apply_mutation_request(runtime, BatchMutationRequest::EnvTitleClear { env })
        }
        Request::ClientEnsure { client } => {
            apply_mutation_request(runtime, BatchMutationRequest::ClientEnsure { client })
        }
        Request::SlotAssign {
            env,
            slot,
            assignment_mode,
            client,
            cwd,
            launch_argv,
            display_name,
        } => apply_mutation_request(
            runtime,
            BatchMutationRequest::SlotAssign {
                env,
                slot,
                assignment_mode,
                client,
                cwd,
                launch_argv,
                display_name,
            },
        ),
        Request::SlotClear { env, slot, client } => apply_mutation_request(
            runtime,
            BatchMutationRequest::SlotClear { env, slot, client },
        ),
        Request::SlotCommandSet {
            env,
            slot,
            argv,
            display_name,
        } => apply_mutation_request(
            runtime,
            BatchMutationRequest::SlotCommandSet {
                env,
                slot,
                argv,
                display_name,
            },
        ),
        Request::SlotCommandClear { env, slot } => apply_mutation_request(
            runtime,
            BatchMutationRequest::SlotCommandClear { env, slot },
        ),
        Request::SlotNameSet { env, slot, name } => apply_mutation_request(
            runtime,
            BatchMutationRequest::SlotNameSet { env, slot, name },
        ),
        Request::SlotNameClear { env, slot } => {
            apply_mutation_request(runtime, BatchMutationRequest::SlotNameClear { env, slot })
        }
        Request::SlotResolve { env, slot } => {
            ensure_positive_slot(slot)?;
            let resolved_env = resolve_required_environment(env.as_deref(), &runtime.store)?;
            let record = runtime
                .store
                .resolve_slot_effective(&resolved_env, slot)?
                .ok_or_else(|| {
                    anyhow!("slot {slot} is not assigned for environment {resolved_env}")
                })?;
            let mut result = serde_json::to_value(slot_resolution_from_record(record))?;
            result["browser_target"] =
                serde_json::to_value(runtime.store.browser_target(&resolved_env, slot)?)?;
            Ok(result)
        }
        Request::LockSet { env, .. } => {
            apply_mutation_request(runtime, BatchMutationRequest::LockSet { env })
        }
        Request::LockClear { .. } => {
            apply_mutation_request(runtime, BatchMutationRequest::LockClear)
        }
        Request::BrowserSlotSet { env, slot, target } => {
            ensure_positive_slot(slot)?;
            let env = resolve_required_environment(env.as_deref(), &runtime.store)?;
            runtime
                .store
                .set_browser_target(&env, slot, Some(&target))?;
            Ok(json!({"environment_id":env, "slot_index":slot, "browser_target":target}))
        }
        Request::BrowserSlotClear { env, slot } => {
            ensure_positive_slot(slot)?;
            let env = resolve_required_environment(env.as_deref(), &runtime.store)?;
            runtime.store.set_browser_target(&env, slot, None)?;
            Ok(json!({"environment_id":env, "slot_index":slot}))
        }
        Request::WorkspaceGoto { env, slot, .. } => {
            ensure_positive_slot(slot)?;
            let resolved_env = resolve_required_environment(env.as_deref(), &runtime.store)?;
            let record = runtime
                .store
                .resolve_slot_effective(&resolved_env, slot)?
                .ok_or_else(|| {
                    anyhow!("slot {slot} is not assigned for environment {resolved_env}")
                })?;
            debug!(
                requested_env = ?env,
                resolved_env,
                slot,
                workspace_id = record.workspace_id,
                "workspace goto resolved"
            );
            append_switch_log(
                "server.goto.slot",
                format!(
                    "requested_env={:?} resolved_env={} slot={} workspace_id={}",
                    env, resolved_env, slot, record.workspace_id
                ),
            );
            goto_workspace(&runtime.paths, record.workspace_id)?;
            runtime.store.record_environment_focus(&resolved_env)?;
            let launch = attempt_slot_launch(runtime, &record)?;
            debug!(
                workspace_id = record.workspace_id,
                launch_configured = launch.configured,
                launch_attempted = launch.attempted,
                launch_skipped = ?launch.skipped_reason,
                launch_error = ?launch.error,
                "workspace goto completed"
            );
            append_switch_log(
                "server.goto.slot.result",
                format!(
                    "workspace_id={} launch_configured={} launch_attempted={} launch_skipped={:?} launch_error={:?}",
                    record.workspace_id,
                    launch.configured,
                    launch.attempted,
                    launch.skipped_reason,
                    launch.error
                ),
            );
            Ok(serde_json::to_value(WorkspaceNavigationResult {
                workspace_id: record.workspace_id,
                slot_resolution: Some(slot_resolution_from_record(record)),
                launch,
            })?)
        }
        Request::WorkspaceGotoPhysical { workspace_id, .. } => {
            debug!(workspace_id, "physical workspace goto requested");
            append_switch_log(
                "server.goto.physical",
                format!("workspace_id={workspace_id}"),
            );
            goto_workspace(&runtime.paths, workspace_id)?;
            let locked_environment_id = runtime.store.locked_environment()?;
            let (record, skipped_reason) = resolve_slot_for_physical_workspace(
                runtime,
                workspace_id,
                locked_environment_id.as_deref(),
            )?;
            if let Some(environment_id) =
                resolve_focus_environment_for_physical_workspace(&runtime.store, workspace_id)?
            {
                runtime.store.record_environment_focus(&environment_id)?;
            }
            let launch = if let Some(record) = record.as_ref() {
                attempt_slot_launch(runtime, record)?
            } else {
                NavigationLaunchResult {
                    configured: false,
                    attempted: false,
                    skipped_reason: Some(
                        skipped_reason
                            .clone()
                            .unwrap_or(NavigationLaunchSkippedReason::NoSlotMapping),
                    ),
                    error: None,
                }
            };
            debug!(
                workspace_id,
                locked_environment_id,
                resolved_env = ?record.as_ref().map(|record| record.environment_id.as_str()),
                resolved_slot = ?record.as_ref().map(|record| record.slot_index),
                binding_kind = ?record.as_ref().map(|record| record.binding_kind.as_str()),
                skipped_reason = ?skipped_reason,
                launch_configured = launch.configured,
                launch_attempted = launch.attempted,
                launch_skipped = ?launch.skipped_reason,
                launch_error = ?launch.error,
                "physical workspace goto completed"
            );
            append_switch_log(
                "server.goto.physical.result",
                format!(
                    "workspace_id={workspace_id} locked_env={:?} resolved_env={:?} resolved_slot={:?} binding_kind={:?} skipped_reason={:?} launch_configured={} launch_attempted={} launch_skipped={:?} launch_error={:?}",
                    locked_environment_id,
                    record.as_ref().map(|record| record.environment_id.as_str()),
                    record.as_ref().map(|record| record.slot_index),
                    record.as_ref().map(|record| record.binding_kind.as_str()),
                    skipped_reason,
                    launch.configured,
                    launch.attempted,
                    launch.skipped_reason,
                    launch.error
                ),
            );
            Ok(serde_json::to_value(WorkspaceNavigationResult {
                workspace_id,
                slot_resolution: record.map(slot_resolution_from_record),
                launch,
            })?)
        }
        Request::WorkspaceRun { env, slot, argv } => {
            ensure_positive_slot(slot)?;
            if argv.is_empty() {
                return Err(anyhow!("run requires a command"));
            }

            let resolved_env = resolve_required_environment(env.as_deref(), &runtime.store)?;
            let record = runtime
                .store
                .resolve_slot_effective(&resolved_env, slot)?
                .ok_or_else(|| {
                    anyhow!("slot {slot} is not assigned for environment {resolved_env}")
                })?;
            run_in_workspace(&runtime.paths, record.workspace_id, &argv)?;
            Ok(json!({
                "environment_id": record.environment_id,
                "binding_environment_id": record.binding_environment_id,
                "command_environment_id": record.command_environment_id,
                "slot_index": record.slot_index,
                "physical_workspace_id": record.workspace_id,
                "binding_kind": record.binding_kind.as_str(),
                "launch_argv": record.launch_argv,
                "argv": argv,
            }))
        }
        Request::SpawnPrepare {
            target,
            focus_policy,
            no_stick,
        } => {
            let target = parse_spawn_target(&target)?;
            let focus_policy = parse_spawn_focus_policy(&focus_policy)?;
            let target_monitor_id = current_focused_monitor_id(&runtime.paths)?;
            let origin = current_spawn_origin_snapshot(&runtime.paths)?;
            let live_workspace_ids = live_workspace_ids(&runtime.paths)?;
            let operation = {
                let mut registry = runtime
                    .spawn_registry
                    .lock()
                    .map_err(|error| anyhow!("spawn registry poisoned: {error}"))?;
                registry.prepare(
                    target,
                    target_monitor_id,
                    focus_policy,
                    origin,
                    &runtime.store,
                    &live_workspace_ids,
                    !no_stick,
                )?
            };
            Ok(serde_json::to_value(SpawnPrepared {
                operation_id: operation.operation_id,
                workspace_id: operation.workspace_id,
                temporary: operation.temporary,
                origin_monitor_id: operation.origin_monitor_id,
                origin_workspace_id: operation.origin_workspace_id,
                origin_window_address: operation.origin_window_address,
            })?)
        }
        Request::SpawnStart {
            operation_id,
            root_pid,
        } => {
            let operation = {
                let mut registry = runtime
                    .spawn_registry
                    .lock()
                    .map_err(|error| anyhow!("spawn registry poisoned: {error}"))?;
                registry.activate(&operation_id, root_pid)?
            };

            let record = StickRecord {
                stick_id: operation.operation_id.clone(),
                workspace_id: operation.workspace_id,
                monitor_id: operation.target_monitor_id,
                root_pid,
                focus_policy: match operation.focus_policy {
                    SpawnFocusPolicy::Follow => "follow".to_owned(),
                    SpawnFocusPolicy::Preserve => "preserve".to_owned(),
                },
                origin_monitor_id: operation.origin_monitor_id,
                origin_workspace_id: operation.origin_workspace_id,
                origin_window_address: operation.origin_window_address.clone(),
                created_at: 0,
            };
            if operation.stick {
                runtime.store.insert_stick(&record)?;
            }
            if let Err(error) = send_plugin_spawn_request(&runtime.paths, &stick_request(&record, false)) {
                let mut registry = runtime
                    .spawn_registry
                    .lock()
                    .map_err(|poison| anyhow!("spawn registry poisoned: {poison}"))?;
                let _ = registry.finish(&operation_id);
                if operation.stick {
                    let _ = runtime.store.delete_stick(&record.stick_id);
                }
                return Err(error);
            }

            Ok(serde_json::to_value(SpawnStarted {
                operation_id: operation.operation_id,
                workspace_id: operation.workspace_id,
                root_pid,
            })?)
        }
        Request::SpawnFinish { operation_id } => {
            if let Some(operation) = runtime
                .spawn_registry
                .lock()
                .map_err(|error| anyhow!("spawn registry poisoned: {error}"))?
                .finish(&operation_id)
            {
                if operation.state == SpawnOperationState::Active && !operation.stick {
                    let _ = send_plugin_spawn_request(
                        &runtime.paths,
                        &PluginSpawnRequest::Unstick {
                            stick_id: &operation.operation_id,
                        },
                    );
                }
            }

            Ok(json!({"operation_id": operation_id, "finished": true}))
        }
        Request::AgentRegister {
            agent_id,
            label,
            client,
            pid,
            cwd,
            env,
            thread_id,
            thread_environment_id,
        } => {
            if agent_id.trim().is_empty() {
                return Err(anyhow!("agent_id is required"));
            }
            let label = label
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| format!("agent {pid}"));
            let client = client.unwrap_or_else(|| "cua".to_owned());
            // Re-registration (same id) keeps the slot; thread attribution is refreshed.
            if let Ok(mut agents) = runtime.agents.lock() {
                if let Some(existing) = agents.get_mut(&agent_id) {
                    if thread_id.is_some() {
                        existing.thread_id = thread_id.clone();
                    }
                    if thread_environment_id.is_some() {
                        existing.thread_environment_id = thread_environment_id.clone();
                    }
                    return Ok(serde_json::to_value(&*existing)?);
                }
            }
            // Parent environment: explicit, else derived from cwd, else "agents".
            let resolved_env = match env {
                Some(env) if !env.is_empty() => env,
                _ => match cwd.as_deref().map(resolve_environment_from_cwd).transpose()? {
                    Some(value) if !value.is_empty() => value,
                    _ => "agents".to_owned(),
                },
            };
            let display_id = default_display_id(None, &resolved_env);
            runtime.store.ensure_environment(
                &resolved_env,
                &display_id,
                cwd.as_deref(),
                Some(&client),
                if resolved_env == "agents" { Some("Agents") } else { None },
            )?;
            let live = live_workspace_ids(&runtime.paths)?;
            let slot = runtime.store.create_temp_slot(
                &resolved_env,
                &display_id,
                cwd.as_deref(),
                Some(&client),
                &live,
                Some(&label),
                Some(&agent_id),
                None,
            )?;
            let record = runtime
                .store
                .resolve_slot_effective(&resolved_env, slot)?
                .ok_or_else(|| anyhow!("agent slot did not resolve"))?;
            let snapshot = AgentSnapshot {
                agent_id: agent_id.clone(),
                label,
                client,
                pid,
                environment_id: resolved_env,
                slot_index: slot,
                workspace_id: record.workspace_id,
                state: "idle".to_owned(),
                last_beat_ms: now_ms(),
                action_count: 0,
                last_action: None,
                current_target: None,
                attached_windows: Vec::new(),
                created_at_ms: now_ms(),
                thread_id,
                thread_environment_id,
            };
            runtime
                .agents
                .lock()
                .map_err(|e| anyhow!("agents poisoned: {e}"))?
                .insert(agent_id, snapshot.clone());
            Ok(serde_json::to_value(snapshot)?)
        }
        Request::AgentBeat {
            agent_id,
            state,
            target,
            action,
        } => {
            let mut agents = runtime.agents.lock().map_err(|e| anyhow!("agents poisoned: {e}"))?;
            let agent = agents
                .get_mut(&agent_id)
                .ok_or_else(|| anyhow!("unknown agent {agent_id}"))?;
            if let Some(state) = state {
                if !agent_state_valid(&state) {
                    return Err(anyhow!("invalid agent state {state}"));
                }
                agent.state = state;
            }
            if let Some(target) = target.filter(|t| !t.is_empty()) {
                if !agent.attached_windows.contains(&target) {
                    agent.attached_windows.push(target.clone());
                }
                agent.current_target = Some(target);
            }
            if let Some(action) = action {
                agent.action_count += 1;
                agent.last_action = Some(action);
            }
            agent.last_beat_ms = now_ms();
            Ok(serde_json::to_value(&*agent)?)
        }
        Request::AgentLabel { agent_id, label } => {
            let (env, slot) = {
                let mut agents = runtime.agents.lock().map_err(|e| anyhow!("agents poisoned: {e}"))?;
                let agent = agents
                    .get_mut(&agent_id)
                    .ok_or_else(|| anyhow!("unknown agent {agent_id}"))?;
                agent.label = label.clone();
                (agent.environment_id.clone(), agent.slot_index)
            };
            runtime.store.set_slot_display_name(&env, slot, &label)?;
            Ok(json!({"agent_id": agent_id, "label": label}))
        }
        Request::AgentFinish { agent_id } => {
            let mut agents = runtime.agents.lock().map_err(|e| anyhow!("agents poisoned: {e}"))?;
            if let Some(agent) = agents.get_mut(&agent_id) {
                agent.state = "finished".to_owned();
                agent.last_beat_ms = now_ms();
            }
            Ok(json!({"agent_id": agent_id, "finished": true}))
        }
        Request::AgentsList => {
            let agents = runtime.agents.lock().map_err(|e| anyhow!("agents poisoned: {e}"))?;
            let mut list: Vec<&AgentSnapshot> = agents.values().collect();
            list.sort_by_key(|a| a.created_at_ms);
            Ok(serde_json::to_value(list)?)
        }
        Request::SlotTempCreate {
            env,
            cwd,
            name,
            owner,
            client,
            launch_argv,
        } => {
            let resolved_env = resolve_explicit_or_default_with_connection(
                env.as_deref(),
                cwd.as_deref(),
                &runtime.store,
                None,
            )?;
            let display_id = default_display_id(env.as_deref(), &resolved_env);
            let live = live_workspace_ids(&runtime.paths)?;
            let slot = runtime.store.create_temp_slot(
                &resolved_env,
                &display_id,
                cwd.as_deref(),
                client.as_deref(),
                &live,
                name.as_deref(),
                owner.as_deref(),
                launch_argv.as_deref(),
            )?;
            let mut result = slot_configuration_response(&runtime.store, &resolved_env, slot)?;
            result["temporary"] = json!(true);
            result["slot"] = json!(slot);
            Ok(result)
        }
        Request::SlotRemove { env, slot, name } => {
            let resolved_env =
                resolve_required_environment(env.as_deref(), &runtime.store)?;
            let slot = match (slot, name) {
                (Some(slot), _) => slot,
                (None, Some(name)) => runtime
                    .store
                    .find_slot_by_name(&resolved_env, &name)?
                    .ok_or_else(|| anyhow!("no slot named {name:?} in {resolved_env}"))?,
                (None, None) => return Err(anyhow!("slot remove needs --slot or --name")),
            };
            runtime.store.clear_slot(&resolved_env, slot)?;
            Ok(json!({"removed": true, "env_id": resolved_env, "slot": slot}))
        }
        Request::SlotTempList => {
            let temps = runtime.store.list_temp_slots()?;
            Ok(json!(temps.iter().map(|t| json!({
                "env_id": t.env_id, "slot": t.slot_index, "workspace_id": t.workspace_id,
                "owner": t.owner, "empty_since": t.empty_since,
            })).collect::<Vec<_>>()))
        }
        Request::StickList => {
            let records = runtime.store.list_sticks()?;
            let live = send_plugin_spawn_value(&runtime.paths, &PluginSpawnRequest::List).ok();
            Ok(json!({
                "persisted": records.iter().map(|r| json!({
                    "stick_id": r.stick_id,
                    "workspace_id": r.workspace_id,
                    "root_pid": r.root_pid,
                    "root_alive": crate::spawn::pid_exists(r.root_pid),
                    "focus_policy": r.focus_policy,
                    "created_at": r.created_at,
                })).collect::<Vec<_>>(),
                "plugin": live,
            }))
        }
        Request::StickRelease { stick_id } => {
            let removed = runtime.store.delete_stick(&stick_id)?;
            let plugin = send_plugin_spawn_request(
                &runtime.paths,
                &PluginSpawnRequest::Unstick { stick_id: &stick_id },
            )
            .is_ok();
            Ok(json!({"stick_id": stick_id, "removed": removed, "plugin_notified": plugin}))
        }
        Request::StickAdd { workspace_id, pid } => {
            if workspace_id <= 0 {
                return Err(anyhow!("workspace id must be positive"));
            }
            if !crate::spawn::pid_exists(pid) {
                return Err(anyhow!("pid {pid} is not running"));
            }
            let record = StickRecord {
                stick_id: format!("manual-{pid}-{}", now_ms()),
                workspace_id,
                monitor_id: current_focused_monitor_id(&runtime.paths).unwrap_or(-1),
                root_pid: pid,
                focus_policy: "preserve".to_owned(),
                origin_monitor_id: -1,
                origin_workspace_id: -1,
                origin_window_address: None,
                created_at: 0,
            };
            runtime.store.insert_stick(&record)?;
            send_plugin_spawn_request(&runtime.paths, &stick_request(&record, true))?;
            Ok(json!({"stick_id": record.stick_id, "workspace_id": workspace_id, "root_pid": pid}))
        }
        Request::StickMove { stick_id, workspace_id } => {
            if workspace_id <= 0 {
                return Err(anyhow!("workspace id must be positive"));
            }
            let updated = runtime.store.set_stick_workspace(&stick_id, workspace_id)?;
            send_plugin_spawn_request(
                &runtime.paths,
                &PluginSpawnRequest::Move { stick_id: &stick_id, workspace_id },
            )?;
            Ok(json!({"stick_id": stick_id, "workspace_id": workspace_id, "persisted": updated}))
        }
        Request::UiSnapshotSwitcher { reverse } => {
            let snapshot = build_switcher_snapshot(runtime, reverse)?;
            debug!(
                reverse,
                item_count = snapshot.items.len(),
                initial_index = snapshot.initial_index,
                "built switcher UI snapshot"
            );
            append_switch_log(
                "server.snapshot.switcher.response",
                format!(
                    "reverse={reverse} items={} initial_index={}",
                    snapshot.items.len(),
                    snapshot.initial_index
                ),
            );
            Ok(serde_json::to_value(snapshot)?)
        }
        Request::UiSnapshotGrid { cwd } => {
            let snapshot = build_grid_snapshot(runtime, cwd.as_deref())?;
            Ok(serde_json::to_value(snapshot)?)
        }
        Request::BatchMutate {
            atomic, operations, ..
        } => {
            if !atomic {
                return Err(anyhow!("best-effort batch mode is not implemented"));
            }
            if operations.is_empty() {
                return Err(anyhow!("batch requires at least one operation"));
            }
            handle_batch_mutate(runtime, operations)
        }
    }
}

fn apply_mutation_request(
    runtime: &Arc<ServerRuntime>,
    request: BatchMutationRequest,
) -> Result<serde_json::Value> {
    apply_batch_mutation_request(runtime, None, request)
}

fn handle_batch_mutate(
    runtime: &Arc<ServerRuntime>,
    operations: Vec<BatchMutationRequest>,
) -> Result<serde_json::Value> {
    runtime.store.with_transaction(|connection| {
        let mut results = Vec::with_capacity(operations.len());
        for (index, operation) in operations.iter().cloned().enumerate() {
            let op_name = operation.op_name();
            let value = apply_batch_mutation_request(runtime, Some(connection), operation)
                .map_err(|error| anyhow!("batch operation {index} ({op_name}) failed: {error}"))?;
            results.push(BatchMutationOperationResult(value));
        }

        serde_json::to_value(BatchMutationResponse {
            atomic: true,
            operation_count: results.len(),
            results,
        })
        .map_err(Into::into)
    })
}

fn apply_batch_mutation_request(
    runtime: &Arc<ServerRuntime>,
    connection: Option<&Connection>,
    request: BatchMutationRequest,
) -> Result<serde_json::Value> {
    match request {
        BatchMutationRequest::EnvEnsure {
            env,
            cwd,
            client,
            title,
        } => {
            let resolved = resolve_or_default_environment(env.as_deref(), cwd.as_deref())?;
            let display_id = default_display_id(env.as_deref(), &resolved);
            let record = match connection {
                Some(connection) => runtime.store.ensure_environment_with_connection(
                    connection,
                    &resolved,
                    &display_id,
                    cwd.as_deref(),
                    client.as_deref(),
                    title.as_deref(),
                )?,
                None => runtime.store.ensure_environment(
                    &resolved,
                    &display_id,
                    cwd.as_deref(),
                    client.as_deref(),
                    title.as_deref(),
                )?,
            };
            Ok(json!({
                "env_id": record.env_id,
                "display_id": record.display_id,
                "title": record.title,
                "source_path": record.source_path,
            }))
        }
        BatchMutationRequest::EnvDelete { env } => {
            match connection {
                Some(connection) => runtime
                    .store
                    .delete_environment_with_connection(connection, &env)?,
                None => runtime.store.delete_environment(&env)?,
            }
            Ok(json!({"deleted": true, "env_id": env}))
        }
        BatchMutationRequest::EnvTitleSet { env, title } => {
            match connection {
                Some(connection) => runtime
                    .store
                    .set_environment_title_with_connection(connection, &env, &title)?,
                None => runtime.store.set_environment_title(&env, &title)?,
            }
            Ok(json!({"env_id": env, "title": title}))
        }
        BatchMutationRequest::EnvTitleClear { env } => {
            match connection {
                Some(connection) => runtime
                    .store
                    .clear_environment_title_with_connection(connection, &env)?,
                None => runtime.store.clear_environment_title(&env)?,
            }
            Ok(json!({"env_id": env, "title": serde_json::Value::Null}))
        }
        BatchMutationRequest::ClientEnsure { client } => {
            match connection {
                Some(connection) => runtime
                    .store
                    .ensure_client_with_connection(connection, &client)?,
                None => runtime.store.ensure_client(&client)?,
            }
            Ok(json!({"client_id": client}))
        }
        BatchMutationRequest::SlotAssign {
            env,
            slot,
            assignment_mode,
            client,
            cwd,
            launch_argv,
            display_name,
        } => {
            ensure_positive_slot(slot)?;
            let resolved_env = resolve_explicit_or_default_with_connection(
                env.as_deref(),
                cwd.as_deref(),
                &runtime.store,
                connection,
            )?;
            if matches!(assignment_mode, SlotAssignmentMode::Inherit)
                && !environment_has_parent(&resolved_env)
            {
                return Err(anyhow!(
                    "slot assign --inherit requires a named dotted environment with a parent"
                ));
            }
            let display_id = default_display_id(env.as_deref(), &resolved_env);
            let live_workspace_ids = if matches!(assignment_mode, SlotAssignmentMode::Managed) {
                live_workspace_ids(&runtime.paths)?
            } else {
                HashSet::new()
            };
            match connection {
                Some(connection) => runtime.store.assign_slot_with_connection(
                    connection,
                    &resolved_env,
                    slot,
                    &assignment_mode,
                    &display_id,
                    cwd.as_deref(),
                    client.as_deref(),
                    &live_workspace_ids,
                    launch_argv.as_deref(),
                    display_name.as_deref(),
                )?,
                None => runtime.store.assign_slot(
                    &resolved_env,
                    slot,
                    &assignment_mode,
                    &display_id,
                    cwd.as_deref(),
                    client.as_deref(),
                    &live_workspace_ids,
                    launch_argv.as_deref(),
                    display_name.as_deref(),
                )?,
            }
            slot_configuration_response_with_connection(
                &runtime.store,
                connection,
                &resolved_env,
                slot,
            )
        }
        BatchMutationRequest::SlotClear { env, slot, .. } => {
            ensure_positive_slot(slot)?;
            let resolved_env = resolve_required_environment_with_connection(
                env.as_deref(),
                &runtime.store,
                connection,
            )?;
            match connection {
                Some(connection) => {
                    runtime
                        .store
                        .clear_slot_with_connection(connection, &resolved_env, slot)?
                }
                None => runtime.store.clear_slot(&resolved_env, slot)?,
            }
            Ok(json!({"cleared": true, "env_id": resolved_env, "slot": slot}))
        }
        BatchMutationRequest::SlotCommandSet {
            env,
            slot,
            argv,
            display_name,
        } => {
            ensure_positive_slot(slot)?;
            if argv.is_empty() {
                return Err(anyhow!("slot command set requires a command"));
            }
            let resolved_env = resolve_required_environment_with_connection(
                env.as_deref(),
                &runtime.store,
                connection,
            )?;
            match connection {
                Some(connection) => runtime.store.set_slot_launch_command_with_connection(
                    connection,
                    &resolved_env,
                    slot,
                    &argv,
                    display_name.as_deref(),
                )?,
                None => runtime.store.set_slot_launch_command(
                    &resolved_env,
                    slot,
                    &argv,
                    display_name.as_deref(),
                )?,
            }
            slot_configuration_response_with_connection(
                &runtime.store,
                connection,
                &resolved_env,
                slot,
            )
        }
        BatchMutationRequest::SlotCommandClear { env, slot } => {
            ensure_positive_slot(slot)?;
            let resolved_env = resolve_required_environment_with_connection(
                env.as_deref(),
                &runtime.store,
                connection,
            )?;
            let cleared = match connection {
                Some(connection) => runtime.store.clear_slot_launch_command_with_connection(
                    connection,
                    &resolved_env,
                    slot,
                )?,
                None => runtime
                    .store
                    .clear_slot_launch_command(&resolved_env, slot)?,
            };
            match slot_configuration_response_with_connection(
                &runtime.store,
                connection,
                &resolved_env,
                slot,
            ) {
                Ok(mut value) => {
                    if let Some(object) = value.as_object_mut() {
                        object.insert("cleared".to_owned(), serde_json::Value::Bool(cleared));
                    }
                    Ok(value)
                }
                Err(_) if !cleared => Ok(json!({
                    "environment_id": resolved_env,
                    "slot_index": slot,
                    "binding_environment_id": serde_json::Value::Null,
                    "command_environment_id": serde_json::Value::Null,
                    "physical_workspace_id": serde_json::Value::Null,
                    "binding_kind": serde_json::Value::Null,
                    "launch_argv": serde_json::Value::Null,
                    "resolved": false,
                    "cleared": false,
                })),
                Err(error) => Err(error),
            }
        }
        BatchMutationRequest::SlotNameSet { env, slot, name } => {
            ensure_positive_slot(slot)?;
            let resolved_env = resolve_required_environment_with_connection(
                env.as_deref(),
                &runtime.store,
                connection,
            )?;
            match connection {
                Some(connection) => runtime.store.set_slot_display_name_with_connection(
                    connection,
                    &resolved_env,
                    slot,
                    &name,
                )?,
                None => runtime
                    .store
                    .set_slot_display_name(&resolved_env, slot, &name)?,
            }
            slot_configuration_response_with_connection(
                &runtime.store,
                connection,
                &resolved_env,
                slot,
            )
        }
        BatchMutationRequest::SlotNameClear { env, slot } => {
            ensure_positive_slot(slot)?;
            let resolved_env = resolve_required_environment_with_connection(
                env.as_deref(),
                &runtime.store,
                connection,
            )?;
            let cleared = match connection {
                Some(connection) => runtime.store.clear_slot_display_name_with_connection(
                    connection,
                    &resolved_env,
                    slot,
                )?,
                None => runtime.store.clear_slot_display_name(&resolved_env, slot)?,
            };
            let mut value = slot_configuration_response_with_connection(
                &runtime.store,
                connection,
                &resolved_env,
                slot,
            )?;
            if let Some(object) = value.as_object_mut() {
                object.insert("cleared".to_owned(), serde_json::Value::Bool(cleared));
            }
            Ok(value)
        }
        BatchMutationRequest::LockSet { env } => {
            match connection {
                Some(connection) => {
                    runtime.store.ensure_environment_with_connection(
                        connection,
                        &env,
                        &default_display_id(Some(&env), &env),
                        None,
                        None,
                        None,
                    )?;
                    runtime
                        .store
                        .set_locked_environment_with_connection(connection, &env)?;
                }
                None => {
                    runtime.store.ensure_environment(
                        &env,
                        &default_display_id(Some(&env), &env),
                        None,
                        None,
                        None,
                    )?;
                    runtime.store.set_locked_environment(&env)?;
                }
            }
            Ok(json!({"locked_environment_id": env}))
        }
        BatchMutationRequest::LockClear => {
            match connection {
                Some(connection) => runtime
                    .store
                    .clear_locked_environment_with_connection(connection)?,
                None => runtime.store.clear_locked_environment()?,
            }
            Ok(json!({"locked_environment_id": serde_json::Value::Null}))
        }
    }
}

fn build_switcher_snapshot(runtime: &ServerRuntime, reverse: bool) -> Result<SwitcherSnapshot> {
    let local_bindings = runtime.store.list_local_bindings()?;
    let locked_environment_id = runtime.store.locked_environment()?;
    let descriptors = current_workspace_cards(runtime)?;
    let card_count = descriptors.len();
    let descriptors = descriptors
        .into_iter()
        .map(|mut item| {
            let (resolution, _) = resolve_slot_for_physical_workspace_with_store(
                &runtime.store,
                &local_bindings,
                item.workspace_id,
                locked_environment_id.as_deref(),
            )?;
            if let Some(resolution) = resolution {
                item.slot_index = resolution.slot_index;
                item.slot_display_name = resolution.display_name.unwrap_or_default();
            }
            item.workspace_name = live_display_label(
                &item.subtitle,
                &item.app_class,
                Some(item.slot_display_name.as_str()),
                &item.workspace_name,
            );
            Ok(item)
        })
        .collect::<Result<Vec<_>>>()?;
    // Temporary workspaces stay out of the MRU switcher: they belong to their
    // environment's grid row and nowhere else, so Alt-Tab never lands on one.
    // Filtering here (before the initial selection is computed) keeps the
    // indexes the shell cycles through in step with the cards it draws; the
    // shell drops temp cards too, as belt and braces.
    let descriptors = descriptors
        .into_iter()
        .filter(|item| item.slot_index < TEMP_SLOT_START)
        .collect::<Vec<_>>();
    let initial_index = initial_selection_index(
        &descriptors
            .iter()
            .map(|item| crate::workspace_utils::WorkspaceDescriptor {
                id: item.workspace_id,
                name: item.workspace_name.clone(),
                subtitle: item.subtitle.clone(),
                app_class: item.app_class.clone(),
                window_count: item.window_count,
                focus_history_rank: i32::MAX,
                active: item.active,
            })
            .collect::<Vec<_>>(),
        reverse,
    );
    let workspace_ids = descriptors
        .iter()
        .map(|item| item.workspace_id.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let active_workspace_id = descriptors
        .iter()
        .find(|item| item.active)
        .map(|item| item.workspace_id)
        .unwrap_or(-1);
    debug!(
        reverse,
        card_count,
        item_count = descriptors.len(),
        locked_environment_id,
        workspace_ids,
        active_workspace_id,
        initial_index,
        "switcher snapshot summary"
    );
    append_switch_log(
        "server.snapshot.switcher",
        format!(
            "reverse={reverse} cards={card_count} items={} locked_env={:?} workspace_ids={} active_workspace_id={active_workspace_id} initial_index={initial_index}",
            descriptors.len(),
            locked_environment_id,
            workspace_ids
        ),
    );

    let mut items: Vec<WorkspaceCardSnapshot> = descriptors
        .into_iter()
        .map(|item| WorkspaceCardSnapshot {
            environment_id: None,
            workspace_id: item.workspace_id,
            slot_index: item.slot_index,
            workspace_name: item.workspace_name,
            subtitle: item.subtitle,
            app_class: item.app_class,
            window_count: item.window_count,
            active: item.active,
        })
        .collect();
    // Browser slots share a physical workspace but retain separate env/slot identities.
    for cell in build_grid_snapshot(runtime, None)?.items {
        if cell.slot_index >= TEMP_SLOT_START {
            continue;
        }
        if runtime
            .store
            .browser_target(&cell.environment_id, cell.slot_index)?
            .is_none()
        {
            continue;
        }
        items.push(WorkspaceCardSnapshot {
            environment_id: Some(cell.environment_id),
            workspace_id: cell.physical_workspace_id,
            slot_index: cell.slot_index,
            workspace_name: cell.workspace_name,
            subtitle: cell.subtitle,
            app_class: cell.app_class,
            window_count: 1,
            active: false,
        });
    }
    let initial_index = if reverse && !items.is_empty() {
        items.len() as i32 - 1
    } else {
        initial_index
    };
    Ok(SwitcherSnapshot {
        items,
        initial_index,
    })
}

fn slot_resolution_from_record(record: crate::db::SlotResolutionRecord) -> SlotResolution {
    SlotResolution {
        environment_id: record.environment_id,
        binding_environment_id: record.binding_environment_id,
        command_environment_id: record.command_environment_id,
        slot_index: record.slot_index,
        display_name: record.display_name,
        physical_workspace_id: record.workspace_id,
        binding_kind: record.binding_kind.as_str().to_owned(),
        launch_argv: record.launch_argv,
    }
}

fn slot_configuration_response(
    store: &StateStore,
    env_id: &str,
    slot_index: i32,
) -> Result<serde_json::Value> {
    slot_configuration_response_with_connection(store, None, env_id, slot_index)
}

fn slot_configuration_response_with_connection(
    store: &StateStore,
    connection: Option<&Connection>,
    env_id: &str,
    slot_index: i32,
) -> Result<serde_json::Value> {
    let resolved = match connection {
        Some(connection) => {
            store.resolve_slot_effective_with_connection(connection, env_id, slot_index)?
        }
        None => store.resolve_slot_effective(env_id, slot_index)?,
    };
    if let Some(record) = resolved {
        let mut value = serde_json::to_value(slot_resolution_from_record(record))?;
        if let Some(object) = value.as_object_mut() {
            object.insert("resolved".to_owned(), serde_json::Value::Bool(true));
        }
        return Ok(value);
    }

    let local = match connection {
        Some(connection) => store.get_local_slot_with_connection(connection, env_id, slot_index)?,
        None => store.get_local_slot(env_id, slot_index)?,
    }
    .ok_or_else(|| anyhow!("slot {slot_index} is not assigned for environment {env_id}"))?;
    Ok(json!({
        "environment_id": env_id,
        "binding_environment_id": serde_json::Value::Null,
        "command_environment_id": local.launch_argv.as_ref().map(|_| env_id.to_owned()),
        "slot_index": slot_index,
        "display_name": local.display_name,
        "physical_workspace_id": serde_json::Value::Null,
        "binding_kind": local.binding_kind.as_str(),
        "launch_argv": local.launch_argv,
        "resolved": false,
    }))
}

fn resolve_required_environment_with_connection(
    env: Option<&str>,
    store: &StateStore,
    connection: Option<&Connection>,
) -> Result<String> {
    if let Some(env) = env.filter(|value| !value.is_empty()) {
        return Ok(env.to_owned());
    }

    match connection {
        Some(connection) => store
            .locked_environment_with_connection(connection)?
            .ok_or_else(|| anyhow!("no environment specified and no global lock is active")),
        None => resolve_required_environment(env, store),
    }
}

fn resolve_explicit_or_default_with_connection(
    env: Option<&str>,
    cwd: Option<&str>,
    store: &StateStore,
    connection: Option<&Connection>,
) -> Result<String> {
    if let Some(env) = env.filter(|value| !value.is_empty()) {
        return Ok(env.to_owned());
    }

    resolve_or_default_environment(None, cwd).or_else(|_| match connection {
        Some(connection) => store
            .locked_environment_with_connection(connection)?
            .ok_or_else(|| anyhow!("no environment specified and no global lock is active")),
        None => store
            .locked_environment()?
            .ok_or_else(|| anyhow!("no environment specified and no global lock is active")),
    })
}

fn attempt_slot_launch(
    runtime: &ServerRuntime,
    record: &crate::db::SlotResolutionRecord,
) -> Result<NavigationLaunchResult> {
    if let Some(target) = runtime
        .store
        .browser_target(&record.environment_id, record.slot_index)?
    {
        // Navigation runs on every visit, including when the browser already exists.
        crate::browser::navigate(&target)?;
        return Ok(NavigationLaunchResult {
            configured: true,
            attempted: true,
            skipped_reason: None,
            error: None,
        });
    }
    let Some(argv) = record.launch_argv.as_ref() else {
        return Ok(skipped_launch(
            NavigationLaunchSkippedReason::NoLaunchConfigured,
        ));
    };

    let launch_key = LaunchSlotKey {
        environment_id: record.environment_id.clone(),
        slot_index: record.slot_index,
    };
    if workspace_has_mapped_clients(&runtime.paths, record.workspace_id)? {
        clear_pending_launch(runtime, &launch_key)?;
        return Ok(NavigationLaunchResult {
            configured: true,
            attempted: false,
            skipped_reason: Some(NavigationLaunchSkippedReason::WorkspaceNotEmpty),
            error: None,
        });
    }

    {
        let mut pending = runtime
            .pending_launches
            .lock()
            .map_err(|error| anyhow!("pending launch registry poisoned: {error}"))?;
        let now = now_ms();
        pending.purge_expired(now);
        if pending.contains(&launch_key) {
            return Ok(NavigationLaunchResult {
                configured: true,
                attempted: false,
                skipped_reason: Some(NavigationLaunchSkippedReason::PendingLaunch),
                error: None,
            });
        }
        pending.insert(launch_key.clone(), record.workspace_id, now);
    }

    match run_in_workspace(&runtime.paths, record.workspace_id, argv) {
        Ok(()) => Ok(NavigationLaunchResult {
            configured: true,
            attempted: true,
            skipped_reason: None,
            error: None,
        }),
        Err(error) => {
            clear_pending_launch(runtime, &launch_key)?;
            Ok(NavigationLaunchResult {
                configured: true,
                attempted: true,
                skipped_reason: None,
                error: Some(error.to_string()),
            })
        }
    }
}

fn clear_pending_launch(runtime: &ServerRuntime, key: &LaunchSlotKey) -> Result<()> {
    let mut pending = runtime
        .pending_launches
        .lock()
        .map_err(|error| anyhow!("pending launch registry poisoned: {error}"))?;
    pending.remove(key);
    Ok(())
}

fn skipped_launch(skipped_reason: NavigationLaunchSkippedReason) -> NavigationLaunchResult {
    NavigationLaunchResult {
        configured: false,
        attempted: false,
        skipped_reason: Some(skipped_reason),
        error: None,
    }
}

fn resolve_slot_for_physical_workspace(
    runtime: &ServerRuntime,
    workspace_id: i32,
    locked_environment_id: Option<&str>,
) -> Result<(
    Option<crate::db::SlotResolutionRecord>,
    Option<NavigationLaunchSkippedReason>,
)> {
    let bindings = runtime.store.list_local_bindings()?;
    resolve_slot_for_physical_workspace_with_store(
        &runtime.store,
        &bindings,
        workspace_id,
        locked_environment_id,
    )
}

fn resolve_environment_for_physical_workspace(
    store: &StateStore,
    workspace_id: i32,
    locked_environment_id: Option<&str>,
) -> Result<Option<String>> {
    let bindings = store.list_local_bindings()?;
    let (record, _) = resolve_slot_for_physical_workspace_with_store(
        store,
        &bindings,
        workspace_id,
        locked_environment_id,
    )?;
    Ok(record.map(|record| record.environment_id))
}

fn resolve_focus_environment_for_physical_workspace(
    store: &StateStore,
    workspace_id: i32,
) -> Result<Option<String>> {
    let bindings = store.list_local_bindings()?;
    Ok(resolve_focus_environment_from_bindings(
        &bindings,
        workspace_id,
    ))
}

fn resolve_focus_environment_from_bindings(
    bindings: &[SlotBindingRecord],
    workspace_id: i32,
) -> Option<String> {
    let (record, _) = resolve_slot_binding_for_workspace_id(bindings, workspace_id);
    record.map(|record| record.environment_id)
}

fn resolve_slot_for_physical_workspace_with_store(
    store: &StateStore,
    bindings: &[SlotBindingRecord],
    workspace_id: i32,
    locked_environment_id: Option<&str>,
) -> Result<(
    Option<crate::db::SlotResolutionRecord>,
    Option<NavigationLaunchSkippedReason>,
)> {
    if let Some(locked_environment_id) = locked_environment_id {
        let effective_matches = slot_indexes_for_environment(locked_environment_id, &bindings)
            .into_iter()
            .map(|slot_index| store.resolve_slot_effective(locked_environment_id, slot_index))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .filter(|record| record.workspace_id == workspace_id)
            .collect::<Vec<_>>();

        if effective_matches.len() == 1 {
            return Ok((effective_matches.into_iter().next(), None));
        }

        if effective_matches.len() > 1 {
            return Ok((
                None,
                Some(NavigationLaunchSkippedReason::AmbiguousSlotMapping),
            ));
        }
    }

    Ok(resolve_slot_binding_for_workspace_id(
        bindings,
        workspace_id,
    ))
}

fn resolve_slot_binding_for_workspace_id(
    bindings: &[SlotBindingRecord],
    workspace_id: i32,
) -> (
    Option<crate::db::SlotResolutionRecord>,
    Option<NavigationLaunchSkippedReason>,
) {
    let matches = bindings
        .iter()
        .filter(|binding| {
            binding.binding_kind.is_concrete() && binding.workspace_id == Some(workspace_id)
        })
        .cloned()
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return (None, Some(NavigationLaunchSkippedReason::NoSlotMapping));
    }

    if matches.len() == 1 {
        let binding = matches.into_iter().next().expect("single match");
        let environment_id = binding.env_id.clone();
        let command_environment_id = binding.launch_argv.as_ref().map(|_| environment_id.clone());
        return (
            Some(crate::db::SlotResolutionRecord {
                environment_id: environment_id.clone(),
                binding_environment_id: environment_id,
                command_environment_id,
                slot_index: binding.slot_index,
                display_name: binding.display_name,
                binding_kind: binding.binding_kind,
                workspace_id: binding.workspace_id.expect("concrete binding workspace"),
                launch_argv: binding.launch_argv,
            }),
            None,
        );
    }

    (
        None,
        Some(NavigationLaunchSkippedReason::AmbiguousSlotMapping),
    )
}

fn build_grid_snapshot(runtime: &ServerRuntime, cwd: Option<&str>) -> Result<GridSnapshot> {
    let workspace_cards = current_workspace_cards(runtime)?;
    let cards_by_workspace = workspace_cards
        .into_iter()
        .map(|card| (card.workspace_id, card))
        .collect::<HashMap<_, _>>();
    let current_workspace_id = current_active_workspace_id(&runtime.paths)?;
    let locked_env_id = runtime.store.locked_environment()?;
    let current_env_id = cwd
        .map(resolve_environment_from_cwd)
        .transpose()?
        .filter(|value| !value.is_empty());
    let local_bindings = runtime.store.list_local_bindings()?;
    let rows = plan_grid_rows(
        runtime.store.list_environments()?,
        &local_bindings,
        locked_env_id.as_deref(),
        current_env_id.as_deref(),
    );

    let stuck_workspaces = runtime
        .store
        .list_sticks()
        .unwrap_or_default()
        .into_iter()
        .map(|stick| stick.workspace_id)
        .collect::<HashSet<i32>>();
    let temp_meta = runtime
        .store
        .list_temp_slots()
        .unwrap_or_default()
        .into_iter()
        .map(|meta| ((meta.env_id.clone(), meta.slot_index), meta))
        .collect::<HashMap<(String, i32), TempSlotMeta>>();
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let agents_by_workspace = runtime
        .agents
        .lock()
        .map(|agents| {
            agents
                .values()
                .map(|agent| (agent.workspace_id, agent.clone()))
                .collect::<HashMap<i32, AgentSnapshot>>()
        })
        .unwrap_or_default();

    let mut items = Vec::new();
    let mut max_column_count = 0;
    let mut row_count = 0;

    for row in &rows {
        let mut row_items = Vec::new();

        for (column_index, slot) in row.slots.iter().enumerate() {
            let record = &slot.record;
            let workspace_id = record.workspace_id;
            let card = cards_by_workspace.get(&workspace_id);
            let browser_target = runtime
                .store
                .browser_target(&row.leaf.env_id, record.slot_index)?;
            let active = workspace_id == current_workspace_id && browser_target.is_none();
            let temp = temp_meta.get(&(record.binding_environment_id.clone(), record.slot_index));

            row_items.push(GridCellSnapshot {
                workspace_name: browser_target
                    .as_ref()
                    .map(|target| {
                        record
                            .display_name
                            .clone()
                            .unwrap_or_else(|| target.workspace.clone())
                    })
                    .unwrap_or_else(|| workspace_display_label(card, record, workspace_id)),
                subtitle: browser_target
                    .as_ref()
                    .map(|target| format!("{} · {}", target.name, target.workspace))
                    .or_else(|| card.map(|item| item.subtitle.clone()))
                    .unwrap_or_else(|| format!("Workspace {}", workspace_id)),
                app_class: card.map(|item| item.app_class.clone()).unwrap_or_default(),
                window_count: card.map(|item| item.window_count).unwrap_or(0),
                active,
                stuck: stuck_workspaces.contains(&workspace_id),
                temporary: temp.is_some(),
                owner: temp.and_then(|meta| meta.owner.clone()),
                empty_for_ms: temp
                    .and_then(|meta| meta.empty_since)
                    .map(|since| ((now_unix - since).max(0) as u64) * 1000),
                agent: agents_by_workspace.get(&workspace_id).cloned(),
                ..grid_cell_from_plan(row, slot, row_count, column_index)
            });
        }

        if row_items.is_empty() {
            continue;
        }

        max_column_count = max_column_count.max(row_items.len() as i32);
        items.extend(row_items);
        row_count += 1;
    }

    let initial_index = if items.is_empty() {
        -1
    } else {
        items
            .iter()
            .position(|item| {
                item.row_index == 0 && item.physical_workspace_id == current_workspace_id
            })
            .or_else(|| items.iter().position(|item| item.row_index == 0))
            .map(|index| index as i32)
            .unwrap_or(0)
    };

    Ok(GridSnapshot {
        items,
        initial_index,
        row_count,
        max_column_count,
    })
}

/// Row order: the row holding the lock, then the row you are in, then by
/// recent focus. A row holds the lock or the cwd when that environment is
/// anywhere on its chain.
fn compare_grid_rows(
    left: &EnvironmentRecord,
    right: &EnvironmentRecord,
    locked_env_id: Option<&str>,
    current_env_id: Option<&str>,
) -> std::cmp::Ordering {
    row_sort_key(&left.env_id, locked_env_id, current_env_id)
        .cmp(&row_sort_key(&right.env_id, locked_env_id, current_env_id))
        .then_with(|| right.last_focused_at.cmp(&left.last_focused_at))
        .then_with(|| left.display_id.cmp(&right.display_id))
        .then_with(|| left.env_id.cmp(&right.env_id))
}

/// One slot of a planned grid row.
struct PlannedSlot {
    record: crate::db::SlotResolutionRecord,
    /// Label of the environment that binds the slot.
    owner_label: String,
}

/// One grid row: a leaf environment and every slot it resolves.
struct GridRowPlan {
    leaf: EnvironmentRecord,
    title: String,
    chain: Vec<GridChainLevel>,
    locked_environment_id: Option<String>,
    slots: Vec<PlannedSlot>,
}

/// What to call an environment: its title, else the last component of its
/// cwd, else nothing.
fn environment_label(env: &EnvironmentRecord) -> String {
    if let Some(title) = env
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
    {
        return title.to_owned();
    }
    env.source_path
        .as_deref()
        .map(|path| path.trim_end_matches('/'))
        .and_then(|path| Path::new(path).file_name())
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_default()
}

/// The grid's rows: one per leaf of the environment tree.
///
/// Environment ids are prefix chains (`p.x` → `p.x.w` → `p.x.w.a.t`) and a
/// child resolves slots through its ancestors, so an ancestor's frames are
/// the same workspaces every descendant shows. Showing every environment as
/// its own row repeats those frames once per level. Instead:
///
/// - A candidate is an environment with a binding of its own, or the locked
///   or current environment, that resolves at least one slot.
/// - A candidate is a row unless another candidate descends from it. An
///   ancestor with no live descendant is its own row; so is one that owns a
///   temporary slot, since a temp shows only in its owner's row.
/// - A row's cells are its numbered slots resolved through the chain plus its
///   own temps, in slot order. A cell bound by an ancestor is `shared`.
/// - The title is the deepest titled level of the chain, else the deepest
///   cwd name, else the leaf's id.
fn plan_grid_rows(
    environments: Vec<EnvironmentRecord>,
    local_bindings: &[SlotBindingRecord],
    locked_env_id: Option<&str>,
    current_env_id: Option<&str>,
) -> Vec<GridRowPlan> {
    let binding_index = local_bindings
        .iter()
        .map(|binding| ((binding.env_id.as_str(), binding.slot_index), binding))
        .collect::<HashMap<_, _>>();
    let by_id = environments
        .iter()
        .map(|env| (env.env_id.as_str(), env))
        .collect::<HashMap<_, _>>();
    let bound = local_bindings
        .iter()
        .map(|binding| binding.env_id.as_str())
        .collect::<HashSet<_>>();
    let temp_owners = local_bindings
        .iter()
        .filter(|binding| binding.slot_index >= TEMP_SLOT_START)
        .map(|binding| binding.env_id.as_str())
        .collect::<HashSet<_>>();

    let mut candidates = Vec::new();
    for env in &environments {
        let id = env.env_id.as_str();
        if !(bound.contains(id) || locked_env_id == Some(id) || current_env_id == Some(id)) {
            continue;
        }
        let records = slot_indexes_for_environment(id, local_bindings)
            .into_iter()
            .filter_map(|slot_index| {
                resolve_slot_effective_from_bindings(&binding_index, id, slot_index)
            })
            .collect::<Vec<_>>();
        if !records.is_empty() {
            candidates.push((env.clone(), records));
        }
    }

    let candidate_chains = candidates
        .iter()
        .map(|(env, _)| environment_chain(&env.env_id))
        .collect::<Vec<_>>();
    let has_live_descendant = |id: &str| {
        candidate_chains
            .iter()
            .any(|chain| chain.len() > 1 && chain[1..].iter().any(|ancestor| ancestor == id))
    };
    candidates.retain(|(env, _)| {
        temp_owners.contains(env.env_id.as_str()) || !has_live_descendant(&env.env_id)
    });
    candidates.sort_by(|(left, _), (right, _)| {
        compare_grid_rows(left, right, locked_env_id, current_env_id)
    });

    candidates
        .into_iter()
        .map(|(leaf, records)| {
            let full_chain = environment_chain(&leaf.env_id);
            let chain = full_chain
                .iter()
                .rev()
                .filter_map(|id| by_id.get(id.as_str()))
                .map(|env| GridChainLevel {
                    id: env.env_id.clone(),
                    title: env.title.clone().unwrap_or_default().trim().to_owned(),
                    label: environment_label(env),
                    locked: locked_env_id == Some(env.env_id.as_str()),
                })
                .collect::<Vec<_>>();
            let locked_environment_id = locked_env_id
                .filter(|locked| full_chain.iter().any(|id| id == locked))
                .map(ToOwned::to_owned);
            let title = chain
                .iter()
                .rev()
                .find(|level| !level.title.is_empty())
                .or_else(|| chain.iter().rev().find(|level| !level.label.is_empty()))
                .map(|level| level.label.clone())
                .or_else(|| Some(leaf.display_id.clone()).filter(|id| !id.is_empty()))
                .unwrap_or_else(|| leaf.env_id.clone());
            let slots = records
                .into_iter()
                .map(|record| PlannedSlot {
                    owner_label: by_id
                        .get(record.binding_environment_id.as_str())
                        .map(|env| environment_label(env))
                        .unwrap_or_default(),
                    record,
                })
                .collect();
            GridRowPlan {
                leaf,
                title,
                chain,
                locked_environment_id,
                slots,
            }
        })
        .collect()
}

/// A cell with the row and slot fields filled in and the live ones (windows,
/// focus, temp timers, agents) left empty for the caller.
fn grid_cell_from_plan(
    row: &GridRowPlan,
    slot: &PlannedSlot,
    row_index: i32,
    column_index: usize,
) -> GridCellSnapshot {
    let record = &slot.record;
    let shared = record.binding_environment_id != row.leaf.env_id;
    GridCellSnapshot {
        environment_id: row.leaf.env_id.clone(),
        environment_display_id: row.leaf.display_id.clone(),
        environment_title: row.title.clone(),
        binding_environment_id: Some(record.binding_environment_id.clone()),
        command_environment_id: record.command_environment_id.clone(),
        slot_index: record.slot_index,
        slot_display_name: record.display_name.clone().unwrap_or_default(),
        physical_workspace_id: record.workspace_id,
        binding_kind: record.binding_kind.as_str().to_owned(),
        inherited: shared,
        owner_environment_id: record.binding_environment_id.clone(),
        owner_title: slot.owner_label.clone(),
        shared,
        environment_chain: row.chain.clone(),
        locked_environment_id: row.locked_environment_id.clone(),
        workspace_name: format!("Workspace {}", record.workspace_id),
        subtitle: format!("Workspace {}", record.workspace_id),
        app_class: String::new(),
        window_count: 0,
        active: false,
        environment_locked: row.locked_environment_id.is_some(),
        stuck: false,
        temporary: false,
        unnumbered: record.slot_index >= TEMP_SLOT_START,
        owner: None,
        empty_for_ms: None,
        agent: None,
        show_environment_label: column_index == 0,
        row_index,
        column_index: column_index as i32,
    }
}

fn resolve_slot_effective_from_bindings<'a>(
    binding_index: &HashMap<(&'a str, i32), &'a SlotBindingRecord>,
    env_id: &str,
    slot_index: i32,
) -> Option<crate::db::SlotResolutionRecord> {
    let chain = if slot_index >= TEMP_SLOT_START {
        // Temporary slots are owned outright: never walk up to an ancestor.
        vec![env_id.to_owned()]
    } else {
        environment_chain(env_id)
    };
    let mut binding_environment_id = None;
    let mut binding_kind = None;
    let mut workspace_id = None;
    let mut command_environment_id = None;
    let mut display_name = None;
    let mut launch_argv = None;

    for candidate_env in chain {
        let Some(local) = binding_index.get(&(candidate_env.as_str(), slot_index)) else {
            continue;
        };

        if display_name.is_none() && local.display_name.is_some() {
            display_name = local.display_name.clone();
        }

        if command_environment_id.is_none() && local.launch_argv.is_some() {
            command_environment_id = Some(local.env_id.clone());
            launch_argv = local.launch_argv.clone();
        }

        if binding_environment_id.is_none() && local.binding_kind.is_concrete() {
            binding_environment_id = Some(local.env_id.clone());
            binding_kind = Some(local.binding_kind);
            workspace_id = local.workspace_id;
        }
    }

    match (binding_environment_id, binding_kind, workspace_id) {
        (Some(binding_environment_id), Some(binding_kind), Some(workspace_id)) => {
            Some(crate::db::SlotResolutionRecord {
                environment_id: env_id.to_owned(),
                binding_environment_id,
                command_environment_id,
                slot_index,
                display_name,
                binding_kind,
                workspace_id,
                launch_argv,
            })
        }
        _ => None,
    }
}

fn workspace_display_label(
    card: Option<&WorkspaceCardData>,
    record: &crate::db::SlotResolutionRecord,
    workspace_id: i32,
) -> String {
    let fallback = format!("Workspace {}", workspace_id);
    live_display_label(
        card.map(|item| item.subtitle.as_str()).unwrap_or_default(),
        card.map(|item| item.app_class.as_str()).unwrap_or_default(),
        record.display_name.as_deref(),
        &fallback,
    )
}

fn live_display_label(
    subtitle: &str,
    app_class: &str,
    stored_name: Option<&str>,
    fallback: &str,
) -> String {
    if !subtitle.is_empty() {
        return subtitle.to_owned();
    }
    if !app_class.is_empty() {
        return app_class.to_owned();
    }
    if let Some(name) = stored_name.filter(|value| !value.is_empty()) {
        return name.to_owned();
    }
    fallback.to_owned()
}

/// Slot indexes that make up one environment's row.
///
/// Numbered slots (`slot_index < TEMP_SLOT_START`) are inherited down the
/// environment chain, so a child row shows its ancestors' numbered slots.
/// Temporary slots are not: a temp belongs to the environment that created it
/// and appears only in that environment's row. The index threshold is the
/// source of truth here — `create_temp_slot` always allocates at or above
/// `TEMP_SLOT_START` and nothing else does, so it agrees with the `temporary`
/// column the snapshot's `temporary` field reads.
fn slot_indexes_for_environment(env_id: &str, bindings: &[SlotBindingRecord]) -> Vec<i32> {
    let env_ids = environment_chain(env_id);
    let hierarchical = env_ids.len() > 1;
    let mut slot_indexes = bindings
        .iter()
        .filter(|binding| {
            if binding.env_id != env_id && binding.slot_index >= TEMP_SLOT_START {
                // An ancestor's temporary slot is never inherited.
                return false;
            }
            if hierarchical {
                env_ids.iter().any(|candidate| candidate == &binding.env_id)
            } else {
                binding.env_id == env_id
            }
        })
        .map(|binding| binding.slot_index)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    slot_indexes.sort_unstable();
    slot_indexes
}

fn row_sort_key(env_id: &str, locked_env_id: Option<&str>, current_env_id: Option<&str>) -> i32 {
    let chain = environment_chain(env_id);
    let on_chain = |target: Option<&str>| target.is_some_and(|target| chain.iter().any(|id| id == target));
    if on_chain(locked_env_id) {
        0
    } else if on_chain(current_env_id) {
        1
    } else {
        2
    }
}

fn current_workspace_cards(runtime: &ServerRuntime) -> Result<Vec<WorkspaceCardData>> {
    let monitors = run_hyprctl_json(&runtime.paths, &["-j", "monitors"])?;
    let workspaces = run_hyprctl_json(&runtime.paths, &["-j", "workspaces"])?;
    let clients = run_hyprctl_json(&runtime.paths, &["-j", "clients"])?;
    current_workspace_cards_from_json(&monitors, &workspaces, &clients)
}

fn current_workspace_cards_from_json(
    monitors: &[u8],
    workspaces: &[u8],
    clients: &[u8],
) -> Result<Vec<WorkspaceCardData>> {
    let descriptors = build_workspace_descriptors(monitors, workspaces, clients);

    Ok(descriptors
        .into_iter()
        .map(|descriptor| WorkspaceCardData {
            workspace_id: descriptor.id,
            slot_index: 0,
            slot_display_name: String::new(),
            workspace_name: descriptor.name,
            subtitle: descriptor.subtitle,
            app_class: descriptor.app_class,
            window_count: descriptor.window_count,
            active: descriptor.active,
        })
        .collect())
}

fn current_active_workspace_id(paths: &RuntimePaths) -> Result<i32> {
    let monitors = run_hyprctl_json(paths, &["-j", "monitors"])?;
    let monitors = serde_json::from_slice::<Vec<MonitorInfo>>(&monitors).unwrap_or_default();
    Ok(monitors
        .into_iter()
        .find(|monitor| monitor.focused)
        .map(|monitor| monitor.active_workspace.id)
        .unwrap_or(-1))
}

fn current_focused_monitor_id(paths: &RuntimePaths) -> Result<i32> {
    let monitors = run_hyprctl_json(paths, &["-j", "monitors"])?;
    let monitors = serde_json::from_slice::<Vec<MonitorInfo>>(&monitors).unwrap_or_default();
    Ok(monitors
        .into_iter()
        .find(|monitor| monitor.focused)
        .map(|monitor| monitor.id)
        .unwrap_or(-1))
}

fn current_spawn_origin_snapshot(paths: &RuntimePaths) -> Result<SpawnOriginSnapshot> {
    let monitor_id = current_focused_monitor_id(paths)?;
    let workspace_id = current_active_workspace_id(paths)?;
    let active_window = run_hyprctl_json(paths, &["-j", "activewindow"])?;
    let active_window =
        serde_json::from_slice::<ActiveWindowInfo>(&active_window).unwrap_or_default();

    Ok(SpawnOriginSnapshot {
        monitor_id,
        workspace_id,
        window_address: (!active_window.address.is_empty()).then_some(active_window.address),
    })
}

fn live_workspace_ids(paths: &RuntimePaths) -> Result<HashSet<i32>> {
    let workspaces = run_hyprctl_json(paths, &["-j", "workspaces"])?;
    let items = serde_json::from_slice::<Vec<WorkspaceInfo>>(&workspaces).unwrap_or_default();
    Ok(items
        .into_iter()
        .map(|item| item.id)
        .filter(|id| *id > 0)
        .collect())
}

fn workspace_has_mapped_clients(paths: &RuntimePaths, workspace_id: i32) -> Result<bool> {
    Ok(mapped_workspace_ids(paths)?.contains(&workspace_id))
}

fn mapped_workspace_ids(paths: &RuntimePaths) -> Result<HashSet<i32>> {
    let clients = run_hyprctl_json(paths, &["-j", "clients"])?;
    let clients = serde_json::from_slice::<Vec<ClientInfo>>(&clients).unwrap_or_default();
    Ok(clients
        .into_iter()
        .filter(|client| client.mapped && client.workspace.id > 0)
        .map(|client| client.workspace.id)
        .collect())
}

fn goto_workspace(paths: &RuntimePaths, workspace_id: i32) -> Result<()> {
    if workspace_id <= 0 {
        warn!(workspace_id, "refusing invalid workspace goto");
        append_switch_log(
            "server.hyprctl.goto.invalid",
            format!("workspace_id={workspace_id}"),
        );
        return Err(anyhow!("workspace id must be positive"));
    }

    // Unit tests run with a fake instance and must never reach a compositor.
    #[cfg(test)]
    if paths.instance_signature == "test" {
        return Ok(());
    }

    debug!(workspace_id, "dispatching workspace goto");
    append_switch_log(
        "server.hyprctl.goto",
        format!("workspace_id={workspace_id}"),
    );
    let dispatcher = workspace_goto_dispatcher(workspace_id);
    run_hyprctl_command(paths, &["dispatch", &dispatcher])
}

fn workspace_goto_dispatcher(workspace_id: i32) -> String {
    format!("hl.dsp.focus({{ workspace = {workspace_id} }})")
}

fn run_in_workspace(paths: &RuntimePaths, workspace_id: i32, argv: &[String]) -> Result<()> {
    let command = argv
        .iter()
        .map(|item| shell_escape::escape(item.as_str().into()).to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let dispatcher = workspace_exec_dispatcher(workspace_id, &command);
    run_hyprctl_command(paths, &["dispatch", &dispatcher])
}

fn workspace_exec_dispatcher(workspace_id: i32, command: &str) -> String {
    format!(
        "hl.dsp.exec_cmd({}, {{ workspace = {:?} }})",
        lua_string_literal(command),
        format!("{workspace_id} silent")
    )
}

fn lua_string_literal(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string cannot fail")
}

fn run_hyprctl_json(paths: &RuntimePaths, args: &[&str]) -> Result<Vec<u8>> {
    let mut command = Command::new("hyprctl");
    command.args(args);
    if !paths.instance_signature.is_empty() {
        command.env("HYPRLAND_INSTANCE_SIGNATURE", &paths.instance_signature);
    }

    let output = command
        .output()
        .with_context(|| format!("running hyprctl {:?}", args))?;
    if !output.status.success() {
        return Err(anyhow!("hyprctl {:?} failed with {}", args, output.status));
    }

    Ok(output.stdout)
}

fn run_hyprctl_command(paths: &RuntimePaths, args: &[&str]) -> Result<()> {
    let mut command = Command::new("hyprctl");
    command.args(args);
    if !paths.instance_signature.is_empty() {
        command.env("HYPRLAND_INSTANCE_SIGNATURE", &paths.instance_signature);
    }

    let output = command
        .output()
        .with_context(|| format!("running hyprctl {:?}", args))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if let Some(error) = hyprctl_command_error(output.status.success(), &stdout, &stderr) {
        warn!(args = ?args, status = %output.status, %error, "hyprctl command failed");
        append_switch_log(
            "server.hyprctl.error",
            format!("args={:?} status={} error={error:?}", args, output.status),
        );
        return Err(anyhow!("hyprctl {:?} failed: {error}", args));
    }

    debug!(args = ?args, status = %output.status, "hyprctl command succeeded");
    append_switch_log(
        "server.hyprctl.success",
        format!("args={:?} status={}", args, output.status),
    );
    Ok(())
}

fn hyprctl_command_error(status_success: bool, stdout: &str, stderr: &str) -> Option<String> {
    let stdout = stdout.trim();
    let stderr = stderr.trim();
    let textual_error = [stdout, stderr]
        .into_iter()
        .find(|text| text.to_ascii_lowercase().starts_with("error:"));

    if status_success && textual_error.is_none() {
        return None;
    }

    Some(
        textual_error
            .or_else(|| (!stderr.is_empty()).then_some(stderr))
            .or_else(|| (!stdout.is_empty()).then_some(stdout))
            .unwrap_or("hyprctl exited unsuccessfully")
            .to_owned(),
    )
}

fn resolve_explicit_or_default(
    env: Option<&str>,
    cwd: Option<&str>,
    store: &StateStore,
) -> Result<String> {
    if let Some(env) = env.filter(|value| !value.is_empty()) {
        return Ok(env.to_owned());
    }

    resolve_or_default_environment(None, cwd).or_else(|_| {
        store
            .locked_environment()?
            .ok_or_else(|| anyhow!("no environment specified and no global lock is active"))
    })
}

fn resolve_required_environment(env: Option<&str>, store: &StateStore) -> Result<String> {
    if let Some(env) = env.filter(|value| !value.is_empty()) {
        return Ok(env.to_owned());
    }

    store
        .locked_environment()?
        .ok_or_else(|| anyhow!("no environment specified and no global lock is active"))
}

fn resolve_or_default_environment(env: Option<&str>, cwd: Option<&str>) -> Result<String> {
    if let Some(env) = env.filter(|value| !value.is_empty()) {
        return Ok(env.to_owned());
    }

    resolve_environment_from_cwd(cwd.unwrap_or("."))
}

fn resolve_environment_from_cwd(cwd: &str) -> Result<String> {
    fs::canonicalize(cwd)
        .with_context(|| format!("canonicalizing environment path {cwd}"))
        .map(|path| path.to_string_lossy().into_owned())
}

fn default_display_id(explicit_env: Option<&str>, resolved_env: &str) -> String {
    explicit_env
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            Path::new(resolved_env)
                .file_name()
                .and_then(|value| value.to_str())
                .map(ToOwned::to_owned)
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| resolved_env.to_owned())
}

fn ensure_positive_slot(slot: i32) -> Result<()> {
    if slot <= 0 {
        return Err(anyhow!("slot must be positive"));
    }

    Ok(())
}

fn send_plugin_spawn_request(paths: &RuntimePaths, request: &PluginSpawnRequest<'_>) -> Result<()> {
    send_plugin_spawn_value(paths, request).map(|_| ())
}

fn send_plugin_spawn_value(paths: &RuntimePaths, request: &PluginSpawnRequest<'_>) -> Result<serde_json::Value> {
    let mut stream = UnixStream::connect(&paths.spawn_socket_path)
        .with_context(|| format!("connecting to {}", paths.spawn_socket_path.display()))?;
    let payload = serde_json::to_vec(request).context("encoding plugin spawn request")?;
    stream.write_all(&payload)?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.trim().is_empty() {
        return Err(anyhow!("plugin spawn socket returned empty response"));
    }

    let response = serde_json::from_str::<PluginSpawnResponse>(&line)
        .context("decoding plugin spawn response")?;
    if response.ok {
        Ok(response.result.unwrap_or(serde_json::Value::Null))
    } else {
        Err(anyhow!(
            "{}",
            response
                .error
                .map(|error| error.message)
                .unwrap_or_else(|| "plugin spawn request failed".to_owned())
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{EnvironmentRecord, SlotBindingKind, StateStore};
    use crate::protocol::{
        BatchMutationRequest, BatchMutationResponse, Request, SlotAssignmentMode,
    };
    use crate::runtime_paths::RuntimePaths;
    use crate::spawn::SpawnRegistry;
    use std::collections::HashSet;
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process;
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn binding(env_id: &str, slot_index: i32, workspace_id: i32) -> SlotBindingRecord {
        SlotBindingRecord {
            env_id: env_id.to_owned(),
            display_id: env_id.to_owned(),
            slot_index,
            display_name: None,
            binding_kind: SlotBindingKind::Fixed,
            workspace_id: Some(workspace_id),
            launch_argv: Some(vec!["ghostty".to_owned()]),
        }
    }

    fn inherit_binding(env_id: &str, slot_index: i32) -> SlotBindingRecord {
        SlotBindingRecord {
            env_id: env_id.to_owned(),
            display_id: env_id.to_owned(),
            slot_index,
            display_name: None,
            binding_kind: SlotBindingKind::Inherit,
            workspace_id: None,
            launch_argv: None,
        }
    }

    fn test_db_path(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            env::temp_dir().join(format!("hyprnav-server-{label}-{}-{unique}", process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.sqlite3");
        fs::File::create(&path).unwrap();
        path
    }

    fn cleanup(path: &Path) {
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    fn test_runtime(label: &str) -> (Arc<ServerRuntime>, PathBuf) {
        let db_path = test_db_path(label);
        let root = db_path.parent().unwrap().to_path_buf();
        let runtime_root = root.join("runtime");
        let runtime_dir = crate::runtime_paths::runtime_directory(&runtime_root, "test");
        fs::create_dir_all(&runtime_dir).unwrap();

        let runtime = Arc::new(ServerRuntime {
            paths: RuntimePaths {
                runtime_root: runtime_root.clone(),
                instance_signature: "test".to_owned(),
                runtime_dir: runtime_dir.clone(),
                spawn_socket_path: runtime_dir.join("spawn.sock"),
                switcher_socket_path: runtime_dir.join("switcher.sock"),
                grid_socket_path: runtime_dir.join("grid.sock"),
                server_socket_path: runtime_dir.join("hyprnav.sock"),
                events_socket_path: runtime_dir.join("events.sock"),
                frames_socket_path: runtime_dir.join("frames.sock"),
                hypr_event_socket_path: runtime_dir.join("hypr-events.sock"),
                switch_log_path: runtime_dir.join("switch.log"),
                state_root: root.clone(),
                state_db_path: db_path.clone(),
            },
            store: StateStore::new(&db_path).unwrap(),
            spawn_registry: Mutex::new(SpawnRegistry::new()),
            pending_launches: Mutex::new(PendingLaunchRegistry::default()),
            plugin_instance: Mutex::new(None),
            agents: Mutex::new(HashMap::new()),
            events: EventBus::new(),
        });

        (runtime, db_path)
    }

    fn environment(env_id: &str, display_id: &str) -> EnvironmentRecord {
        EnvironmentRecord {
            env_id: env_id.to_owned(),
            display_id: display_id.to_owned(),
            title: None,
            source_path: None,
            last_focused_at: 0,
        }
    }

    fn card(workspace_id: i32, subtitle: &str, active: bool) -> WorkspaceCardSnapshot {
        WorkspaceCardSnapshot {
            environment_id: None,
            workspace_id,
            slot_index: 0,
            workspace_name: workspace_id.to_string(),
            subtitle: subtitle.to_owned(),
            app_class: String::new(),
            window_count: 1,
            active,
        }
    }

    fn build_grid_snapshot_from_data(
        environments: Vec<EnvironmentRecord>,
        local_bindings: Vec<SlotBindingRecord>,
        workspace_cards: Vec<WorkspaceCardSnapshot>,
        current_workspace_id: i32,
        locked_env_id: Option<&str>,
        current_env_id: Option<&str>,
    ) -> GridSnapshot {
        let cards_by_workspace = workspace_cards
            .into_iter()
            .map(|card| (card.workspace_id, card))
            .collect::<HashMap<_, _>>();

        let mut items = Vec::new();
        let mut max_column_count = 0;
        let mut row_count = 0;

        let rows = plan_grid_rows(environments, &local_bindings, locked_env_id, current_env_id);
        for row in &rows {
            let mut row_items = Vec::new();

            for (column_index, slot) in row.slots.iter().enumerate() {
                let record = &slot.record;
                let workspace_id = record.workspace_id;
                let card = cards_by_workspace.get(&workspace_id);

                row_items.push(GridCellSnapshot {
                    workspace_name: live_display_label(
                        card.map(|item| item.subtitle.as_str()).unwrap_or_default(),
                        card.map(|item| item.app_class.as_str()).unwrap_or_default(),
                        record.display_name.as_deref(),
                        &format!("Workspace {}", workspace_id),
                    ),
                    subtitle: card
                        .map(|item| item.subtitle.clone())
                        .unwrap_or_else(|| format!("Workspace {}", workspace_id)),
                    app_class: card.map(|item| item.app_class.clone()).unwrap_or_default(),
                    window_count: card.map(|item| item.window_count).unwrap_or(0),
                    active: workspace_id == current_workspace_id,
                    ..grid_cell_from_plan(row, slot, row_count, column_index)
                });
            }

            if row_items.is_empty() {
                continue;
            }

            max_column_count = max_column_count.max(row_items.len() as i32);
            items.extend(row_items);
            row_count += 1;
        }

        let initial_index = if items.is_empty() {
            -1
        } else {
            items
                .iter()
                .position(|item| {
                    item.row_index == 0 && item.physical_workspace_id == current_workspace_id
                })
                .or_else(|| items.iter().position(|item| item.row_index == 0))
                .map(|index| index as i32)
                .unwrap_or(0)
        };

        GridSnapshot {
            items,
            initial_index,
            row_count,
            max_column_count,
        }
    }

    #[test]
    fn resolve_slot_binding_for_workspace_uses_unique_match() {
        let bindings = vec![binding("env-a", 1, 5), binding("env-b", 2, 6)];
        let (record, skipped_reason) = resolve_slot_binding_for_workspace_id(&bindings, 5);

        assert_eq!(skipped_reason, None);
        let record = record.expect("expected a resolved slot");
        assert_eq!(record.environment_id, "env-a");
        assert_eq!(record.binding_environment_id, "env-a");
        assert_eq!(record.slot_index, 1);
        assert_eq!(record.launch_argv, Some(vec!["ghostty".to_owned()]));
    }

    #[test]
    fn resolve_slot_binding_for_workspace_reports_ambiguity_without_lock() {
        let bindings = vec![binding("env-a", 1, 5), binding("env-b", 2, 5)];
        let (record, skipped_reason) = resolve_slot_binding_for_workspace_id(&bindings, 5);

        assert!(record.is_none());
        assert_eq!(
            skipped_reason,
            Some(NavigationLaunchSkippedReason::AmbiguousSlotMapping)
        );
    }

    #[test]
    fn focus_environment_uses_concrete_workspace_owner_not_inherited_locked_env() {
        let bindings = vec![binding("project", 1, 5), inherit_binding("project.task", 1)];

        assert_eq!(
            resolve_focus_environment_from_bindings(&bindings, 5),
            Some("project".to_owned())
        );
    }

    #[test]
    fn focus_environment_ignores_ambiguous_workspace_owner() {
        let bindings = vec![binding("env-a", 1, 5), binding("env-b", 2, 5)];

        assert_eq!(resolve_focus_environment_from_bindings(&bindings, 5), None);
    }

    #[test]
    fn resolve_slot_for_physical_workspace_prefers_locked_env_effective_slot() {
        let path = test_db_path("locked-effective");
        let store = StateStore::new(&path).unwrap();
        let live_workspace_ids = HashSet::new();
        store
            .assign_slot(
                "x",
                2,
                &SlotAssignmentMode::Fixed { workspace_id: 5 },
                "x",
                None,
                None,
                &live_workspace_ids,
                Some(&["ghostty".to_owned()]),
                None,
            )
            .unwrap();
        store
            .assign_slot(
                "x.y.z",
                2,
                &SlotAssignmentMode::Inherit,
                "x.y.z",
                None,
                None,
                &live_workspace_ids,
                Some(&["kitty".to_owned()]),
                None,
            )
            .unwrap();
        let bindings = store.list_local_bindings().unwrap();
        let (record, skipped_reason) =
            resolve_slot_for_physical_workspace_with_store(&store, &bindings, 5, Some("x.y.z"))
                .unwrap();

        assert_eq!(skipped_reason, None);
        let record = record.expect("expected a locked-environment effective match");
        assert_eq!(record.environment_id, "x.y.z");
        assert_eq!(record.binding_environment_id, "x");
        assert_eq!(record.command_environment_id, Some("x.y.z".to_owned()));
        assert_eq!(record.launch_argv, Some(vec!["kitty".to_owned()]));

        cleanup(&path);
    }

    #[test]
    fn slot_indexes_for_environment_include_inherited_ancestor_slots() {
        let bindings = vec![
            binding("x", 1, 5),
            binding("x.y", 2, 6),
            binding("other", 3, 7),
        ];

        assert_eq!(slot_indexes_for_environment("x.y.z", &bindings), vec![1, 2]);
        assert_eq!(
            slot_indexes_for_environment("/tmp/x.y.z", &bindings),
            Vec::<i32>::new()
        );
    }

    #[test]
    fn slot_indexes_for_environment_exclude_ancestor_temporary_slots() {
        let bindings = vec![
            binding("x", 1, 5),
            binding("x", TEMP_SLOT_START, 90),
            binding("x.y", 2, 6),
            binding("x.y", TEMP_SLOT_START, 91),
            binding("x.y", TEMP_SLOT_START + 1, 92),
        ];

        // The parent keeps its own temp, and never sees the child's.
        assert_eq!(
            slot_indexes_for_environment("x", &bindings),
            vec![1, TEMP_SLOT_START]
        );
        // The child inherits the numbered ancestor slot but not the temp, and
        // its own temps sort last.
        assert_eq!(
            slot_indexes_for_environment("x.y", &bindings),
            vec![1, 2, TEMP_SLOT_START, TEMP_SLOT_START + 1]
        );
        // A grandchild with no temps of its own shows numbered slots only.
        assert_eq!(slot_indexes_for_environment("x.y.z", &bindings), vec![1, 2]);
    }

    #[test]
    fn resolve_slot_effective_from_bindings_does_not_inherit_temporary_slots() {
        let bindings = vec![binding("x", TEMP_SLOT_START, 90)];
        let binding_index = bindings
            .iter()
            .map(|binding| ((binding.env_id.as_str(), binding.slot_index), binding))
            .collect::<HashMap<_, _>>();

        assert!(
            resolve_slot_effective_from_bindings(&binding_index, "x", TEMP_SLOT_START).is_some()
        );
        assert!(
            resolve_slot_effective_from_bindings(&binding_index, "x.y", TEMP_SLOT_START).is_none()
        );
    }

    #[test]
    fn grid_snapshot_keeps_own_temp_last_and_drops_it_from_the_child_row() {
        let snapshot = build_grid_snapshot_from_data(
            vec![environment("x", "Parent"), environment("x.y", "Child")],
            vec![
                binding("x", 1, 11),
                binding("x", TEMP_SLOT_START, 90),
                binding("x.y", 2, 22),
                binding("x.y", TEMP_SLOT_START, 91),
            ],
            vec![
                card(11, "x-1", false),
                card(90, "x-temp", false),
                card(22, "xy-2", true),
                card(91, "xy-temp", false),
            ],
            22,
            None,
            Some("x.y"),
        );

        let row = |index: i32| {
            snapshot
                .items
                .iter()
                .filter(|item| item.row_index == index)
                .collect::<Vec<_>>()
        };

        // The current environment sorts first.
        let child = row(0);
        assert_eq!(child[0].environment_id, "x.y");
        assert_eq!(
            child
                .iter()
                .map(|item| item.slot_index)
                .collect::<Vec<_>>(),
            vec![1, 2, TEMP_SLOT_START]
        );
        // Slot 1 is inherited from the parent; the parent's temp is not here.
        assert!(child[0].inherited);
        assert_eq!(child[0].physical_workspace_id, 11);
        assert!(!child[2].inherited);
        assert!(child[2].unnumbered);
        assert_eq!(child[2].physical_workspace_id, 91);

        let parent = row(1);
        assert_eq!(parent[0].environment_id, "x");
        assert_eq!(
            parent
                .iter()
                .map(|item| item.slot_index)
                .collect::<Vec<_>>(),
            vec![1, TEMP_SLOT_START]
        );
        assert!(parent[1].unnumbered);
        assert_eq!(parent[1].physical_workspace_id, 90);
    }

    fn titled(env_id: &str, title: Option<&str>, cwd: Option<&str>, focused: i64) -> EnvironmentRecord {
        EnvironmentRecord {
            env_id: env_id.to_owned(),
            display_id: env_id.to_owned(),
            title: title.map(ToOwned::to_owned),
            source_path: cwd.map(ToOwned::to_owned),
            last_focused_at: focused,
        }
    }

    fn rows_of(snapshot: &GridSnapshot) -> Vec<Vec<&GridCellSnapshot>> {
        (0..snapshot.row_count)
            .map(|index| {
                snapshot
                    .items
                    .iter()
                    .filter(|item| item.row_index == index)
                    .collect()
            })
            .collect()
    }

    fn slots_of(row: &[&GridCellSnapshot]) -> Vec<(i32, bool)> {
        row.iter().map(|cell| (cell.slot_index, cell.shared)).collect()
    }

    #[test]
    fn grid_merges_a_chain_of_three_into_one_row() {
        let snapshot = build_grid_snapshot_from_data(
            vec![
                titled("p.x", Some("Proj"), None, 0),
                titled("p.x.w", None, None, 0),
                titled("p.x.w.a.t", Some("Design"), None, 0),
            ],
            vec![
                binding("p.x", 1, 11),
                binding("p.x.w", 2, 12),
                binding("p.x.w", 3, 13),
                binding("p.x.w.a.t", 5, 15),
                binding("p.x.w.a.t", 8, 18),
            ],
            vec![],
            15,
            None,
            None,
        );
        let rows = rows_of(&snapshot);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(
            slots_of(row),
            vec![(1, true), (2, true), (3, true), (5, false), (8, false)]
        );
        assert!(row.iter().all(|cell| cell.environment_id == "p.x.w.a.t"));
        assert_eq!(row[0].owner_environment_id, "p.x");
        assert_eq!(row[0].owner_title, "Proj");
        assert_eq!(row[1].owner_environment_id, "p.x.w");
        assert_eq!(row[4].owner_environment_id, "p.x.w.a.t");
        assert!(row[0].inherited && !row[4].inherited);
        assert_eq!(row[0].environment_title, "Design");
        assert_eq!(
            row[0]
                .environment_chain
                .iter()
                .map(|level| level.id.as_str())
                .collect::<Vec<_>>(),
            vec!["p.x", "p.x.w", "p.x.w.a.t"]
        );
    }

    #[test]
    fn grid_gives_each_thread_under_a_worktree_its_own_row_with_the_shared_frames() {
        let snapshot = build_grid_snapshot_from_data(
            vec![
                titled("p.x", Some("Proj"), None, 0),
                titled("p.x.w", None, None, 0),
                titled("p.x.w.a.t", Some("Design"), None, 2),
                titled("p.x.w.b.t", Some("Other"), None, 1),
            ],
            vec![
                binding("p.x.w", 1, 11),
                binding("p.x.w", 2, 12),
                binding("p.x.w", 3, 13),
                binding("p.x.w.a.t", 5, 15),
                binding("p.x.w.a.t", 8, 18),
                binding("p.x.w.b.t", 4, 14),
            ],
            vec![],
            0,
            None,
            None,
        );
        let rows = rows_of(&snapshot);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0].environment_title, "Design");
        assert_eq!(
            slots_of(&rows[0]),
            vec![(1, true), (2, true), (3, true), (5, false), (8, false)]
        );
        assert_eq!(rows[1][0].environment_title, "Other");
        assert_eq!(
            slots_of(&rows[1]),
            vec![(1, true), (2, true), (3, true), (4, false)]
        );
        // The worktree has no row of its own.
        assert!(snapshot.items.iter().all(|cell| cell.environment_id != "p.x.w"));
    }

    #[test]
    fn grid_keeps_an_orphan_worktree_as_its_own_row() {
        let snapshot = build_grid_snapshot_from_data(
            vec![
                titled("p.x", Some("Proj"), None, 0),
                titled("p.x.w", None, Some("/home/u/src/feature-branch/"), 0),
                // A thread with no frames of its own is not a row.
                titled("p.x.w.a.t", Some("Idle thread"), None, 5),
            ],
            vec![binding("p.x.w", 1, 11), binding("p.x.w", 2, 12)],
            vec![],
            0,
            None,
            None,
        );
        let rows = rows_of(&snapshot);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].environment_id, "p.x.w");
        assert_eq!(slots_of(&rows[0]), vec![(1, false), (2, false)]);
        // Deepest titled level wins over the worktree's own cwd name.
        assert_eq!(rows[0][0].environment_title, "Proj");
        assert_eq!(rows[0][0].environment_chain[1].label, "feature-branch");
    }

    #[test]
    fn grid_row_title_falls_back_to_cwd_then_id() {
        let snapshot = build_grid_snapshot_from_data(
            vec![
                titled("q", None, Some("/srv/checkout"), 0),
                titled("r.s", None, None, 0),
            ],
            vec![binding("q", 1, 11), binding("r.s", 1, 21)],
            vec![],
            0,
            None,
            None,
        );
        let titles = rows_of(&snapshot)
            .iter()
            .map(|row| row[0].environment_title.clone())
            .collect::<Vec<_>>();
        assert!(titles.contains(&"checkout".to_owned()));
        assert!(titles.contains(&"r.s".to_owned()));
    }

    #[test]
    fn grid_row_title_picks_the_deepest_titled_level() {
        let snapshot = build_grid_snapshot_from_data(
            vec![
                titled("p.x", Some("Proj"), None, 0),
                titled("p.x.w", Some("main"), None, 0),
                titled("p.x.w.a.t", None, None, 0),
            ],
            vec![binding("p.x", 1, 11), binding("p.x.w.a.t", 5, 15)],
            vec![],
            0,
            None,
            None,
        );
        let rows = rows_of(&snapshot);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].environment_title, "main");
    }

    #[test]
    fn grid_excludes_an_ancestor_temp_from_the_merged_row() {
        let snapshot = build_grid_snapshot_from_data(
            vec![titled("p.x.w", None, None, 0), titled("p.x.w.a.t", Some("T"), None, 5)],
            vec![
                binding("p.x.w", 1, 11),
                binding("p.x.w", TEMP_SLOT_START, 90),
                binding("p.x.w.a.t", 5, 15),
                binding("p.x.w.a.t", TEMP_SLOT_START + 1, 91),
            ],
            vec![],
            0,
            None,
            None,
        );
        let rows = rows_of(&snapshot);
        let thread = rows
            .iter()
            .find(|row| row[0].environment_id == "p.x.w.a.t")
            .expect("thread row");
        assert_eq!(
            slots_of(thread),
            vec![(1, true), (5, false), (TEMP_SLOT_START + 1, false)]
        );
        assert!(thread.iter().all(|cell| cell.physical_workspace_id != 90));
        // The worktree owns a temp, so it keeps its own row to show it.
        let worktree = rows
            .iter()
            .find(|row| row[0].environment_id == "p.x.w")
            .expect("worktree row");
        assert_eq!(slots_of(worktree), vec![(1, false), (TEMP_SLOT_START, false)]);
    }

    #[test]
    fn grid_marks_every_row_under_a_locked_ancestor_and_sorts_them_first() {
        let snapshot = build_grid_snapshot_from_data(
            vec![
                titled("z", Some("Zed"), None, 100),
                titled("p.x.w", None, None, 0),
                titled("p.x.w.a.t", Some("A"), None, 1),
                titled("p.x.w.b.t", Some("B"), None, 2),
            ],
            vec![
                binding("z", 1, 31),
                binding("p.x.w", 1, 11),
                binding("p.x.w.a.t", 5, 15),
                binding("p.x.w.b.t", 4, 14),
            ],
            vec![],
            0,
            Some("p.x.w"),
            None,
        );
        let rows = rows_of(&snapshot);
        assert_eq!(
            rows.iter()
                .map(|row| row[0].environment_id.as_str())
                .collect::<Vec<_>>(),
            vec!["p.x.w.b.t", "p.x.w.a.t", "z"]
        );
        assert!(rows[0][0].environment_locked && rows[1][0].environment_locked);
        assert_eq!(rows[0][0].locked_environment_id.as_deref(), Some("p.x.w"));
        assert!(!rows[2][0].environment_locked);
        assert!(rows[0][0].environment_chain[0].locked);
    }

    #[test]
    fn grid_gives_a_locked_thread_without_frames_its_row() {
        let snapshot = build_grid_snapshot_from_data(
            vec![titled("p.x.w", None, None, 0), titled("p.x.w.a.t", Some("A"), None, 0)],
            vec![binding("p.x.w", 1, 11)],
            vec![],
            0,
            Some("p.x.w.a.t"),
            None,
        );
        let rows = rows_of(&snapshot);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].environment_id, "p.x.w.a.t");
        assert_eq!(rows[0][0].environment_title, "A");
        assert_eq!(slots_of(&rows[0]), vec![(1, true)]);
    }

    #[test]
    fn resolve_slot_effective_from_bindings_prefers_nearest_command_and_binding_sources() {
        let bindings = vec![
            binding("x", 1, 5),
            SlotBindingRecord {
                env_id: "x.y".to_owned(),
                display_id: "x.y".to_owned(),
                slot_index: 1,
                display_name: None,
                binding_kind: SlotBindingKind::Inherit,
                workspace_id: None,
                launch_argv: Some(vec!["kitty".to_owned()]),
            },
        ];
        let binding_index = bindings
            .iter()
            .map(|binding| ((binding.env_id.as_str(), binding.slot_index), binding))
            .collect::<HashMap<_, _>>();

        let record =
            resolve_slot_effective_from_bindings(&binding_index, "x.y.z", 1).expect("resolved");

        assert_eq!(record.environment_id, "x.y.z");
        assert_eq!(record.binding_environment_id, "x");
        assert_eq!(record.command_environment_id, Some("x.y".to_owned()));
        assert_eq!(record.workspace_id, 5);
        assert_eq!(record.launch_argv, Some(vec!["kitty".to_owned()]));
    }

    #[test]
    fn grid_snapshot_omits_empty_rows_and_uses_dense_row_indexes() {
        let snapshot = build_grid_snapshot_from_data(
            vec![
                environment("empty", "Empty"),
                environment("alpha", "Alpha"),
                environment("beta", "Beta"),
            ],
            vec![
                binding("alpha", 1, 11),
                binding("alpha", 3, 13),
                binding("beta", 2, 22),
            ],
            vec![
                card(11, "alpha-1", false),
                card(13, "alpha-3", false),
                card(22, "beta-2", true),
            ],
            22,
            None,
            Some("alpha"),
        );

        assert_eq!(snapshot.row_count, 2);
        assert_eq!(snapshot.max_column_count, 2);
        assert_eq!(snapshot.initial_index, 0);
        assert_eq!(snapshot.items.len(), 3);

        assert_eq!(snapshot.items[0].environment_id, "alpha");
        assert_eq!(snapshot.items[0].row_index, 0);
        assert_eq!(snapshot.items[0].column_index, 0);
        assert!(snapshot.items[0].show_environment_label);

        assert_eq!(snapshot.items[1].environment_id, "alpha");
        assert_eq!(snapshot.items[1].row_index, 0);
        assert_eq!(snapshot.items[1].column_index, 1);
        assert!(!snapshot.items[1].show_environment_label);

        assert_eq!(snapshot.items[2].environment_id, "beta");
        assert_eq!(snapshot.items[2].row_index, 1);
        assert_eq!(snapshot.items[2].column_index, 0);
        assert!(snapshot.items[2].show_environment_label);
    }

    #[test]
    fn grid_snapshot_orders_rows_by_recent_focus() {
        let mut stale = environment("alpha", "Alpha");
        stale.last_focused_at = 10;
        let mut recent = environment("beta", "Beta");
        recent.last_focused_at = 20;

        let snapshot = build_grid_snapshot_from_data(
            vec![stale, recent],
            vec![binding("alpha", 1, 11), binding("beta", 1, 22)],
            vec![card(11, "alpha-1", false), card(22, "beta-1", true)],
            22,
            Some("beta"),
            Some("alpha"),
        );

        assert_eq!(snapshot.row_count, 2);
        assert_eq!(snapshot.items[0].environment_id, "beta");
        assert_eq!(snapshot.items[0].row_index, 0);
        assert!(snapshot.items[0].environment_locked);
        assert_eq!(snapshot.items[1].environment_id, "alpha");
        assert_eq!(snapshot.items[1].row_index, 1);
    }

    #[test]
    fn grid_snapshot_prefers_top_row_for_initial_selection_after_row_compaction() {
        let mut alpha = environment("alpha", "Alpha");
        alpha.last_focused_at = 20;
        let mut beta = environment("beta", "Beta");
        beta.last_focused_at = 10;

        let snapshot = build_grid_snapshot_from_data(
            vec![alpha, beta],
            vec![binding("alpha", 1, 11), binding("beta", 1, 11)],
            vec![card(11, "shared", true)],
            11,
            Some("beta"),
            Some("alpha"),
        );

        assert_eq!(snapshot.row_count, 2);
        assert_eq!(snapshot.initial_index, 0);
        assert_eq!(snapshot.items[0].environment_id, "beta");
        assert_eq!(snapshot.items[1].environment_id, "alpha");
        assert!(snapshot.items[0].environment_locked);
    }

    #[test]
    fn grid_snapshot_moves_locked_row_above_more_recent_rows() {
        let mut project = environment("project", "Project");
        project.last_focused_at = 20;
        let mut task = environment("project.task", "Task");
        task.last_focused_at = 10;

        let mut other = environment("other", "Other");
        other.last_focused_at = 30;

        let snapshot = build_grid_snapshot_from_data(
            vec![other, task, project],
            vec![
                binding("project", 1, 5),
                inherit_binding("project.task", 1),
                binding("other", 1, 7),
            ],
            vec![card(5, "project", true)],
            5,
            Some("project.task"),
            Some("project.task"),
        );

        // The parent merges into the task's row; the locked row still sorts
        // above the more recently focused one.
        assert_eq!(snapshot.row_count, 2);
        assert_eq!(snapshot.initial_index, 0);
        assert_eq!(snapshot.items[0].environment_id, "project.task");
        assert!(snapshot.items[0].shared);
        assert_eq!(snapshot.items[1].environment_id, "other");
        assert!(snapshot.items[0].environment_locked);
    }

    #[test]
    fn grid_snapshot_moves_current_row_above_more_recent_rows() {
        let mut recent = environment("recent", "Recent");
        recent.last_focused_at = 20;
        let mut current = environment("current", "Current");
        current.last_focused_at = 10;

        let snapshot = build_grid_snapshot_from_data(
            vec![recent, current],
            vec![binding("recent", 1, 4), binding("current", 1, 5)],
            vec![card(4, "recent", false), card(5, "current", true)],
            5,
            None,
            Some("current"),
        );

        assert_eq!(snapshot.row_count, 2);
        assert_eq!(snapshot.initial_index, 0);
        assert_eq!(snapshot.items[0].environment_id, "current");
        assert_eq!(snapshot.items[1].environment_id, "recent");
    }

    #[test]
    fn pending_launch_registry_expires_entries_after_ttl() {
        let key = LaunchSlotKey {
            environment_id: "demo".to_owned(),
            slot_index: 1,
        };
        let mut registry = PendingLaunchRegistry::default();
        registry.insert(key.clone(), 7, 1_000);
        assert!(registry.contains(&key));

        registry.purge_expired(1_000 + LAUNCH_PENDING_TTL_MS - 1);
        assert!(registry.contains(&key));

        registry.purge_expired(1_000 + LAUNCH_PENDING_TTL_MS);
        assert!(!registry.contains(&key));
    }

    #[test]
    fn lua_string_literal_escapes_command_content() {
        let command = "say \"hello\" 'there' \\\nnext";
        let literal = lua_string_literal(command);
        assert_eq!(serde_json::from_str::<String>(&literal).unwrap(), command);
        assert!(literal.contains("\\\"hello\\\""));
        assert!(literal.contains("\\\\"));
        assert!(literal.contains("\\n"));
    }

    #[test]
    fn workspace_exec_dispatcher_uses_hyprland_lua_rules() {
        assert_eq!(
            workspace_exec_dispatcher(111, "ghostty --title='hello world'"),
            "hl.dsp.exec_cmd(\"ghostty --title='hello world'\", { workspace = \"111 silent\" })"
        );
    }

    #[test]
    fn workspace_goto_dispatcher_uses_hyprland_lua_api() {
        assert_eq!(
            workspace_goto_dispatcher(111),
            "hl.dsp.focus({ workspace = 111 })"
        );
    }

    #[test]
    fn hyprctl_result_classification_accepts_ok() {
        assert_eq!(hyprctl_command_error(true, "ok\n", ""), None);
    }

    #[test]
    fn hyprctl_result_classification_rejects_nonzero_status() {
        assert_eq!(
            hyprctl_command_error(false, "", "request failed\n"),
            Some("request failed".to_owned())
        );
    }

    #[test]
    fn hyprctl_result_classification_rejects_textual_error() {
        assert_eq!(
            hyprctl_command_error(true, "error: invalid dispatcher\n", ""),
            Some("error: invalid dispatcher".to_owned())
        );
    }

    #[test]
    fn pending_launch_registry_returns_keys_for_occupied_workspaces() {
        let key_a = LaunchSlotKey {
            environment_id: "demo".to_owned(),
            slot_index: 1,
        };
        let key_b = LaunchSlotKey {
            environment_id: "demo".to_owned(),
            slot_index: 2,
        };
        let mut registry = PendingLaunchRegistry::default();
        registry.insert(key_a.clone(), 7, 1_000);
        registry.insert(key_b.clone(), 8, 1_000);

        let occupied = HashSet::from([8]);
        let keys = registry.keys_for_workspaces(&occupied);

        assert_eq!(keys, vec![key_b]);
        assert!(registry.contains(&key_a));
    }

    #[test]
    fn batch_mutate_commits_all_mutations() {
        let (runtime, path) = test_runtime("batch-commit");

        let request = Request::BatchMutate {
            atomic: true,
            origin: None,
            operations: vec![
                BatchMutationRequest::EnvEnsure {
                    env: Some("demo".to_owned()),
                    cwd: None,
                    client: Some("t3code".to_owned()),
                    title: Some("Thread A".to_owned()),
                },
                BatchMutationRequest::SlotAssign {
                    env: Some("demo".to_owned()),
                    slot: 1,
                    assignment_mode: SlotAssignmentMode::Fixed { workspace_id: 5 },
                    client: Some("t3code".to_owned()),
                    cwd: None,
                    launch_argv: None,
                    display_name: Some("API".to_owned()),
                },
                BatchMutationRequest::SlotCommandSet {
                    env: Some("demo".to_owned()),
                    slot: 1,
                    argv: vec!["ghostty".to_owned()],
                    display_name: Some("API".to_owned()),
                },
                BatchMutationRequest::LockSet {
                    env: "demo".to_owned(),
                },
            ],
        };

        let result = try_handle_request(&runtime, request).unwrap();
        let response = serde_json::from_value::<BatchMutationResponse>(result).unwrap();
        assert!(response.atomic);
        assert_eq!(response.operation_count, 4);
        assert_eq!(response.results.len(), 4);

        let record = runtime
            .store
            .resolve_slot_effective("demo", 1)
            .unwrap()
            .unwrap();
        assert_eq!(record.workspace_id, 5);
        assert_eq!(record.display_name.as_deref(), Some("API"));
        assert_eq!(record.launch_argv, Some(vec!["ghostty".to_owned()]));
        assert_eq!(
            runtime.store.locked_environment().unwrap().as_deref(),
            Some("demo")
        );
        assert_eq!(
            runtime.store.list_environments().unwrap()[0]
                .title
                .as_deref(),
            Some("Thread A")
        );

        cleanup(&path);
    }

    #[test]
    fn batch_mutate_rolls_back_on_failure() {
        let (runtime, path) = test_runtime("batch-rollback");

        let error = try_handle_request(
            &runtime,
            Request::BatchMutate {
                atomic: true,
                origin: None,
                operations: vec![
                    BatchMutationRequest::EnvEnsure {
                        env: Some("demo".to_owned()),
                        cwd: None,
                        client: None,
                        title: None,
                    },
                    BatchMutationRequest::SlotAssign {
                        env: Some("demo".to_owned()),
                        slot: 1,
                        assignment_mode: SlotAssignmentMode::Fixed { workspace_id: 5 },
                        client: None,
                        cwd: None,
                        launch_argv: None,
                        display_name: Some("API".to_owned()),
                    },
                    BatchMutationRequest::SlotCommandSet {
                        env: Some("missing".to_owned()),
                        slot: 4,
                        argv: vec!["ghostty".to_owned()],
                        display_name: None,
                    },
                ],
            },
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("batch operation 2 (slot_command_set) failed"),
            "{error}"
        );
        assert!(runtime.store.list_environments().unwrap().is_empty());
        assert!(runtime
            .store
            .resolve_slot_effective("demo", 1)
            .unwrap()
            .is_none());

        cleanup(&path);
    }

    #[test]
    fn batch_mutate_rejects_best_effort_mode() {
        let (runtime, path) = test_runtime("batch-best-effort");
        let error = try_handle_request(
            &runtime,
            Request::BatchMutate {
                atomic: false,
                origin: None,
                operations: vec![BatchMutationRequest::LockClear],
            },
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("best-effort batch mode is not implemented"));
        cleanup(&path);
    }

    fn env_op(env: &str, title: Option<&str>, cwd: Option<&str>) -> BatchMutationRequest {
        BatchMutationRequest::EnvEnsure {
            env: Some(env.to_owned()),
            cwd: cwd.map(ToOwned::to_owned),
            client: None,
            title: title.map(ToOwned::to_owned),
        }
    }

    fn fixed_op(env: &str, slot: i32, workspace_id: i32) -> BatchMutationRequest {
        BatchMutationRequest::SlotAssign {
            env: Some(env.to_owned()),
            slot,
            assignment_mode: SlotAssignmentMode::Fixed { workspace_id },
            client: None,
            cwd: None,
            launch_argv: None,
            display_name: None,
        }
    }

    fn next_locked(rx: &std::sync::mpsc::Receiver<Arc<String>>) -> serde_json::Value {
        let line = rx.try_recv().expect("expected a locked event");
        println!("{}", line.trim());
        serde_json::from_str(line.trim()).unwrap()
    }

    #[test]
    fn locked_event_follows_every_lock_change_and_only_changes() {
        let (runtime, path) = test_runtime("locked-event");
        let setup = handle_request(
            &runtime,
            Request::BatchMutate {
                atomic: true,
                origin: None,
                operations: vec![
                    // Workspace 12 is bound by two worktrees: ambiguous.
                    env_op("p.x.w", Some("Worktree"), Some("/home/me/wt")),
                    env_op("p.x.w.a.t", Some("Design"), None),
                    env_op("p.x.w.b.t", Some("Other"), None),
                    // Re-ensuring an env without a cwd keeps the stored one.
                    fixed_op("p.x.w", 2, 12),
                    fixed_op("p.y.w", 2, 12),
                    fixed_op("p.x.w.a.t", 8, 18),
                ],
            },
        );
        assert!(setup.ok, "{:?}", setup.error);
        let rx = runtime.events.test_subscribe();

        // Grid cell on B's row: the shared frame locks the row's leaf.
        let goto = || Request::WorkspaceGoto {
            env: Some("p.x.w.b.t".to_owned()),
            slot: 2,
            origin: Some("hyprnav-shell".to_owned()),
        };
        assert!(handle_request(&runtime, goto()).ok);
        let event = next_locked(&rx);
        assert_eq!(event["event"], "locked");
        assert_eq!(event["cause"], "workspace_goto");
        assert_eq!(event["origin"], "hyprnav-shell");
        assert_eq!(event["locked_environment_id"], "p.x.w.b.t");
        assert!(event["previous_environment_id"].is_null());
        assert_eq!(event["environment"]["title"], "Other");
        // Existing levels only, root first; the thread inherits the worktree cwd.
        assert_eq!(event["environment"]["chain"], json!(["p.x.w", "p.x.w.b.t"]));
        assert_eq!(event["environment"]["cwd"], "/home/me/wt");
        let seq = event["seq"].as_u64().unwrap();

        // Same lock again: silent.
        assert!(handle_request(&runtime, goto()).ok);
        assert!(rx.try_recv().is_err());

        // Hyprland focus on an ambiguous workspace leaves the lock alone.
        record_workspace_focus(&runtime, 12);
        assert!(rx.try_recv().is_err());
        assert_eq!(
            runtime.store.locked_environment().unwrap().as_deref(),
            Some("p.x.w.b.t")
        );

        // Focus on A's own frame moves the lock to A.
        record_workspace_focus(&runtime, 18);
        let event = next_locked(&rx);
        assert_eq!(event["cause"], "focus");
        assert!(event["origin"].is_null());
        assert_eq!(event["locked_environment_id"], "p.x.w.a.t");
        assert_eq!(event["previous_environment_id"], "p.x.w.b.t");
        assert_eq!(event["seq"].as_u64().unwrap(), seq + 1);

        // Deleting the locked environment clears the lock.
        assert!(
            handle_request(
                &runtime,
                Request::EnvDelete {
                    env: "p.x.w.a.t".to_owned()
                }
            )
            .ok
        );
        let event = next_locked(&rx);
        assert_eq!(event["cause"], "env_delete");
        assert!(event["locked_environment_id"].is_null());
        assert!(event["environment"].is_null());

        // A worktree lock carries its cwd; locking it twice emits once.
        let lock = || Request::LockSet {
            env: "p.x.w".to_owned(),
            origin: Some("t3code".to_owned()),
        };
        assert!(handle_request(&runtime, lock()).ok);
        let event = next_locked(&rx);
        assert_eq!(event["cause"], "lock_set");
        assert_eq!(event["origin"], "t3code");
        assert_eq!(event["environment"]["cwd"], "/home/me/wt");
        assert!(handle_request(&runtime, lock()).ok);
        assert!(rx.try_recv().is_err());

        cleanup(&path);
    }
}
