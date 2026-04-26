use crate::protocol::SlotAssignmentMode;
use crate::runtime_paths::{ensure_parent_dir, legacy_state_root};
use anyhow::{anyhow, Context, Result};
use rusqlite::{params, types::Type, Connection, OptionalExtension};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const LOCKED_ENV_KEY: &str = "locked_env_id";
const BOOT_ID_KEY: &str = "boot_id";
const MANAGED_WORKSPACE_START: i32 = 101;

#[derive(Clone, Debug)]
pub struct StateStore {
    path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct EnvironmentRecord {
    pub env_id: String,
    pub display_id: String,
    pub title: Option<String>,
    pub source_path: Option<String>,
    pub last_focused_at: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlotBindingKind {
    Fixed,
    Managed,
    Inherit,
}

impl SlotBindingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fixed => "fixed",
            Self::Managed => "managed",
            Self::Inherit => "inherit",
        }
    }

    pub fn from_str(value: &str) -> Result<Self> {
        match value {
            "fixed" => Ok(Self::Fixed),
            "managed" => Ok(Self::Managed),
            "inherit" => Ok(Self::Inherit),
            other => Err(anyhow!("unknown slot binding kind {other}")),
        }
    }

    pub fn is_concrete(self) -> bool {
        matches!(self, Self::Fixed | Self::Managed)
    }
}

#[derive(Clone, Debug)]
pub struct SlotBindingRecord {
    pub env_id: String,
    pub display_id: String,
    pub slot_index: i32,
    pub display_name: Option<String>,
    pub binding_kind: SlotBindingKind,
    pub workspace_id: Option<i32>,
    pub launch_argv: Option<Vec<String>>,
}

#[derive(Clone, Debug)]
pub struct SlotResolutionRecord {
    pub environment_id: String,
    pub binding_environment_id: String,
    pub command_environment_id: Option<String>,
    pub slot_index: i32,
    pub display_name: Option<String>,
    pub binding_kind: SlotBindingKind,
    pub workspace_id: i32,
    pub launch_argv: Option<Vec<String>>,
}

impl StateStore {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        ensure_parent_dir(&path)?;
        migrate_legacy_state_db(&path)?;
        let store = Self { path };
        store.init()?;
        store.ensure_boot_scope()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn with_transaction<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let mut connection = self.open()?;
        let transaction = connection.transaction()?;
        let result = f(&transaction)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn ensure_client(&self, client_id: &str) -> Result<()> {
        let connection = self.open()?;
        self.ensure_client_with_connection(&connection, client_id)
    }

    pub fn ensure_environment(
        &self,
        env_id: &str,
        display_id: &str,
        source_path: Option<&str>,
        client_id: Option<&str>,
        title: Option<&str>,
    ) -> Result<EnvironmentRecord> {
        let connection = self.open()?;
        self.ensure_environment_with_connection(
            &connection,
            env_id,
            display_id,
            source_path,
            client_id,
            title,
        )
    }

    pub fn delete_environment(&self, env_id: &str) -> Result<()> {
        let connection = self.open()?;
        self.delete_environment_with_connection(&connection, env_id)
    }

    pub fn set_locked_environment(&self, env_id: &str) -> Result<()> {
        let connection = self.open()?;
        self.set_locked_environment_with_connection(&connection, env_id)
    }

    pub fn clear_locked_environment(&self) -> Result<()> {
        let connection = self.open()?;
        self.clear_locked_environment_with_connection(&connection)
    }

    pub fn locked_environment(&self) -> Result<Option<String>> {
        let connection = self.open()?;
        self.locked_environment_with_connection(&connection)
    }

    pub fn record_environment_focus(&self, env_id: &str) -> Result<()> {
        let connection = self.open()?;
        self.record_environment_focus_with_connection(&connection, env_id)
    }

    pub fn set_environment_title(&self, env_id: &str, title: &str) -> Result<()> {
        let connection = self.open()?;
        self.set_environment_title_with_connection(&connection, env_id, title)
    }

    pub fn clear_environment_title(&self, env_id: &str) -> Result<()> {
        let connection = self.open()?;
        self.clear_environment_title_with_connection(&connection, env_id)
    }

    pub fn assign_slot(
        &self,
        env_id: &str,
        slot_index: i32,
        assignment_mode: &SlotAssignmentMode,
        display_id: &str,
        source_path: Option<&str>,
        client_id: Option<&str>,
        live_workspace_ids: &HashSet<i32>,
        launch_argv: Option<&[String]>,
        display_name: Option<&str>,
    ) -> Result<()> {
        let connection = self.open()?;
        self.assign_slot_with_connection(
            &connection,
            env_id,
            slot_index,
            assignment_mode,
            display_id,
            source_path,
            client_id,
            live_workspace_ids,
            launch_argv,
            display_name,
        )
    }

    pub fn clear_slot(&self, env_id: &str, slot_index: i32) -> Result<()> {
        let connection = self.open()?;
        self.clear_slot_with_connection(&connection, env_id, slot_index)
    }

    pub fn get_local_slot(
        &self,
        env_id: &str,
        slot_index: i32,
    ) -> Result<Option<SlotBindingRecord>> {
        let connection = self.open()?;
        self.get_local_slot_with_connection(&connection, env_id, slot_index)
    }

    pub fn resolve_slot_effective(
        &self,
        env_id: &str,
        slot_index: i32,
    ) -> Result<Option<SlotResolutionRecord>> {
        let connection = self.open()?;
        self.resolve_slot_effective_with_connection(&connection, env_id, slot_index)
    }

    pub fn set_slot_launch_command(
        &self,
        env_id: &str,
        slot_index: i32,
        argv: &[String],
        display_name: Option<&str>,
    ) -> Result<()> {
        let connection = self.open()?;
        self.set_slot_launch_command_with_connection(
            &connection,
            env_id,
            slot_index,
            argv,
            display_name,
        )
    }

