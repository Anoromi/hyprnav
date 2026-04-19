use crate::db::{
    environment_chain, environment_has_parent, SlotBindingKind, SlotBindingRecord, StateStore,
};
use crate::protocol::{
    read_request, write_response, GridCellSnapshot, GridSnapshot, NavigationLaunchResult,
    NavigationLaunchSkippedReason, Request, Response, SlotAssignmentMode, SlotResolution,
    SpawnPrepared, SpawnStarted, StatusSnapshot, SwitcherSnapshot, WorkspaceCardSnapshot,
    WorkspaceNavigationResult,
};
use crate::runtime_paths::{ensure_parent_dir, preview_path, resolve_runtime_paths, RuntimePaths};
use crate::spawn::{
    now_ms, parse_spawn_focus_policy, parse_spawn_target, SpawnFocusPolicy, SpawnOperation,
    SpawnOperationState, SpawnOriginSnapshot, SpawnRegistry,
};
use crate::workspace_utils::{build_workspace_descriptors, initial_selection_index};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tracing::warn;

const PREVIEW_POLL_INTERVAL_MS: u64 = 15_000;
const PREVIEW_WORKER_SLEEP_MS: u64 = 1_000;
const PREVIEW_SOCKET_READ_TIMEOUT_MS: u64 = 200;
const LAUNCH_PENDING_TTL_MS: u64 = 30_000;

#[derive(Clone, Debug)]
struct WorkspaceCardData {
    workspace_id: i32,
    workspace_name: String,
    subtitle: String,
    app_class: String,
    window_count: i32,
    active: bool,
    preview_path: String,
    generation: u64,
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
    preview_refresh: Mutex<PreviewRefreshState>,
    pending_launches: Mutex<PendingLaunchRegistry>,
}

#[derive(Debug, Deserialize)]
struct PluginSpawnResponse {
    ok: bool,
    #[serde(default)]
    error: Option<PluginSpawnError>,
}

#[derive(Debug, Deserialize)]
struct PluginSpawnError {
    message: String,
}

#[derive(Debug, Default)]
struct PreviewRefreshState {
    pending_ids: HashSet<i32>,
    last_requested_ms: HashMap<i32, u64>,
    known_generations: HashMap<i32, u64>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct LaunchSlotKey {
    environment_id: String,
    slot_index: i32,
}

#[derive(Debug, Default)]
struct PendingLaunchRegistry {
    entries: HashMap<LaunchSlotKey, u64>,
}

impl PendingLaunchRegistry {
    fn purge_expired(&mut self, now_ms: u64) {
        self.entries.retain(|_, started_at_ms| {
            now_ms.saturating_sub(*started_at_ms) < LAUNCH_PENDING_TTL_MS
        });
    }

    fn contains(&self, key: &LaunchSlotKey) -> bool {
        self.entries.contains_key(key)
    }

    fn insert(&mut self, key: LaunchSlotKey, now_ms: u64) {
        self.entries.insert(key, now_ms);
    }

    fn remove(&mut self, key: &LaunchSlotKey) {
        self.entries.remove(key);
    }
}

#[derive(Debug, Deserialize)]
struct PreviewSocketEvent {
    #[serde(default)]
    event: String,
    #[serde(default, rename = "workspaceId")]
    workspace_id: i32,
    #[serde(default)]
    path: String,
    #[serde(default)]
    generation: u64,
}

#[derive(Debug, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum PluginSpawnRequest<'a> {
    Ping,
    Watch {
        operation_id: &'a str,
        workspace_id: i32,
        root_pid: u32,
        target_monitor_id: i32,
        focus_policy: &'a str,
        origin_monitor_id: i32,
        origin_workspace_id: i32,
        #[serde(skip_serializing_if = "Option::is_none")]
        origin_window_address: Option<&'a str>,
    },
    Unwatch {
        operation_id: &'a str,
    },
}

