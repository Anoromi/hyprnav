use crate::runtime_paths::{fallback_server_socket_paths, runtime_root};
use anyhow::{anyhow, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkspaceCardSnapshot {
    pub workspace_id: i32,
    pub workspace_name: String,
    pub subtitle: String,
    pub app_class: String,
    pub window_count: i32,
    pub active: bool,
    pub preview_path: String,
    pub generation: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SwitcherSnapshot {
    pub items: Vec<WorkspaceCardSnapshot>,
    pub initial_index: i32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GridCellSnapshot {
    pub environment_id: String,
    pub environment_display_id: String,
    pub binding_environment_id: Option<String>,
    pub command_environment_id: Option<String>,
    pub slot_index: i32,
    pub physical_workspace_id: i32,
    pub binding_kind: String,
    pub inherited: bool,
    pub workspace_name: String,
    pub subtitle: String,
    pub app_class: String,
    pub window_count: i32,
    pub active: bool,
    pub preview_path: String,
    pub generation: u64,
    pub environment_locked: bool,
    pub show_environment_label: bool,
    pub row_index: i32,
    pub column_index: i32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GridSnapshot {
    pub items: Vec<GridCellSnapshot>,
    pub initial_index: i32,
    pub row_count: i32,
    pub max_column_count: i32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StatusSnapshot {
    pub locked_environment_id: Option<String>,
    pub current_environment_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SlotResolution {
    pub environment_id: String,
    pub binding_environment_id: String,
    pub command_environment_id: Option<String>,
    pub slot_index: i32,
    pub physical_workspace_id: i32,
    pub binding_kind: String,
    pub launch_argv: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum SlotAssignmentMode {
    Fixed { workspace_id: i32 },
    Managed,
    Inherit,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum NavigationLaunchSkippedReason {
    NoLaunchConfigured,
    WorkspaceNotEmpty,
    PendingLaunch,
    NoSlotMapping,
    AmbiguousSlotMapping,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NavigationLaunchResult {
    pub configured: bool,
    pub attempted: bool,
    pub skipped_reason: Option<NavigationLaunchSkippedReason>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkspaceNavigationResult {
    pub workspace_id: i32,
    pub slot_resolution: Option<SlotResolution>,
    pub launch: NavigationLaunchResult,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SpawnPrepared {
    pub operation_id: String,
    pub workspace_id: i32,
    pub temporary: bool,
    pub origin_monitor_id: i32,
    pub origin_workspace_id: i32,
    pub origin_window_address: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SpawnStarted {
    pub operation_id: String,
    pub workspace_id: i32,
    pub root_pid: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    StatusGet {
        cwd: Option<String>,
    },
    EnvEnsure {
        env: Option<String>,
        cwd: Option<String>,
        client: Option<String>,
    },
    EnvDelete {
        env: String,
    },
    ClientEnsure {
        client: String,
    },
    SlotAssign {
        env: Option<String>,
        slot: i32,
        assignment_mode: SlotAssignmentMode,
        client: Option<String>,
        cwd: Option<String>,
        launch_argv: Option<Vec<String>>,
    },
    SlotClear {
        env: Option<String>,
        slot: i32,
        client: Option<String>,
    },
    SlotCommandSet {
        env: Option<String>,
        slot: i32,
        argv: Vec<String>,
    },
    SlotCommandClear {
        env: Option<String>,
        slot: i32,
    },
    SlotResolve {
        env: Option<String>,
        slot: i32,
    },
    LockSet {
        env: String,
    },
    LockClear,
    WorkspaceGoto {
        env: Option<String>,
        slot: i32,
    },
    WorkspaceGotoPhysical {
        workspace_id: i32,
    },
    WorkspaceRun {
        env: Option<String>,
        slot: i32,
        argv: Vec<String>,
    },
    SpawnPrepare {
        target: String,
        focus_policy: String,
    },
    SpawnStart {
        operation_id: String,
        root_pid: u32,
    },
    SpawnFinish {
        operation_id: String,
    },
    UiSnapshotSwitcher {
        reverse: bool,
    },
    UiSnapshotGrid {
        cwd: Option<String>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ErrorPayload {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Response<T> {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorPayload>,
}

impl<T> Response<T> {
    pub fn ok(result: T) -> Self {
        Self {
            ok: true,
            result: Some(result),
            error: None,
        }
    }
}

impl Response<serde_json::Value> {
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            result: None,
            error: Some(ErrorPayload {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

pub fn read_request(line: &str) -> Result<Request> {
    serde_json::from_str(line).context("parsing request")
}

pub fn write_response<T: Serialize>(writer: &mut impl Write, response: &Response<T>) -> Result<()> {
    let encoded = serde_json::to_vec(response).context("encoding response")?;
    writer.write_all(&encoded)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

pub fn send_request<R: DeserializeOwned>(path: &Path, request: &Request) -> Result<R> {
    let mut stream =
        UnixStream::connect(path).with_context(|| format!("connecting to {}", path.display()))?;
    let encoded = serde_json::to_vec(request).context("encoding request")?;
    stream.write_all(&encoded)?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.trim().is_empty() {
        return Err(anyhow!("server returned empty response"));
    }

    let response = serde_json::from_str::<Response<R>>(&line).context("decoding response")?;
    if response.ok {
        response
            .result
            .context("missing result in success response")
    } else {
        let error = response.error.unwrap_or(ErrorPayload {
            code: "unknown".to_owned(),
            message: "request failed".to_owned(),
        });
        Err(anyhow!("{}: {}", error.code, error.message))
    }
}

pub fn send_request_with_fallbacks<R: DeserializeOwned>(
    preferred_path: &Path,
    request: &Request,
) -> Result<R> {
    match send_request(preferred_path, request) {
        Ok(response) => Ok(response),
        Err(primary_error) => {
            for candidate in fallback_server_socket_paths(&runtime_root()) {
                if candidate == preferred_path {
                    continue;
                }

                if let Ok(response) = send_request(&candidate, request) {
                    return Ok(response);
                }
            }

            Err(primary_error)
        }
    }
}
