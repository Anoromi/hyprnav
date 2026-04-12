use crate::runtime_paths::{ensure_parent_dir, legacy_state_root};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const LOCKED_ENV_KEY: &str = "locked_env_id";
const MANAGED_WORKSPACE_START: i32 = 101;

#[derive(Clone, Debug)]
pub struct StateStore {
    path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct EnvironmentRecord {
    pub env_id: String,
    pub display_id: String,
    pub source_path: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SlotBindingRecord {
    pub env_id: String,
    pub display_id: String,
    pub slot_index: i32,
    pub binding_kind: String,
    pub workspace_id: i32,
}

#[derive(Clone, Debug)]
pub struct SlotResolutionRecord {
    pub env_id: String,
    pub slot_index: i32,
    pub binding_kind: String,
    pub workspace_id: i32,
}

impl StateStore {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        ensure_parent_dir(&path)?;
        migrate_legacy_state_db(&path)?;
        let store = Self { path };
        store.init()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn ensure_client(&self, client_id: &str) -> Result<()> {
        let now = now_unix();
        let connection = self.open()?;
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

    pub fn ensure_environment(
        &self,
        env_id: &str,
        display_id: &str,
        source_path: Option<&str>,
        client_id: Option<&str>,
    ) -> Result<EnvironmentRecord> {
        if let Some(client_id) = client_id {
            self.ensure_client(client_id)?;
        }

        let now = now_unix();
        let connection = self.open()?;
        connection.execute(
            "INSERT INTO environments (env_id, display_id, source_path, created_by_client_id, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(env_id) DO UPDATE SET
               display_id = excluded.display_id,
               source_path = excluded.source_path,
               updated_at = excluded.updated_at",
            params![env_id, display_id, source_path, client_id, now],
        )?;

        Ok(EnvironmentRecord {
            env_id: env_id.to_owned(),
            display_id: display_id.to_owned(),
            source_path: source_path.map(ToOwned::to_owned),
        })
    }

    pub fn delete_environment(&self, env_id: &str) -> Result<()> {
        let connection = self.open()?;
        connection.execute("DELETE FROM slot_bindings WHERE env_id = ?1", params![env_id])?;
        connection.execute("DELETE FROM environments WHERE env_id = ?1", params![env_id])?;
        connection.execute(
            "DELETE FROM global_state WHERE key = ?1 AND value = ?2",
            params![LOCKED_ENV_KEY, env_id],
        )?;
        Ok(())
    }

    pub fn set_locked_environment(&self, env_id: &str) -> Result<()> {
        let connection = self.open()?;
        connection.execute(
            "INSERT INTO global_state (key, value)
             VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![LOCKED_ENV_KEY, env_id],
        )?;
        Ok(())
    }

    pub fn clear_locked_environment(&self) -> Result<()> {
        let connection = self.open()?;
        connection.execute("DELETE FROM global_state WHERE key = ?1", params![LOCKED_ENV_KEY])?;
        Ok(())
    }

    pub fn locked_environment(&self) -> Result<Option<String>> {
        let connection = self.open()?;
        connection
            .query_row(
                "SELECT value FROM global_state WHERE key = ?1",
                params![LOCKED_ENV_KEY],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn assign_slot(
        &self,
        env_id: &str,
        slot_index: i32,
        workspace_id: Option<i32>,
        managed: bool,
        display_id: &str,
        source_path: Option<&str>,
        client_id: Option<&str>,
        live_workspace_ids: &HashSet<i32>,
    ) -> Result<SlotResolutionRecord> {
        self.ensure_environment(env_id, display_id, source_path, client_id)?;

        let now = now_unix();
        let connection = self.open()?;
        let workspace_id = if managed {
            if let Some(existing) = connection
                .query_row(
                    "SELECT workspace_id FROM slot_bindings
                     WHERE env_id = ?1 AND slot_index = ?2 AND binding_kind = 'managed'",
                    params![env_id, slot_index],
                    |row| row.get(0),
                )
                .optional()?
            {
                existing
            } else {
                let reserved = self.managed_workspace_ids(&connection)?;
                allocate_managed_workspace_id(&reserved, live_workspace_ids)
            }
        } else {
            workspace_id.context("fixed slot assignment requires --workspace")?
        };

        let binding_kind = if managed { "managed" } else { "fixed" };
        connection.execute(
            "INSERT INTO slot_bindings (env_id, slot_index, binding_kind, workspace_id, updated_by_client_id, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
             ON CONFLICT(env_id, slot_index) DO UPDATE SET
               binding_kind = excluded.binding_kind,
               workspace_id = excluded.workspace_id,
               updated_by_client_id = excluded.updated_by_client_id,
               updated_at = excluded.updated_at",
            params![env_id, slot_index, binding_kind, workspace_id, client_id, now],
        )?;

        Ok(SlotResolutionRecord {
            env_id: env_id.to_owned(),
            slot_index,
            binding_kind: binding_kind.to_owned(),
            workspace_id,
        })
    }

    pub fn clear_slot(&self, env_id: &str, slot_index: i32) -> Result<()> {
        let connection = self.open()?;
        connection.execute(
            "DELETE FROM slot_bindings WHERE env_id = ?1 AND slot_index = ?2",
            params![env_id, slot_index],
        )?;
        Ok(())
    }

    pub fn resolve_slot(&self, env_id: &str, slot_index: i32) -> Result<Option<SlotResolutionRecord>> {
        let connection = self.open()?;
        connection
            .query_row(
                "SELECT env_id, slot_index, binding_kind, workspace_id
                 FROM slot_bindings
                 WHERE env_id = ?1 AND slot_index = ?2",
                params![env_id, slot_index],
                |row| {
                    Ok(SlotResolutionRecord {
                        env_id: row.get(0)?,
                        slot_index: row.get(1)?,
                        binding_kind: row.get(2)?,
                        workspace_id: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn list_bindings(&self) -> Result<Vec<SlotBindingRecord>> {
        let connection = self.open()?;
        let mut statement = connection.prepare(
            "SELECT slot_bindings.env_id, environments.display_id, slot_bindings.slot_index, slot_bindings.binding_kind, slot_bindings.workspace_id
             FROM slot_bindings
             JOIN environments ON environments.env_id = slot_bindings.env_id
             ORDER BY environments.display_id, slot_bindings.env_id, slot_bindings.slot_index",
        )?;

        let rows = statement.query_map([], |row| {
            Ok(SlotBindingRecord {
                env_id: row.get(0)?,
                display_id: row.get(1)?,
                slot_index: row.get(2)?,
                binding_kind: row.get(3)?,
                workspace_id: row.get(4)?,
            })
        })?;

        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
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
              source_path TEXT NULL,
              created_by_client_id TEXT NULL,
              created_at INTEGER NOT NULL,
              updated_at INTEGER NOT NULL
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
              binding_kind TEXT NOT NULL CHECK(binding_kind IN ('fixed','managed')),
              workspace_id INTEGER NOT NULL,
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
        Ok(())
    }

    fn managed_workspace_ids(&self, connection: &Connection) -> Result<HashSet<i32>> {
        let mut statement =
            connection.prepare("SELECT workspace_id FROM slot_bindings WHERE binding_kind = 'managed'")?;
        let rows = statement.query_map([], |row| row.get::<_, i32>(0))?;
        let ids = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids.into_iter().collect())
    }

    fn open(&self) -> Result<Connection> {
        Connection::open(&self.path).with_context(|| format!("opening {}", self.path.display()))
    }
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

fn allocate_managed_workspace_id(reserved: &HashSet<i32>, live_workspace_ids: &HashSet<i32>) -> i32 {
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