pub fn run_server() -> Result<()> {
    let paths = resolve_runtime_paths();
    let runtime = Arc::new(ServerRuntime {
        store: StateStore::new(paths.state_db_path.clone())?,
        paths,
        spawn_registry: Mutex::new(SpawnRegistry::new()),
        preview_refresh: Mutex::new(PreviewRefreshState::default()),
        pending_launches: Mutex::new(PendingLaunchRegistry::default()),
    });
    let listener = bind_listener(&runtime.paths.server_socket_path)?;
    start_spawn_cleanup_thread(runtime.clone());
    start_preview_worker_thread(runtime.clone());
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
                                    if previous_workspace_id > 0 {
                                        enqueue_preview_refresh_ids(
                                            &runtime,
                                            [previous_workspace_id],
                                        );
                                    }
                                    previous_workspace_id = next_workspace_id;
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

fn bind_listener(path: &Path) -> Result<UnixListener> {
    ensure_parent_dir(path)?;
    if path.exists() {
        let _ = fs::remove_file(path);
    }

    UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))
}

fn start_spawn_cleanup_thread(runtime: Arc<ServerRuntime>) {
    thread::spawn(move || loop {
        thread::sleep(Duration::from_millis(250));

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

        if expired.is_empty() {
            continue;
        }

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
            if operation.state == SpawnOperationState::Active {
                let _ = send_plugin_spawn_request(
                    &runtime.paths,
                    &PluginSpawnRequest::Unwatch {
                        operation_id: &operation.operation_id,
                    },
                );
            }
        }
    });
}

