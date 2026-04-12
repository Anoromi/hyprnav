use crate::db::StateStore;
use anyhow::{anyhow, Context, Result};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const TEMP_WORKSPACE_START: i32 = 101;
pub const PREPARED_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug)]
pub enum SpawnTarget {
    Explicit(i32),
    RandomTemporary,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpawnFocusPolicy {
    Follow,
    Preserve,
}

#[derive(Clone, Debug)]
pub struct SpawnOriginSnapshot {
    pub monitor_id: i32,
    pub workspace_id: i32,
    pub window_address: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpawnOperationState {
    Prepared,
    Active,
}

#[derive(Clone, Debug)]
pub struct SpawnOperation {
    pub operation_id: String,
    pub workspace_id: i32,
    pub temporary: bool,
    pub target_monitor_id: i32,
    pub focus_policy: SpawnFocusPolicy,
    pub origin_monitor_id: i32,
    pub origin_workspace_id: i32,
    pub origin_window_address: Option<String>,
    pub root_pid: Option<u32>,
    pub state: SpawnOperationState,
    pub created_at_ms: u64,
}

#[derive(Debug, Default)]
pub struct SpawnRegistry {
    counter: AtomicU64,
    operations: Vec<SpawnOperation>,
}

impl SpawnRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn prepare(
        &mut self,
        target: SpawnTarget,
        target_monitor_id: i32,
        focus_policy: SpawnFocusPolicy,
        origin: SpawnOriginSnapshot,
        store: &StateStore,
        live_workspace_ids: &HashSet<i32>,
    ) -> Result<SpawnOperation> {
        let temporary = matches!(target, SpawnTarget::RandomTemporary);
        let workspace_id = match target {
            SpawnTarget::Explicit(workspace_id) => workspace_id,
            SpawnTarget::RandomTemporary => {
                let reserved = self
                    .operations
                    .iter()
                    .filter(|operation| operation.temporary)
                    .map(|operation| operation.workspace_id)
                    .collect::<HashSet<_>>();
                allocate_temporary_workspace_id(store, live_workspace_ids, &reserved)?
            }
        };

        let operation = SpawnOperation {
            operation_id: new_operation_id(self.counter.fetch_add(1, Ordering::Relaxed)),
            workspace_id,
            temporary,
            target_monitor_id,
            focus_policy,
            origin_monitor_id: origin.monitor_id,
            origin_workspace_id: origin.workspace_id,
            origin_window_address: origin.window_address,
            root_pid: None,
            state: SpawnOperationState::Prepared,
            created_at_ms: now_ms(),
        };

        self.operations.push(operation.clone());
        Ok(operation)
    }

    pub fn activate(&mut self, operation_id: &str, root_pid: u32) -> Result<SpawnOperation> {
        let operation = self
            .operations
            .iter_mut()
            .find(|operation| operation.operation_id == operation_id)
            .ok_or_else(|| anyhow!("unknown spawn operation {operation_id}"))?;

        if operation.state != SpawnOperationState::Prepared {
            return Err(anyhow!("spawn operation {operation_id} is already active"));
        }

        operation.root_pid = Some(root_pid);
        operation.state = SpawnOperationState::Active;
        Ok(operation.clone())
    }

    pub fn finish(&mut self, operation_id: &str) -> Option<SpawnOperation> {
        let index = self
            .operations
            .iter()
            .position(|operation| operation.operation_id == operation_id)?;
        Some(self.operations.remove(index))
    }

    pub fn expired_operations(&self, now_ms: u64) -> Vec<SpawnOperation> {
        self.operations
            .iter()
            .filter(|operation| match operation.state {
                SpawnOperationState::Prepared => now_ms.saturating_sub(operation.created_at_ms) >= PREPARED_TIMEOUT.as_millis() as u64,
                SpawnOperationState::Active => operation.root_pid.is_some_and(|pid| !pid_exists(pid)),
            })
            .cloned()
            .collect()
    }

    pub fn remove_many(&mut self, operation_ids: &HashSet<String>) -> Vec<SpawnOperation> {
        let mut removed = Vec::new();
        self.operations.retain(|operation| {
            if operation_ids.contains(&operation.operation_id) {
                removed.push(operation.clone());
                false
            } else {
                true
            }
        });
        removed
    }
}

pub fn parse_spawn_target(value: &str) -> Result<SpawnTarget> {
    if value == "rand" {
        return Ok(SpawnTarget::RandomTemporary);
    }

    let workspace_id = value
        .parse::<i32>()
        .with_context(|| format!("invalid workspace target {value}"))?;
    if workspace_id <= 0 {
        return Err(anyhow!("workspace id must be positive"));
    }

    Ok(SpawnTarget::Explicit(workspace_id))
}

pub fn parse_spawn_focus_policy(value: &str) -> Result<SpawnFocusPolicy> {
    match value {
        "follow" => Ok(SpawnFocusPolicy::Follow),
        "preserve" => Ok(SpawnFocusPolicy::Preserve),
        _ => Err(anyhow!("invalid spawn focus policy {value}")),
    }
}

pub fn pid_exists(pid: u32) -> bool {
    PathBuf::from(format!("/proc/{pid}")).exists()
}

pub fn allocate_temporary_workspace_id(
    store: &StateStore,
    live_workspace_ids: &HashSet<i32>,
    reserved_temporary_ids: &HashSet<i32>,
) -> Result<i32> {
    let mut blocked = live_workspace_ids.clone();
    blocked.extend(
        store
            .list_bindings()?
            .into_iter()
            .map(|binding| binding.workspace_id),
    );
    blocked.extend(reserved_temporary_ids.iter().copied());

    let mut workspace_id = TEMP_WORKSPACE_START;
    while blocked.contains(&workspace_id) {
        workspace_id += 1;
    }

    Ok(workspace_id)
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn new_operation_id(counter: u64) -> String {
    format!("sp-{}-{}", now_ms(), counter + 1)
}

pub fn exec_command(argv: &[String]) -> Result<()> {
    if argv.is_empty() {
        return Err(anyhow!("spawn-internal requires a command"));
    }

    let mut cstrings = argv
        .iter()
        .map(|arg| std::ffi::CString::new(arg.as_str()).context("spawn arguments cannot contain NUL bytes"))
        .collect::<Result<Vec<_>>>()?;
    let mut argv_ptrs = cstrings.iter().map(|arg| arg.as_ptr()).collect::<Vec<_>>();
    argv_ptrs.push(std::ptr::null());

    let rc = unsafe { libc::execvp(cstrings[0].as_ptr(), argv_ptrs.as_ptr()) };
    if rc == -1 {
        Err(anyhow!(
            "execvp failed for {}: {}",
            argv[0],
            std::io::Error::last_os_error()
        ))
    } else {
        Ok(())
    }
}

pub fn current_pid() -> u32 {
    std::process::id()
}