    pub fn set_slot_display_name(
        &self,
        env_id: &str,
        slot_index: i32,
        display_name: &str,
    ) -> Result<()> {
        let connection = self.open()?;
        self.set_slot_display_name_with_connection(&connection, env_id, slot_index, display_name)
    }

    pub fn clear_slot_display_name(&self, env_id: &str, slot_index: i32) -> Result<bool> {
        let connection = self.open()?;
        self.clear_slot_display_name_with_connection(&connection, env_id, slot_index)
    }

    pub fn clear_slot_launch_command(&self, env_id: &str, slot_index: i32) -> Result<bool> {
        let connection = self.open()?;
        self.clear_slot_launch_command_with_connection(&connection, env_id, slot_index)
    }

    pub fn list_local_bindings(&self) -> Result<Vec<SlotBindingRecord>> {
        let connection = self.open()?;
        self.list_local_bindings_with_connection(&connection)
    }

    pub fn list_environments(&self) -> Result<Vec<EnvironmentRecord>> {
        let connection = self.open()?;
        self.list_environments_with_connection(&connection)
    }

    pub(crate) fn resolve_slot_effective_with_connection(
        &self,
        connection: &Connection,
        env_id: &str,
        slot_index: i32,
    ) -> Result<Option<SlotResolutionRecord>> {
        let chain = environment_chain(env_id);
        let mut binding_environment_id = None;
        let mut binding_kind = None;
        let mut workspace_id = None;
        let mut command_environment_id = None;
        let mut launch_argv = None;
        let mut display_name = None;

        for candidate_env in chain {
            let Some(local) =
                self.get_local_slot_with_connection(connection, &candidate_env, slot_index)?
            else {
                continue;
            };

            if command_environment_id.is_none() && local.launch_argv.is_some() {
                command_environment_id = Some(local.env_id.clone());
                launch_argv = local.launch_argv.clone();
            }

            if binding_environment_id.is_none() && local.binding_kind.is_concrete() {
                binding_environment_id = Some(local.env_id.clone());
                binding_kind = Some(local.binding_kind);
                workspace_id = local.workspace_id;
            }

            if display_name.is_none() && local.display_name.is_some() {
                display_name = local.display_name.clone();
            }
        }

        match (binding_environment_id, binding_kind, workspace_id) {
            (Some(binding_environment_id), Some(binding_kind), Some(workspace_id)) => {
                Ok(Some(SlotResolutionRecord {
                    environment_id: env_id.to_owned(),
                    binding_environment_id,
                    command_environment_id,
                    slot_index,
                    display_name,
                    binding_kind,
                    workspace_id,
                    launch_argv,
                }))
            }
            _ => Ok(None),
        }
    }