fn start_preview_worker_thread(runtime: Arc<ServerRuntime>) {
    thread::spawn(move || {
        let mut stream: Option<UnixStream> = None;
        let mut reader: Option<BufReader<UnixStream>> = None;

        loop {
            let now = now_ms();

            if stream.is_none() || reader.is_none() {
                if let Ok((next_stream, next_reader)) = connect_preview_socket(&runtime.paths) {
                    stream = Some(next_stream);
                    reader = Some(next_reader);
                }
            }

            let due_ids = due_preview_refresh_ids(&runtime, now);
            if let Some(active_stream) = stream.as_mut() {
                if !due_ids.is_empty() {
                    if let Err(error) = send_preview_refresh_request(active_stream, &due_ids) {
                        warn!("preview refresh request failed: {error}");
                        stream = None;
                        reader = None;
                    } else {
                        mark_preview_refresh_ids_sent(&runtime, &due_ids, now);
                    }
                }
            }

            if let Some(active_reader) = reader.as_mut() {
                if let Err(error) = drain_preview_events(&runtime, active_reader) {
                    warn!("preview socket read failed: {error}");
                    stream = None;
                    reader = None;
                }
            }

            thread::sleep(Duration::from_millis(PREVIEW_WORKER_SLEEP_MS));
        }
    });
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

fn handle_request(runtime: &Arc<ServerRuntime>, request: Request) -> Response<serde_json::Value> {
    match try_handle_request(runtime, request) {
        Ok(value) => Response::ok(value),
        Err(error) => Response::error("request_failed", error.to_string()),
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
        })?),
        Request::EnvEnsure { env, cwd, client } => {
            let resolved = resolve_or_default_environment(env.as_deref(), cwd.as_deref())?;
            let display_id = default_display_id(env.as_deref(), &resolved);
            let record = runtime.store.ensure_environment(
                &resolved,
                &display_id,
                cwd.as_deref(),
                client.as_deref(),
            )?;
            Ok(json!({
                "env_id": record.env_id,
                "display_id": record.display_id,
                "source_path": record.source_path,
            }))
        }
        Request::EnvDelete { env } => {
            runtime.store.delete_environment(&env)?;
            Ok(json!({"deleted": true, "env_id": env}))
        }
        Request::ClientEnsure { client } => {
            runtime.store.ensure_client(&client)?;
            Ok(json!({"client_id": client}))
        }
        Request::SlotAssign {
            env,
            slot,
            assignment_mode,
            client,
            cwd,
            launch_argv,
        } => {
            ensure_positive_slot(slot)?;
            let resolved_env =
                resolve_explicit_or_default(env.as_deref(), cwd.as_deref(), &runtime.store)?;
            if matches!(assignment_mode, SlotAssignmentMode::Inherit)
                && !environment_has_parent(&resolved_env)
            {
                return Err(anyhow!(
                    "slot assign --inherit requires a named dotted environment with a parent"
                ));
            }
            let display_id = default_display_id(env.as_deref(), &resolved_env);
            let live_workspace_ids = live_workspace_ids(&runtime.paths)?;
            runtime.store.assign_slot(
                &resolved_env,
                slot,
                &assignment_mode,
                &display_id,
                cwd.as_deref(),
                client.as_deref(),
                &live_workspace_ids,
                launch_argv.as_deref(),
            )?;
            slot_configuration_response(&runtime.store, &resolved_env, slot)
        }
        Request::SlotClear { env, slot, .. } => {
            ensure_positive_slot(slot)?;
            let resolved_env = resolve_required_environment(env.as_deref(), &runtime.store)?;
            runtime.store.clear_slot(&resolved_env, slot)?;
            Ok(json!({"cleared": true, "env_id": resolved_env, "slot": slot}))
        }
        Request::SlotCommandSet { env, slot, argv } => {
            ensure_positive_slot(slot)?;
            if argv.is_empty() {
                return Err(anyhow!("slot command set requires a command"));
            }
            let resolved_env = resolve_required_environment(env.as_deref(), &runtime.store)?;
            runtime
                .store
                .set_slot_launch_command(&resolved_env, slot, &argv)?;
            slot_configuration_response(&runtime.store, &resolved_env, slot)
        }
        Request::SlotCommandClear { env, slot } => {
            ensure_positive_slot(slot)?;
            let resolved_env = resolve_required_environment(env.as_deref(), &runtime.store)?;
            runtime
                .store
                .clear_slot_launch_command(&resolved_env, slot)?;
            slot_configuration_response(&runtime.store, &resolved_env, slot)
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
            Ok(serde_json::to_value(slot_resolution_from_record(record))?)
        }
        Request::LockSet { env } => {
            runtime.store.ensure_environment(
                &env,
                &default_display_id(Some(&env), &env),
                None,
                None,
            )?;
            runtime.store.set_locked_environment(&env)?;
            Ok(json!({"locked_environment_id": env}))
        }
        Request::LockClear => {
            runtime.store.clear_locked_environment()?;
            Ok(json!({"locked_environment_id": serde_json::Value::Null}))
        }
        Request::WorkspaceGoto { env, slot } => {
            ensure_positive_slot(slot)?;
            let resolved_env = resolve_required_environment(env.as_deref(), &runtime.store)?;
            let record = runtime
                .store
                .resolve_slot_effective(&resolved_env, slot)?
                .ok_or_else(|| {
                    anyhow!("slot {slot} is not assigned for environment {resolved_env}")
                })?;
            goto_workspace(&runtime.paths, record.workspace_id)?;
            let launch = attempt_slot_launch(runtime, &record)?;
            Ok(serde_json::to_value(WorkspaceNavigationResult {
                workspace_id: record.workspace_id,
                slot_resolution: Some(slot_resolution_from_record(record)),
                launch,
            })?)
        }
        Request::WorkspaceGotoPhysical { workspace_id } => {
            goto_workspace(&runtime.paths, workspace_id)?;
            let (record, skipped_reason) = resolve_slot_for_physical_workspace(
                runtime,
                workspace_id,
                runtime.store.locked_environment()?.as_deref(),
            )?;
            let launch = if let Some(record) = record.as_ref() {
                attempt_slot_launch(runtime, record)?
            } else {
                NavigationLaunchResult {
                    configured: false,
                    attempted: false,
                    skipped_reason: Some(
                        skipped_reason.unwrap_or(NavigationLaunchSkippedReason::NoSlotMapping),
                    ),
                    error: None,
                }
            };
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

            if let Err(error) = send_plugin_spawn_request(
                &runtime.paths,
                &PluginSpawnRequest::Watch {
                    operation_id: &operation.operation_id,
                    workspace_id: operation.workspace_id,
                    root_pid,
                    target_monitor_id: operation.target_monitor_id,
                    focus_policy: match operation.focus_policy {
                        SpawnFocusPolicy::Follow => "follow",
                        SpawnFocusPolicy::Preserve => "preserve",
                    },
                    origin_monitor_id: operation.origin_monitor_id,
                    origin_workspace_id: operation.origin_workspace_id,
                    origin_window_address: operation.origin_window_address.as_deref(),
                },
            ) {
                let mut registry = runtime
                    .spawn_registry
                    .lock()
                    .map_err(|poison| anyhow!("spawn registry poisoned: {poison}"))?;
                let _ = registry.finish(&operation_id);
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
                if operation.state == SpawnOperationState::Active {
                    let _ = send_plugin_spawn_request(
                        &runtime.paths,
                        &PluginSpawnRequest::Unwatch {
                            operation_id: &operation.operation_id,
                        },
                    );
                }
            }

            Ok(json!({"operation_id": operation_id, "finished": true}))
        }
        Request::UiSnapshotSwitcher { reverse } => {
            let snapshot = build_switcher_snapshot(runtime, reverse)?;
            Ok(serde_json::to_value(snapshot)?)
        }
        Request::UiSnapshotGrid { cwd } => {
            let snapshot = build_grid_snapshot(runtime, cwd.as_deref())?;
            Ok(serde_json::to_value(snapshot)?)
        }
    }
}

fn build_switcher_snapshot(runtime: &ServerRuntime, reverse: bool) -> Result<SwitcherSnapshot> {
    let descriptors = current_workspace_cards(runtime, true)?;
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

    Ok(SwitcherSnapshot {
        items: descriptors
            .into_iter()
            .map(|item| WorkspaceCardSnapshot {
                workspace_id: item.workspace_id,
                workspace_name: item.workspace_name,
                subtitle: item.subtitle,
                app_class: item.app_class,
                window_count: item.window_count,
                active: item.active,
                preview_path: item.preview_path,
                generation: item.generation,
            })
            .collect(),
        initial_index,
    })
}

fn slot_resolution_from_record(record: crate::db::SlotResolutionRecord) -> SlotResolution {
    SlotResolution {
        environment_id: record.environment_id,
        binding_environment_id: record.binding_environment_id,
        command_environment_id: record.command_environment_id,
        slot_index: record.slot_index,
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
    if let Some(record) = store.resolve_slot_effective(env_id, slot_index)? {
        let mut value = serde_json::to_value(slot_resolution_from_record(record))?;
        if let Some(object) = value.as_object_mut() {
            object.insert("resolved".to_owned(), serde_json::Value::Bool(true));
        }
        return Ok(value);
    }

    let local = store
        .get_local_slot(env_id, slot_index)?
        .ok_or_else(|| anyhow!("slot {slot_index} is not assigned for environment {env_id}"))?;
    Ok(json!({
        "environment_id": env_id,
        "binding_environment_id": serde_json::Value::Null,
        "command_environment_id": local.launch_argv.as_ref().map(|_| env_id.to_owned()),
        "slot_index": slot_index,
        "physical_workspace_id": serde_json::Value::Null,
        "binding_kind": local.binding_kind.as_str(),
        "launch_argv": local.launch_argv,
        "resolved": false,
    }))
}

fn attempt_slot_launch(
    runtime: &ServerRuntime,
    record: &crate::db::SlotResolutionRecord,
) -> Result<NavigationLaunchResult> {
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
        pending.insert(launch_key.clone(), now);
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
    let workspace_cards = current_workspace_cards(runtime, true)?;
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
    let environments = runtime.store.list_environments()?;
    let local_bindings = runtime.store.list_local_bindings()?;

    let mut rows = environments;
    let row_count = rows.len() as i32;

    rows.sort_by(|left_env, right_env| {
        row_sort_key(
            &left_env.env_id,
            &left_env.display_id,
            locked_env_id.as_deref(),
            current_env_id.as_deref(),
        )
        .cmp(&row_sort_key(
            &right_env.env_id,
            &right_env.display_id,
            locked_env_id.as_deref(),
            current_env_id.as_deref(),
        ))
    });

    let mut items = Vec::new();
    let mut initial_index = -1;
    let mut max_column_count = 0;

    for (row_index, environment) in rows.into_iter().enumerate() {
        let slot_indexes = slot_indexes_for_environment(&environment.env_id, &local_bindings);
        max_column_count = max_column_count.max(slot_indexes.len() as i32);

        for (column_index, slot_index) in slot_indexes.into_iter().enumerate() {
            let Some(record) = runtime
                .store
                .resolve_slot_effective(&environment.env_id, slot_index)?
            else {
                continue;
            };

            let workspace_id = record.workspace_id;
            let card = cards_by_workspace.get(&workspace_id);
            let preview = preview_path(
                &runtime.paths.runtime_root,
                &runtime.paths.instance_signature,
                workspace_id,
            );
            let preview_path = if let Some(card) = card {
                card.preview_path.clone()
            } else if preview.is_file() {
                preview.to_string_lossy().into_owned()
            } else {
                if workspace_id != current_workspace_id {
                    enqueue_preview_refresh_ids(runtime, [workspace_id]);
                }
                String::new()
            };
            let generation = preview_generation(runtime, workspace_id, &preview_path);
            let active = workspace_id == current_workspace_id;
            let item_index = items.len() as i32;
            if active
                && (initial_index < 0
                    || locked_env_id.as_deref() == Some(environment.env_id.as_str()))
            {
                initial_index = item_index;
            }

            items.push(GridCellSnapshot {
                environment_id: environment.env_id.clone(),
                environment_display_id: environment.display_id.clone(),
                binding_environment_id: Some(record.binding_environment_id.clone()),
                command_environment_id: record.command_environment_id.clone(),
                slot_index: record.slot_index,
                physical_workspace_id: workspace_id,
                binding_kind: record.binding_kind.as_str().to_owned(),
                inherited: record.binding_environment_id != environment.env_id,
                workspace_name: workspace_id.to_string(),
                subtitle: card
                    .map(|item| item.subtitle.clone())
                    .unwrap_or_else(|| format!("Workspace {}", workspace_id)),
                app_class: card.map(|item| item.app_class.clone()).unwrap_or_default(),
                window_count: card.map(|item| item.window_count).unwrap_or(0),
                active,
                preview_path,
                generation,
                environment_locked: locked_env_id.as_deref() == Some(environment.env_id.as_str()),
                show_environment_label: column_index == 0,
                row_index: row_index as i32,
                column_index: column_index as i32,
            });
        }
    }

    Ok(GridSnapshot {
        items,
        initial_index,
        row_count,
        max_column_count,
    })
}

fn slot_indexes_for_environment(env_id: &str, bindings: &[SlotBindingRecord]) -> Vec<i32> {
    let env_ids = environment_chain(env_id);
    let hierarchical = env_ids.len() > 1;
    let mut slot_indexes = bindings
        .iter()
        .filter(|binding| {
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

fn row_sort_key(
    env_id: &str,
    display_id: &str,
    locked_env_id: Option<&str>,
    current_env_id: Option<&str>,
) -> (i32, String, String) {
    let rank = if locked_env_id == Some(env_id) {
        0
    } else if current_env_id == Some(env_id) {
        1
    } else {
        2
    };

    (rank, display_id.to_owned(), env_id.to_owned())
}

fn current_workspace_cards(
    runtime: &ServerRuntime,
    enqueue_missing: bool,
) -> Result<Vec<WorkspaceCardData>> {
    let monitors = run_hyprctl_json(&runtime.paths, &["-j", "monitors"])?;
    let workspaces = run_hyprctl_json(&runtime.paths, &["-j", "workspaces"])?;
    let clients = run_hyprctl_json(&runtime.paths, &["-j", "clients"])?;
    let descriptors = build_workspace_descriptors(&monitors, &workspaces, &clients);

    Ok(descriptors
        .into_iter()
        .map(|descriptor| {
            let preview = preview_path(
                &runtime.paths.runtime_root,
                &runtime.paths.instance_signature,
                descriptor.id,
            );
            let preview_path = if preview.is_file() {
                preview.to_string_lossy().into_owned()
            } else {
                if enqueue_missing && !descriptor.active {
                    enqueue_preview_refresh_ids(runtime, [descriptor.id]);
                }
                String::new()
            };

            WorkspaceCardData {
                workspace_id: descriptor.id,
                workspace_name: descriptor.name,
                subtitle: descriptor.subtitle,
                app_class: descriptor.app_class,
                window_count: descriptor.window_count,
                active: descriptor.active,
                generation: preview_generation(runtime, descriptor.id, &preview_path),
                preview_path,
            }
        })
        .collect())
}

fn preview_generation(runtime: &ServerRuntime, workspace_id: i32, path: &str) -> u64 {
    runtime
        .preview_refresh
        .lock()
        .ok()
        .and_then(|state| state.known_generations.get(&workspace_id).copied())
        .filter(|generation| *generation > 0)
        .unwrap_or_else(|| preview_generation_from_file(path))
}

fn enqueue_preview_refresh_ids(
    runtime: &ServerRuntime,
    workspace_ids: impl IntoIterator<Item = i32>,
) {
    let Ok(mut state) = runtime.preview_refresh.lock() else {
        return;
    };

    for workspace_id in workspace_ids {
        if workspace_id > 0 {
            state.pending_ids.insert(workspace_id);
        }
    }
}

fn due_preview_refresh_ids(runtime: &ServerRuntime, now_ms: u64) -> Vec<i32> {
    let Ok(state) = runtime.preview_refresh.lock() else {
        return Vec::new();
    };

    state
        .pending_ids
        .iter()
        .copied()
        .filter(|workspace_id| {
            state
                .last_requested_ms
                .get(workspace_id)
                .map(|last_requested_ms| {
                    now_ms.saturating_sub(*last_requested_ms) >= PREVIEW_POLL_INTERVAL_MS
                })
                .unwrap_or(true)
        })
        .collect::<Vec<_>>()
}

fn mark_preview_refresh_ids_sent(runtime: &ServerRuntime, workspace_ids: &[i32], now_ms: u64) {
    let Ok(mut state) = runtime.preview_refresh.lock() else {
        return;
    };

    for workspace_id in workspace_ids {
        state.pending_ids.remove(workspace_id);
        state.last_requested_ms.insert(*workspace_id, now_ms);
    }
}

fn connect_preview_socket(paths: &RuntimePaths) -> Result<(UnixStream, BufReader<UnixStream>)> {
    let mut stream = UnixStream::connect(&paths.preview_socket_path)
        .with_context(|| format!("connecting to {}", paths.preview_socket_path.display()))?;
    stream.set_read_timeout(Some(Duration::from_millis(PREVIEW_SOCKET_READ_TIMEOUT_MS)))?;
    stream.write_all(b"HELLO\n")?;
    stream.flush()?;
    let reader = BufReader::new(stream.try_clone()?);
    Ok((stream, reader))
}

fn send_preview_refresh_request(stream: &mut UnixStream, workspace_ids: &[i32]) -> Result<()> {
    if workspace_ids.is_empty() {
        return Ok(());
    }

    let payload = format!(
        "REFRESH {}\n",
        workspace_ids
            .iter()
            .map(|workspace_id| workspace_id.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    );
    stream.write_all(payload.as_bytes())?;
    stream.flush()?;
    Ok(())
}

fn drain_preview_events(runtime: &ServerRuntime, reader: &mut BufReader<UnixStream>) -> Result<()> {
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return Err(anyhow!("preview socket closed")),
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                let event = serde_json::from_str::<PreviewSocketEvent>(trimmed)
                    .with_context(|| format!("decoding preview event: {trimmed}"))?;
                if event.event == "preview" && event.workspace_id > 0 {
                    let mut state = runtime
                        .preview_refresh
                        .lock()
                        .map_err(|error| anyhow!("preview refresh state poisoned: {error}"))?;
                    if event.generation > 0 {
                        state
                            .known_generations
                            .insert(event.workspace_id, event.generation);
                    } else if !event.path.is_empty() {
                        state.known_generations.insert(
                            event.workspace_id,
                            preview_generation_from_file(&event.path),
                        );
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn preview_generation_from_file(path: &str) -> u64 {
    if path.is_empty() {
        return 0;
    }

    fs::metadata(path)
        .map(|metadata| ((metadata.mtime() as u64) << 32) ^ (metadata.mtime_nsec() as u64))
        .unwrap_or(0)
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
    let clients = run_hyprctl_json(paths, &["-j", "clients"])?;
    let clients = serde_json::from_slice::<Vec<ClientInfo>>(&clients).unwrap_or_default();
    Ok(clients
        .into_iter()
        .any(|client| client.mapped && client.workspace.id == workspace_id))
}

fn goto_workspace(paths: &RuntimePaths, workspace_id: i32) -> Result<()> {
    if workspace_id <= 0 {
        return Err(anyhow!("workspace id must be positive"));
    }

    run_hyprctl_command(paths, &["dispatch", "workspace", &workspace_id.to_string()])
}

fn run_in_workspace(paths: &RuntimePaths, workspace_id: i32, argv: &[String]) -> Result<()> {
    let command = argv
        .iter()
        .map(|item| shell_escape::escape(item.as_str().into()).to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let rule = format!("[workspace {workspace_id} silent] {command}");
    run_hyprctl_command(paths, &["dispatch", "exec", &rule])
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

    let status = command
        .status()
        .with_context(|| format!("running hyprctl {:?}", args))?;
    if !status.success() {
        return Err(anyhow!("hyprctl {:?} failed with {}", args, status));
    }

    Ok(())
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
        Ok(())
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
    use crate::db::StateStore;
    use crate::protocol::SlotAssignmentMode;
    use std::collections::HashSet;
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn binding(env_id: &str, slot_index: i32, workspace_id: i32) -> SlotBindingRecord {
        SlotBindingRecord {
            env_id: env_id.to_owned(),
            display_id: env_id.to_owned(),
            slot_index,
            binding_kind: SlotBindingKind::Fixed,
            workspace_id: Some(workspace_id),
            launch_argv: Some(vec!["ghostty".to_owned()]),
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
        dir.join("state.sqlite3")
    }

    fn cleanup(path: &Path) {
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
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
    fn pending_launch_registry_expires_entries_after_ttl() {
        let key = LaunchSlotKey {
            environment_id: "demo".to_owned(),
            slot_index: 1,
        };
        let mut registry = PendingLaunchRegistry::default();
        registry.insert(key.clone(), 1_000);
        assert!(registry.contains(&key));

        registry.purge_expired(1_000 + LAUNCH_PENDING_TTL_MS - 1);
        assert!(registry.contains(&key));

        registry.purge_expired(1_000 + LAUNCH_PENDING_TTL_MS);
        assert!(!registry.contains(&key));
    }
}