    fn init(&self) -> Result<()> {
        let connection = self.open()?;
        connection.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS environments (
              env_id TEXT PRIMARY KEY,
              display_id TEXT NOT NULL,
              title TEXT NULL,
              source_path TEXT NULL,
              created_by_client_id TEXT NULL,
              created_at INTEGER NOT NULL,
              updated_at INTEGER NOT NULL,
              last_focused_at INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS clients (
              client_id TEXT PRIMARY KEY,
              created_at INTEGER NOT NULL,
              updated_at INTEGER NOT NULL,
              last_seen_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS slot_bindings (
              env_id TEXT NOT NULL,
              slot_index INTEGER NOT NULL,
              display_name TEXT NULL,
              binding_kind TEXT NOT NULL CHECK(binding_kind IN ('fixed','managed','inherit')),
              workspace_id INTEGER NULL,
              launch_argv_json TEXT NULL,
              updated_by_client_id TEXT NULL,
              created_at INTEGER NOT NULL,
              updated_at INTEGER NOT NULL,
              PRIMARY KEY (env_id, slot_index)
            );

            CREATE TABLE IF NOT EXISTS global_state (
              key TEXT PRIMARY KEY,
              value TEXT NOT NULL
            );
            ",
        )?;
        migrate_environments_schema(&connection)?;
        migrate_slot_bindings_schema(&connection)?;
        Ok(())
    }

    fn ensure_boot_scope(&self) -> Result<()> {
        let current_boot_id = current_boot_id()?;
        let connection = self.open()?;
        let stored_boot_id = connection
            .query_row(
                "SELECT value FROM global_state WHERE key = ?1",
                params![BOOT_ID_KEY],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        if stored_boot_id.as_deref() != Some(current_boot_id.as_str()) {
            connection.execute("DELETE FROM slot_bindings", [])?;
            connection.execute("DELETE FROM environments", [])?;
            connection.execute("DELETE FROM clients", [])?;
            connection.execute("DELETE FROM global_state", [])?;
            connection.execute(
                "INSERT INTO global_state (key, value) VALUES (?1, ?2)",
                params![BOOT_ID_KEY, current_boot_id],
            )?;
        }

        Ok(())
    }

    fn managed_workspace_ids(&self, connection: &Connection) -> Result<HashSet<i32>> {
        let mut statement = connection.prepare(
            "SELECT workspace_id FROM slot_bindings
             WHERE binding_kind = 'managed' AND workspace_id IS NOT NULL",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, i32>(0))?;
        let ids = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids.into_iter().collect())
    }

    fn open(&self) -> Result<Connection> {
        Connection::open(&self.path).with_context(|| format!("opening {}", self.path.display()))
    }

    pub(crate) fn ensure_client_with_connection(
        &self,
        connection: &Connection,
        client_id: &str,
    ) -> Result<()> {
        let now = now_unix();
        connection.execute(
            "INSERT INTO clients (client_id, created_at, updated_at, last_seen_at)
             VALUES (?1, ?2, ?2, ?2)
             ON CONFLICT(client_id) DO UPDATE SET
               updated_at = excluded.updated_at,
               last_seen_at = excluded.last_seen_at",
            params![client_id, now],
        )?;
        Ok(())
    }

    pub(crate) fn ensure_environment_with_connection(
        &self,
        connection: &Connection,
        env_id: &str,
        display_id: &str,
        source_path: Option<&str>,
        client_id: Option<&str>,
        title: Option<&str>,
    ) -> Result<EnvironmentRecord> {
        if let Some(client_id) = client_id {
            self.ensure_client_with_connection(connection, client_id)?;
        }

        let now = now_unix();
        connection.execute(
            "INSERT INTO environments (env_id, display_id, title, source_path, created_by_client_id, created_at, updated_at, last_focused_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, 0)
             ON CONFLICT(env_id) DO UPDATE SET
               display_id = excluded.display_id,
               title = COALESCE(excluded.title, environments.title),
               source_path = excluded.source_path,
               updated_at = excluded.updated_at",
            params![env_id, display_id, title, source_path, client_id, now],
        )?;

        Ok(EnvironmentRecord {
            env_id: env_id.to_owned(),
            display_id: display_id.to_owned(),
            title: title.map(ToOwned::to_owned),
            source_path: source_path.map(ToOwned::to_owned),
            last_focused_at: 0,
        })
    }

    pub(crate) fn delete_environment_with_connection(
        &self,
        connection: &Connection,
        env_id: &str,
    ) -> Result<()> {
        connection.execute(
            "DELETE FROM slot_bindings WHERE env_id = ?1",
            params![env_id],
        )?;
        connection.execute(
            "DELETE FROM environments WHERE env_id = ?1",
            params![env_id],
        )?;
        connection.execute(
            "DELETE FROM global_state WHERE key = ?1 AND value = ?2",
            params![LOCKED_ENV_KEY, env_id],
        )?;
        Ok(())
    }

    pub(crate) fn set_locked_environment_with_connection(
        &self,
        connection: &Connection,
        env_id: &str,
    ) -> Result<()> {
        connection.execute(
            "INSERT INTO global_state (key, value)
             VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![LOCKED_ENV_KEY, env_id],
        )?;
        Ok(())
    }

    pub(crate) fn clear_locked_environment_with_connection(
        &self,
        connection: &Connection,
    ) -> Result<()> {
        connection.execute(
            "DELETE FROM global_state WHERE key = ?1",
            params![LOCKED_ENV_KEY],
        )?;
        Ok(())
    }

    pub(crate) fn locked_environment_with_connection(
        &self,
        connection: &Connection,
    ) -> Result<Option<String>> {
        connection
            .query_row(
                "SELECT value FROM global_state WHERE key = ?1",
                params![LOCKED_ENV_KEY],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub(crate) fn record_environment_focus_with_connection(
        &self,
        connection: &Connection,
        env_id: &str,
    ) -> Result<()> {
        let now = now_unix();
        connection.execute(
            "UPDATE environments
             SET last_focused_at = ?2, updated_at = ?2
             WHERE env_id = ?1",
            params![env_id, now],
        )?;
        self.set_locked_environment_with_connection(connection, env_id)
    }

    pub(crate) fn set_environment_title_with_connection(
        &self,
        connection: &Connection,
        env_id: &str,
        title: &str,
    ) -> Result<()> {
        let updated = connection.execute(
            "UPDATE environments
             SET title = ?2, updated_at = ?3
             WHERE env_id = ?1",
            params![env_id, title, now_unix()],
        )?;
        if updated == 0 {
            return Err(anyhow!("environment {env_id} does not exist"));
        }
        Ok(())
    }

    pub(crate) fn clear_environment_title_with_connection(
        &self,
        connection: &Connection,
        env_id: &str,
    ) -> Result<()> {
        let updated = connection.execute(
            "UPDATE environments
             SET title = NULL, updated_at = ?2
             WHERE env_id = ?1",
            params![env_id, now_unix()],
        )?;
        if updated == 0 {
            return Err(anyhow!("environment {env_id} does not exist"));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn assign_slot_with_connection(
        &self,
        connection: &Connection,
        env_id: &str,
        slot_index: i32,
        assignment_mode: &SlotAssignmentMode,
        display_id: &str,
        source_path: Option<&str>,
        client_id: Option<&str>,
        live_workspace_ids: &HashSet<i32>,
        launch_argv: Option<&[String]>,
        display_name: Option<&str>,
    ) -> Result<()> {
        self.ensure_environment_with_connection(
            connection,
            env_id,
            display_id,
            source_path,
            client_id,
            None,
        )?;

        let now = now_unix();
        let existing_launch_argv_json = connection
            .query_row(
                "SELECT launch_argv_json FROM slot_bindings WHERE env_id = ?1 AND slot_index = ?2",
                params![env_id, slot_index],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let existing_display_name = connection
            .query_row(
                "SELECT display_name FROM slot_bindings WHERE env_id = ?1 AND slot_index = ?2",
                params![env_id, slot_index],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let effective_launch_argv_json = match launch_argv {
            Some(argv) => Some(encode_launch_argv(argv)?),
            None => existing_launch_argv_json,
        };
        let effective_display_name = display_name
            .map(ToOwned::to_owned)
            .or(existing_display_name);

        let (binding_kind, workspace_id) = match assignment_mode {
            SlotAssignmentMode::Fixed { workspace_id } => {
                (SlotBindingKind::Fixed, Some(*workspace_id))
            }
            SlotAssignmentMode::Managed => {
                let workspace_id = if let Some(existing) = connection
                    .query_row(
                        "SELECT workspace_id FROM slot_bindings
                         WHERE env_id = ?1 AND slot_index = ?2 AND binding_kind = 'managed'",
                        params![env_id, slot_index],
                        |row| row.get::<_, Option<i32>>(0),
                    )
                    .optional()?
                    .flatten()
                {
                    existing
                } else {
                    let reserved = self.managed_workspace_ids(connection)?;
                    allocate_managed_workspace_id(&reserved, live_workspace_ids)
                };
                (SlotBindingKind::Managed, Some(workspace_id))
            }
            SlotAssignmentMode::Inherit => (SlotBindingKind::Inherit, None),
        };

        connection.execute(
            "INSERT INTO slot_bindings (env_id, slot_index, display_name, binding_kind, workspace_id, launch_argv_json, updated_by_client_id, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
             ON CONFLICT(env_id, slot_index) DO UPDATE SET
               display_name = excluded.display_name,
               binding_kind = excluded.binding_kind,
               workspace_id = excluded.workspace_id,
               launch_argv_json = excluded.launch_argv_json,
               updated_by_client_id = excluded.updated_by_client_id,
               updated_at = excluded.updated_at",
            params![
                env_id,
                slot_index,
                effective_display_name,
                binding_kind.as_str(),
                workspace_id,
                effective_launch_argv_json,
                client_id,
                now
            ],
        )?;

        Ok(())
    }

    pub(crate) fn clear_slot_with_connection(
        &self,
        connection: &Connection,
        env_id: &str,
        slot_index: i32,
    ) -> Result<()> {
        connection.execute(
            "DELETE FROM slot_bindings WHERE env_id = ?1 AND slot_index = ?2",
            params![env_id, slot_index],
        )?;
        Ok(())
    }

    pub(crate) fn get_local_slot_with_connection(
        &self,
        connection: &Connection,
        env_id: &str,
        slot_index: i32,
    ) -> Result<Option<SlotBindingRecord>> {
        connection
            .query_row(
                "SELECT slot_bindings.env_id, environments.display_id, slot_bindings.slot_index, slot_bindings.display_name, slot_bindings.binding_kind, slot_bindings.workspace_id, slot_bindings.launch_argv_json
                 FROM slot_bindings
                 JOIN environments ON environments.env_id = slot_bindings.env_id
                 WHERE slot_bindings.env_id = ?1 AND slot_bindings.slot_index = ?2",
                params![env_id, slot_index],
                decode_slot_binding_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub(crate) fn set_slot_launch_command_with_connection(
        &self,
        connection: &Connection,
        env_id: &str,
        slot_index: i32,
        argv: &[String],
        display_name: Option<&str>,
    ) -> Result<()> {
        let effective_display_name = match display_name {
            Some(display_name) => Some(display_name.to_owned()),
            None => connection
                .query_row(
                    "SELECT display_name FROM slot_bindings WHERE env_id = ?1 AND slot_index = ?2",
                    params![env_id, slot_index],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten(),
        };
        let updated = connection.execute(
            "UPDATE slot_bindings
             SET launch_argv_json = ?3, display_name = ?4, updated_at = ?5
             WHERE env_id = ?1 AND slot_index = ?2",
            params![
                env_id,
                slot_index,
                encode_launch_argv(argv)?,
                effective_display_name,
                now_unix()
            ],
        )?;
        if updated == 0 {
            return Err(anyhow!(
                "slot {slot_index} is not assigned for environment {env_id}; create a local slot first (for example with `slot assign --inherit`)"
            ));
        }
        Ok(())
    }

    pub(crate) fn set_slot_display_name_with_connection(
        &self,
        connection: &Connection,
        env_id: &str,
        slot_index: i32,
        display_name: &str,
    ) -> Result<()> {
        let updated = connection.execute(
            "UPDATE slot_bindings
             SET display_name = ?3, updated_at = ?4
             WHERE env_id = ?1 AND slot_index = ?2",
            params![env_id, slot_index, display_name, now_unix()],
        )?;
        if updated == 0 {
            return Err(anyhow!(
                "slot {slot_index} is not assigned for environment {env_id}; create a local slot first"
            ));
        }
        Ok(())
    }

    pub(crate) fn clear_slot_display_name_with_connection(
        &self,
        connection: &Connection,
        env_id: &str,
        slot_index: i32,
    ) -> Result<bool> {
        let updated = connection.execute(
            "UPDATE slot_bindings
             SET display_name = NULL, updated_at = ?3
             WHERE env_id = ?1 AND slot_index = ?2",
            params![env_id, slot_index, now_unix()],
        )?;
        Ok(updated > 0)
    }

    pub(crate) fn clear_slot_launch_command_with_connection(
        &self,
        connection: &Connection,
        env_id: &str,
        slot_index: i32,
    ) -> Result<bool> {
        let updated = connection.execute(
            "UPDATE slot_bindings
             SET launch_argv_json = NULL, updated_at = ?3
             WHERE env_id = ?1 AND slot_index = ?2",
            params![env_id, slot_index, now_unix()],
        )?;
        Ok(updated > 0)
    }

    pub(crate) fn list_local_bindings_with_connection(
        &self,
        connection: &Connection,
    ) -> Result<Vec<SlotBindingRecord>> {
        let mut statement = connection.prepare(
            "SELECT slot_bindings.env_id, environments.display_id, slot_bindings.slot_index, slot_bindings.display_name, slot_bindings.binding_kind, slot_bindings.workspace_id, slot_bindings.launch_argv_json
             FROM slot_bindings
             JOIN environments ON environments.env_id = slot_bindings.env_id
             ORDER BY environments.display_id, slot_bindings.env_id, slot_bindings.slot_index",
        )?;

        let rows = statement.query_map([], decode_slot_binding_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub(crate) fn list_environments_with_connection(
        &self,
        connection: &Connection,
    ) -> Result<Vec<EnvironmentRecord>> {
        let mut statement = connection.prepare(
            "SELECT env_id, display_id, source_path
             , title, last_focused_at
             FROM environments
             ORDER BY last_focused_at DESC, display_id, env_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(EnvironmentRecord {
                env_id: row.get(0)?,
                display_id: row.get(1)?,
                source_path: row.get(2)?,
                title: row.get(3)?,
                last_focused_at: row.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

pub fn environment_chain(env_id: &str) -> Vec<String> {
    if env_id.contains('/') {
        return vec![env_id.to_owned()];
    }

    let mut chain = Vec::new();
    let mut current = env_id.trim();
    if current.is_empty() {
        return chain;
    }

    chain.push(current.to_owned());
    while let Some((parent, _)) = current.rsplit_once('.') {
        if parent.is_empty() {
            break;
        }
        chain.push(parent.to_owned());
        current = parent;
    }

    chain
}

pub fn environment_has_parent(env_id: &str) -> bool {
    environment_chain(env_id).len() > 1
}

fn decode_slot_binding_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SlotBindingRecord> {
    Ok(SlotBindingRecord {
        env_id: row.get(0)?,
        display_id: row.get(1)?,
        slot_index: row.get(2)?,
        display_name: row.get(3)?,
        binding_kind: decode_slot_binding_kind_column(row, 4)?,
        workspace_id: row.get(5)?,
        launch_argv: decode_launch_argv_column(row, 6)?,
    })
}

fn current_boot_id() -> Result<String> {
    if let Some(override_boot_id) = std::env::var_os("HYPRNAV_BOOT_ID_OVERRIDE") {
        let override_boot_id = override_boot_id.to_string_lossy().trim().to_owned();
        if !override_boot_id.is_empty() {
            return Ok(override_boot_id);
        }
    }

    Ok(fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .context("reading current boot id")?
        .trim()
        .to_owned())
}

fn migrate_legacy_state_db(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }

    let legacy_path = legacy_state_root().join("state.sqlite3");
    if !legacy_path.is_file() {
        return Ok(());
    }

    ensure_parent_dir(path)?;
    fs::copy(&legacy_path, path).with_context(|| {
        format!(
            "copying legacy state database from {} to {}",
            legacy_path.display(),
            path.display()
        )
    })?;
    Ok(())
}

fn allocate_managed_workspace_id(
    reserved: &HashSet<i32>,
    live_workspace_ids: &HashSet<i32>,
) -> i32 {
    let mut candidate = MANAGED_WORKSPACE_START;
    loop {
        if !reserved.contains(&candidate) && !live_workspace_ids.contains(&candidate) {
            return candidate;
        }

        candidate += 1;
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs() as i64)
        .unwrap_or_default()
}

fn migrate_slot_bindings_schema(connection: &Connection) -> Result<()> {
    let table_sql = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'slot_bindings'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(table_sql) = table_sql else {
        return Ok(());
    };

    let mut statement = connection.prepare("PRAGMA table_info(slot_bindings)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let has_launch_argv_json = columns.iter().any(|column| column == "launch_argv_json");
    let has_display_name = columns.iter().any(|column| column == "display_name");
    let needs_rebuild =
        !table_sql.contains("'inherit'") || table_sql.contains("workspace_id INTEGER NOT NULL");

    if !needs_rebuild {
        if !has_launch_argv_json {
            connection.execute(
                "ALTER TABLE slot_bindings ADD COLUMN launch_argv_json TEXT NULL",
                [],
            )?;
        }
        if !has_display_name {
            connection.execute(
                "ALTER TABLE slot_bindings ADD COLUMN display_name TEXT NULL",
                [],
            )?;
        }
        return Ok(());
    }

    connection.execute("ALTER TABLE slot_bindings RENAME TO slot_bindings_old", [])?;
    connection.execute_batch(
        "
        CREATE TABLE slot_bindings (
          env_id TEXT NOT NULL,
          slot_index INTEGER NOT NULL,
          display_name TEXT NULL,
          binding_kind TEXT NOT NULL CHECK(binding_kind IN ('fixed','managed','inherit')),
          workspace_id INTEGER NULL,
          launch_argv_json TEXT NULL,
          updated_by_client_id TEXT NULL,
          created_at INTEGER NOT NULL,
          updated_at INTEGER NOT NULL,
          PRIMARY KEY (env_id, slot_index)
        );
        ",
    )?;

    if has_launch_argv_json {
        if has_display_name {
            connection.execute(
                "INSERT INTO slot_bindings (env_id, slot_index, display_name, binding_kind, workspace_id, launch_argv_json, updated_by_client_id, created_at, updated_at)
                 SELECT env_id, slot_index, display_name, binding_kind, workspace_id, launch_argv_json, updated_by_client_id, created_at, updated_at
                 FROM slot_bindings_old",
                [],
            )?;
        } else {
            connection.execute(
                "INSERT INTO slot_bindings (env_id, slot_index, display_name, binding_kind, workspace_id, launch_argv_json, updated_by_client_id, created_at, updated_at)
                 SELECT env_id, slot_index, NULL, binding_kind, workspace_id, launch_argv_json, updated_by_client_id, created_at, updated_at
                 FROM slot_bindings_old",
                [],
            )?;
        }
    } else {
        if has_display_name {
            connection.execute(
                "INSERT INTO slot_bindings (env_id, slot_index, display_name, binding_kind, workspace_id, launch_argv_json, updated_by_client_id, created_at, updated_at)
                 SELECT env_id, slot_index, display_name, binding_kind, workspace_id, NULL, updated_by_client_id, created_at, updated_at
                 FROM slot_bindings_old",
                [],
            )?;
        } else {
            connection.execute(
                "INSERT INTO slot_bindings (env_id, slot_index, display_name, binding_kind, workspace_id, launch_argv_json, updated_by_client_id, created_at, updated_at)
                 SELECT env_id, slot_index, NULL, binding_kind, workspace_id, NULL, updated_by_client_id, created_at, updated_at
                 FROM slot_bindings_old",
                [],
            )?;
        }
    }

    connection.execute("DROP TABLE slot_bindings_old", [])?;
    Ok(())
}

fn migrate_environments_schema(connection: &Connection) -> Result<()> {
    let mut statement = connection.prepare("PRAGMA table_info(environments)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    if !columns.iter().any(|column| column == "title") {
        connection.execute("ALTER TABLE environments ADD COLUMN title TEXT NULL", [])?;
    }

    if !columns.iter().any(|column| column == "last_focused_at") {
        connection.execute(
            "ALTER TABLE environments ADD COLUMN last_focused_at INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }

    Ok(())
}

fn encode_launch_argv(argv: &[String]) -> Result<String> {
    serde_json::to_string(argv).context("encoding slot launch argv")
}

fn decode_launch_argv_json(value: Option<&str>) -> Result<Option<Vec<String>>> {
    value
        .map(|json| serde_json::from_str::<Vec<String>>(json).context("decoding slot launch argv"))
        .transpose()
}

fn decode_slot_binding_kind_column(
    row: &rusqlite::Row<'_>,
    column_index: usize,
) -> rusqlite::Result<SlotBindingKind> {
    let value = row.get::<_, String>(column_index)?;
    SlotBindingKind::from_str(&value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column_index,
            Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                error.to_string(),
            )),
        )
    })
}

fn decode_launch_argv_column(
    row: &rusqlite::Row<'_>,
    column_index: usize,
) -> rusqlite::Result<Option<Vec<String>>> {
    let value = row.get::<_, Option<String>>(column_index)?;
    value
        .map(|json| {
            serde_json::from_str::<Vec<String>>(&json).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(column_index, Type::Text, Box::new(error))
            })
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::process;

    fn test_db_path(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = env::temp_dir().join(format!("hyprnav-{label}-{}-{unique}", process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir.join("state.sqlite3")
    }

    fn cleanup(path: &Path) {
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    fn fixed(workspace_id: i32) -> SlotAssignmentMode {
        SlotAssignmentMode::Fixed { workspace_id }
    }

    #[test]
    fn environment_chain_for_named_and_path_envs() {
        assert_eq!(
            environment_chain("x.y.z"),
            vec!["x.y.z".to_owned(), "x.y".to_owned(), "x".to_owned()]
        );
        assert_eq!(environment_chain("x"), vec!["x".to_owned()]);
        assert_eq!(
            environment_chain("/home/a/project"),
            vec!["/home/a/project".to_owned()]
        );
    }

    #[test]
    fn init_migrates_legacy_slot_schema_without_losing_rows() {
        let path = test_db_path("migrate");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "
                CREATE TABLE environments (
                  env_id TEXT PRIMARY KEY,
                  display_id TEXT NOT NULL,
                  source_path TEXT NULL,
                  created_by_client_id TEXT NULL,
                  created_at INTEGER NOT NULL,
                  updated_at INTEGER NOT NULL
                );
                CREATE TABLE clients (
                  client_id TEXT PRIMARY KEY,
                  created_at INTEGER NOT NULL,
                  updated_at INTEGER NOT NULL,
                  last_seen_at INTEGER NOT NULL
                );
                CREATE TABLE slot_bindings (
                  env_id TEXT NOT NULL,
                  slot_index INTEGER NOT NULL,
                  binding_kind TEXT NOT NULL CHECK(binding_kind IN ('fixed','managed')),
                  workspace_id INTEGER NOT NULL,
                  updated_by_client_id TEXT NULL,
                  created_at INTEGER NOT NULL,
                  updated_at INTEGER NOT NULL,
                  PRIMARY KEY (env_id, slot_index)
                );
                CREATE TABLE global_state (
                  key TEXT PRIMARY KEY,
                  value TEXT NOT NULL
                );
                INSERT INTO environments (env_id, display_id, created_at, updated_at)
                VALUES ('demo', 'demo', 1, 1);
                INSERT INTO slot_bindings (env_id, slot_index, binding_kind, workspace_id, created_at, updated_at)
                VALUES ('demo', 1, 'fixed', 5, 1, 1);
                ",
            )
            .unwrap();
        drop(connection);

        let store = StateStore::new(&path).unwrap();
        let connection = store.open().unwrap();
        let mut statement = connection
            .prepare("PRAGMA table_info(slot_bindings)")
            .unwrap();
        let columns = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, i32>(3)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(columns.iter().any(|(name, _)| name == "launch_argv_json"));
        assert!(columns
            .iter()
            .any(|(name, notnull)| name == "workspace_id" && *notnull == 0));

        let sql = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'slot_bindings'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        assert!(sql.contains("'inherit'"));

        let record = store.resolve_slot_effective("demo", 1).unwrap().unwrap();
        assert_eq!(record.workspace_id, 5);
        assert_eq!(record.binding_kind, SlotBindingKind::Fixed);
        assert_eq!(record.launch_argv, None);

        cleanup(&path);
    }

    #[test]
    fn local_slot_queries_decode_display_name_before_binding_columns() {
        let path = test_db_path("local-slot-query-order");
        let store = StateStore::new(&path).unwrap();
        let live_workspace_ids = HashSet::new();
        store
            .assign_slot(
                "demo",
                1,
                &SlotAssignmentMode::Managed,
                "demo",
                None,
                None,
                &live_workspace_ids,
                None,
                Some("API"),
            )
            .unwrap();

        let local = store.get_local_slot("demo", 1).unwrap().unwrap();
        assert_eq!(local.binding_kind, SlotBindingKind::Managed);
        assert_eq!(local.display_name.as_deref(), Some("API"));
        assert!(local.workspace_id.is_some());

        let bindings = store.list_local_bindings().unwrap();
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].binding_kind, SlotBindingKind::Managed);
        assert_eq!(bindings[0].display_name.as_deref(), Some("API"));

        cleanup(&path);
    }

    #[test]
    fn assign_slot_preserves_existing_launch_command_when_not_replaced() {
        let path = test_db_path("preserve");
        let store = StateStore::new(&path).unwrap();
        let live_workspace_ids = HashSet::new();
        store
            .assign_slot(
                "demo",
                1,
                &fixed(5),
                "demo",
                None,
                None,
                &live_workspace_ids,
                Some(&[
                    "ghostty".to_owned(),
                    "--class".to_owned(),
                    "work".to_owned(),
                ]),
                None,
            )
            .unwrap();

        store
            .assign_slot(
                "demo",
                1,
                &fixed(6),
                "demo",
                None,
                None,
                &live_workspace_ids,
                None,
                None,
            )
            .unwrap();

        let record = store.resolve_slot_effective("demo", 1).unwrap().unwrap();
        assert_eq!(record.workspace_id, 6);
        assert_eq!(
            record.launch_argv,
            Some(vec![
                "ghostty".to_owned(),
                "--class".to_owned(),
                "work".to_owned()
            ])
        );

        cleanup(&path);
    }

    #[test]
    fn assign_slot_replaces_existing_launch_command_when_requested() {
        let path = test_db_path("replace");
        let store = StateStore::new(&path).unwrap();
        let live_workspace_ids = HashSet::new();
        store
            .assign_slot(
                "demo",
                1,
                &fixed(5),
                "demo",
                None,
                None,
                &live_workspace_ids,
                Some(&["ghostty".to_owned()]),
                None,
            )
            .unwrap();

        store
            .assign_slot(
                "demo",
                1,
                &fixed(5),
                "demo",
                None,
                None,
                &live_workspace_ids,
                Some(&["kitty".to_owned(), "--single-instance".to_owned()]),
                None,
            )
            .unwrap();

        let record = store.resolve_slot_effective("demo", 1).unwrap().unwrap();
        assert_eq!(
            record.launch_argv,
            Some(vec!["kitty".to_owned(), "--single-instance".to_owned()])
        );

        cleanup(&path);
    }

    #[test]
    fn missing_child_slot_falls_back_to_parent_binding() {
        let path = test_db_path("parent-fallback");
        let store = StateStore::new(&path).unwrap();
        let live_workspace_ids = HashSet::new();
        store
            .assign_slot(
                "x",
                1,
                &fixed(5),
                "x",
                None,
                None,
                &live_workspace_ids,
                None,
                None,
            )
            .unwrap();

        let record = store.resolve_slot_effective("x.y", 1).unwrap().unwrap();
        assert_eq!(record.environment_id, "x.y");
        assert_eq!(record.binding_environment_id, "x");
        assert_eq!(record.workspace_id, 5);

        cleanup(&path);
    }

    #[test]
    fn missing_child_slot_falls_back_through_multiple_ancestors() {
        let path = test_db_path("multi-parent-fallback");
        let store = StateStore::new(&path).unwrap();
        let live_workspace_ids = HashSet::new();
        store
            .assign_slot(
                "x",
                2,
                &fixed(7),
                "x",
                None,
                None,
                &live_workspace_ids,
                None,
                None,
            )
            .unwrap();

        let record = store.resolve_slot_effective("x.y.z", 2).unwrap().unwrap();
        assert_eq!(record.binding_environment_id, "x");
        assert_eq!(record.workspace_id, 7);

        cleanup(&path);
    }

    #[test]
    fn inherit_row_resolves_parent_workspace_and_child_command() {
        let path = test_db_path("inherit-command");
        let store = StateStore::new(&path).unwrap();
        let live_workspace_ids = HashSet::new();
        store
            .assign_slot(
                "x",
                2,
                &fixed(9),
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

        let record = store.resolve_slot_effective("x.y.z", 2).unwrap().unwrap();
        assert_eq!(record.binding_environment_id, "x");
        assert_eq!(record.command_environment_id, Some("x.y.z".to_owned()));
        assert_eq!(record.workspace_id, 9);
        assert_eq!(record.launch_argv, Some(vec!["kitty".to_owned()]));

        cleanup(&path);
    }

    #[test]
    fn child_fixed_binding_can_inherit_parent_command() {
        let path = test_db_path("child-fixed-parent-command");
        let store = StateStore::new(&path).unwrap();
        let live_workspace_ids = HashSet::new();
        store
            .assign_slot(
                "x",
                3,
                &fixed(11),
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
                "x.y",
                3,
                &fixed(12),
                "x.y",
                None,
                None,
                &live_workspace_ids,
                None,
                None,
            )
            .unwrap();

        let record = store.resolve_slot_effective("x.y", 3).unwrap().unwrap();
        assert_eq!(record.binding_environment_id, "x.y");
        assert_eq!(record.command_environment_id, Some("x".to_owned()));
        assert_eq!(record.workspace_id, 12);
        assert_eq!(record.launch_argv, Some(vec!["ghostty".to_owned()]));

        cleanup(&path);
    }

    #[test]
    fn clearing_child_command_reexposes_parent_command() {
        let path = test_db_path("clear-command");
        let store = StateStore::new(&path).unwrap();
        let live_workspace_ids = HashSet::new();
        store
            .assign_slot(
                "x",
                4,
                &fixed(13),
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
                "x.y",
                4,
                &SlotAssignmentMode::Inherit,
                "x.y",
                None,
                None,
                &live_workspace_ids,
                Some(&["kitty".to_owned()]),
                None,
            )
            .unwrap();

        assert!(store.clear_slot_launch_command("x.y", 4).unwrap());
        let record = store.resolve_slot_effective("x.y", 4).unwrap().unwrap();
        assert_eq!(record.binding_environment_id, "x");
        assert_eq!(record.command_environment_id, Some("x".to_owned()));
        assert_eq!(record.launch_argv, Some(vec!["ghostty".to_owned()]));

        cleanup(&path);
    }

    #[test]
    fn clearing_child_slot_reexposes_parent_workspace_and_command() {
        let path = test_db_path("clear-slot");
        let store = StateStore::new(&path).unwrap();
        let live_workspace_ids = HashSet::new();
        store
            .assign_slot(
                "x",
                5,
                &fixed(14),
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
                "x.y",
                5,
                &SlotAssignmentMode::Inherit,
                "x.y",
                None,
                None,
                &live_workspace_ids,
                Some(&["kitty".to_owned()]),
                None,
            )
            .unwrap();

        store.clear_slot("x.y", 5).unwrap();
        let record = store.resolve_slot_effective("x.y", 5).unwrap().unwrap();
        assert_eq!(record.binding_environment_id, "x");
        assert_eq!(record.command_environment_id, Some("x".to_owned()));
        assert_eq!(record.launch_argv, Some(vec!["ghostty".to_owned()]));

        cleanup(&path);
    }

    #[test]
    fn path_env_ids_do_not_use_dotted_hierarchy() {
        let path = test_db_path("path-flat");
        let store = StateStore::new(&path).unwrap();
        let live_workspace_ids = HashSet::new();
        store
            .assign_slot(
                "/tmp/a.b",
                1,
                &fixed(15),
                "a.b",
                None,
                None,
                &live_workspace_ids,
                None,
                None,
            )
            .unwrap();

        assert!(store
            .resolve_slot_effective("/tmp/a.b.c", 1)
            .unwrap()
            .is_none());

        cleanup(&path);
    }

    #[test]
    fn inherit_requires_named_parent_environment() {
        assert!(!environment_has_parent("x"));
        assert!(environment_has_parent("x.y"));
        assert!(!environment_has_parent("/tmp/x.y"));
    }

    #[test]
    fn slot_launch_command_set_and_clear_round_trip() {
        let path = test_db_path("set-clear");
        let store = StateStore::new(&path).unwrap();
        let live_workspace_ids = HashSet::new();
        store
            .assign_slot(
                "demo",
                1,
                &fixed(5),
                "demo",
                None,
                None,
                &live_workspace_ids,
                None,
                None,
            )
            .unwrap();

        store
            .set_slot_launch_command(
                "demo",
                1,
                &["ghostty".to_owned(), "--class".to_owned()],
                None,
            )
            .unwrap();
        let record = store.resolve_slot_effective("demo", 1).unwrap().unwrap();
        assert_eq!(
            record.launch_argv,
            Some(vec!["ghostty".to_owned(), "--class".to_owned()])
        );

        assert!(store.clear_slot_launch_command("demo", 1).unwrap());
        let cleared = store.resolve_slot_effective("demo", 1).unwrap().unwrap();
        assert_eq!(cleared.launch_argv, None);

        cleanup(&path);
    }

    #[test]
    fn clearing_missing_slot_launch_command_is_a_noop() {
        let path = test_db_path("clear-missing-command");
        let store = StateStore::new(&path).unwrap();

        assert!(!store.clear_slot_launch_command("demo", 2).unwrap());

        cleanup(&path);
    }

    #[test]
    fn transaction_commit_persists_all_mutations() {
        let path = test_db_path("transaction-commit");
        let store = StateStore::new(&path).unwrap();
        let live_workspace_ids = HashSet::new();

        store
            .with_transaction(|connection| {
                let record = store.ensure_environment_with_connection(
                    connection,
                    "demo",
                    "demo",
                    Some("/repo"),
                    Some("t3code"),
                    Some("Thread A"),
                )?;
                assert_eq!(record.env_id, "demo");
                store.assign_slot_with_connection(
                    connection,
                    "demo",
                    1,
                    &fixed(5),
                    "demo",
                    Some("/repo"),
                    Some("t3code"),
                    &live_workspace_ids,
                    Some(&["ghostty".to_owned()]),
                    Some("API"),
                )?;
                store.set_locked_environment_with_connection(connection, "demo")?;
                Ok(())
            })
            .unwrap();

        let environments = store.list_environments().unwrap();
        assert_eq!(environments.len(), 1);
        assert_eq!(environments[0].title.as_deref(), Some("Thread A"));
        assert_eq!(store.locked_environment().unwrap().as_deref(), Some("demo"));

        let record = store.resolve_slot_effective("demo", 1).unwrap().unwrap();
        assert_eq!(record.workspace_id, 5);
        assert_eq!(record.display_name.as_deref(), Some("API"));
        assert_eq!(record.launch_argv, Some(vec!["ghostty".to_owned()]));

        cleanup(&path);
    }

    #[test]
    fn transaction_rollback_discards_earlier_mutations() {
        let path = test_db_path("transaction-rollback");
        let store = StateStore::new(&path).unwrap();
        let live_workspace_ids = HashSet::new();

        let error = store
            .with_transaction(|connection| {
                store.ensure_environment_with_connection(
                    connection, "demo", "demo", None, None, None,
                )?;
                store.assign_slot_with_connection(
                    connection,
                    "demo",
                    1,
                    &fixed(5),
                    "demo",
                    None,
                    None,
                    &live_workspace_ids,
                    None,
                    Some("API"),
                )?;
                store.set_slot_launch_command_with_connection(
                    connection,
                    "missing",
                    7,
                    &["ghostty".to_owned()],
                    None,
                )?;
                Ok(())
            })
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("slot 7 is not assigned for environment missing"));
        assert!(store.list_environments().unwrap().is_empty());
        assert!(store.resolve_slot_effective("demo", 1).unwrap().is_none());

        cleanup(&path);
    }

    #[test]
    fn transaction_commit_persists_title_and_slot_name_together() {
        let path = test_db_path("transaction-title-name");
        let store = StateStore::new(&path).unwrap();
        let live_workspace_ids = HashSet::new();

        store
            .with_transaction(|connection| {
                store.ensure_environment_with_connection(
                    connection,
                    "demo",
                    "demo",
                    None,
                    None,
                    Some("Project"),
                )?;
                store.assign_slot_with_connection(
                    connection,
                    "demo",
                    2,
                    &fixed(9),
                    "demo",
                    None,
                    None,
                    &live_workspace_ids,
                    None,
                    Some("Editor"),
                )?;
                Ok(())
            })
            .unwrap();

        let environments = store.list_environments().unwrap();
        assert_eq!(environments[0].title.as_deref(), Some("Project"));
        let record = store.resolve_slot_effective("demo", 2).unwrap().unwrap();
        assert_eq!(record.display_name.as_deref(), Some("Editor"));

        cleanup(&path);
    }
}
