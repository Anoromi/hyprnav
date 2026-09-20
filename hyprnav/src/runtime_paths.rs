use anyhow::{Context, Result};
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::hash::Hasher;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const DEFAULT_RUNTIME_ROOT: &str = "/run/user";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimePaths {
    pub runtime_root: PathBuf,
    pub instance_signature: String,
    pub runtime_dir: PathBuf,
    pub spawn_socket_path: PathBuf,
    pub switcher_socket_path: PathBuf,
    pub grid_socket_path: PathBuf,
    pub server_socket_path: PathBuf,
    pub events_socket_path: PathBuf,
    pub hypr_event_socket_path: PathBuf,
    pub switch_log_path: PathBuf,
    pub state_root: PathBuf,
    pub state_db_path: PathBuf,
}

pub fn runtime_root() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(format!("{DEFAULT_RUNTIME_ROOT}/{}", rustix_like_getuid()))
        })
}

pub fn rustix_like_getuid() -> u32 {
    unsafe { libc::geteuid() }
}

pub fn runtime_directory(runtime_dir: &Path, instance_signature: &str) -> PathBuf {
    let hashed = fnv1a_64(if instance_signature.is_empty() {
        "default"
    } else {
        instance_signature
    });
    runtime_dir.join("hx").join(format!("{hashed:016x}"))
}

pub fn switcher_socket_path(runtime_dir: &Path, instance_signature: &str) -> PathBuf {
    runtime_directory(runtime_dir, instance_signature).join("ui-hyprnav.sock")
}

pub fn grid_socket_path(runtime_dir: &Path, instance_signature: &str) -> PathBuf {
    runtime_directory(runtime_dir, instance_signature).join("ui-hyprnav-grid.sock")
}

pub fn spawn_socket_path(runtime_dir: &Path, instance_signature: &str) -> PathBuf {
    runtime_directory(runtime_dir, instance_signature).join("spawn.sock")
}

/// File holding the window address the next screencast picker should answer with.
pub fn screencast_request_path(runtime_dir: &Path, instance_signature: &str) -> PathBuf {
    runtime_directory(runtime_dir, instance_signature).join("screencast-request")
}

pub fn server_socket_path(runtime_dir: &Path, instance_signature: &str) -> PathBuf {
    runtime_directory(runtime_dir, instance_signature).join("hyprnav.sock")
}

/// Push-event socket: subscribers read `agents` and `slots` events from it.
/// Sits beside the request socket so clients derive both from one directory.
pub fn events_socket_path(runtime_dir: &Path, instance_signature: &str) -> PathBuf {
    runtime_directory(runtime_dir, instance_signature).join("events.sock")
}

pub fn switch_log_path(runtime_dir: &Path, instance_signature: &str) -> PathBuf {
    runtime_directory(runtime_dir, instance_signature).join("switch.log")
}

pub fn hyprland_event_socket_path(runtime_dir: &Path, instance_signature: &str) -> PathBuf {
    runtime_dir
        .join("hypr")
        .join(instance_signature)
        .join(".socket2.sock")
}

pub fn hyprland_socket_path(runtime_dir: &Path, instance_signature: &str) -> PathBuf {
    runtime_dir
        .join("hypr")
        .join(instance_signature)
        .join(".socket.sock")
}

pub fn discover_hyprland_instance_signature(runtime_dir: &Path, hinted: Option<&str>) -> String {
    if let Some(signature) = hinted.filter(|value| !value.is_empty()) {
        let socket_path = hyprland_socket_path(runtime_dir, signature);
        if socket_path.is_file() {
            return signature.to_owned();
        }
    }

    let root = runtime_dir.join("hypr");
    let Ok(entries) = fs::read_dir(&root) else {
        return String::new();
    };

    let mut newest: Option<(String, i64, i64)> = None;

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let Some(signature) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };

        let socket_path = path.join(".socket.sock");
        let Ok(metadata) = fs::metadata(&socket_path) else {
            continue;
        };

        let mtime = metadata.mtime();
        let mtime_nsec = metadata.mtime_nsec();
        match &newest {
            Some((_, best_secs, best_nsecs))
                if (*best_secs, *best_nsecs) >= (mtime, mtime_nsec) => {}
            _ => newest = Some((signature.to_owned(), mtime, mtime_nsec)),
        }
    }

    newest.map(|value| value.0).unwrap_or_default()
}

pub fn resolve_runtime_paths() -> RuntimePaths {
    let runtime_root = runtime_root();
    let hinted = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok();
    let instance_signature = discover_hyprland_instance_signature(&runtime_root, hinted.as_deref());
    let state_root = state_root();

    RuntimePaths {
        spawn_socket_path: spawn_socket_path(&runtime_root, &instance_signature),
        switcher_socket_path: switcher_socket_path(&runtime_root, &instance_signature),
        grid_socket_path: grid_socket_path(&runtime_root, &instance_signature),
        server_socket_path: server_socket_path(&runtime_root, &instance_signature),
        events_socket_path: events_socket_path(&runtime_root, &instance_signature),
        hypr_event_socket_path: hyprland_event_socket_path(&runtime_root, &instance_signature),
        switch_log_path: switch_log_path(&runtime_root, &instance_signature),
        runtime_dir: runtime_directory(&runtime_root, &instance_signature),
        runtime_root,
        instance_signature,
        state_db_path: state_db_path(&state_root),
        state_root,
    }
}

pub fn ensure_parent_dir(path: &Path) -> Result<()> {
    let parent = path.parent().context("path had no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))
}

pub fn append_switch_log(event: &str, fields: impl fmt::Display) {
    let _ = append_switch_log_result(event, fields);
}

fn append_switch_log_result(event: &str, fields: impl fmt::Display) -> std::io::Result<()> {
    let paths = resolve_runtime_paths();
    let path = paths.switch_log_path;
    let _ = ensure_parent_dir(&path);

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(
        file,
        "{} {} {}",
        now_ms(),
        sanitize_log_fragment(event),
        sanitize_log_fragment(&fields.to_string())
    )
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

pub fn sanitize_log_fragment(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '\n' | '\r' | '\t' => ' ',
            _ if character.is_control() => ' ',
            _ => character,
        })
        .collect()
}

pub fn state_root() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".local/state"))
                .unwrap_or_else(|| PathBuf::from("/tmp"))
        })
        .join("hyprnav")
}

pub fn legacy_state_root() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".local/state"))
                .unwrap_or_else(|| PathBuf::from("/tmp"))
        })
        .join("hyprnav")
}

pub fn state_db_path(state_root: &Path) -> PathBuf {
    state_root.join("state.sqlite3")
}

fn fnv1a_64(value: &str) -> u64 {
    const OFFSET_BASIS: u64 = 14695981039346656037;
    const PRIME: u64 = 1099511628211;

    let mut hasher = Fnv1a64(OFFSET_BASIS);
    hasher.write(value.as_bytes());
    hasher.finish()
}

struct Fnv1a64(u64);

impl Hasher for Fnv1a64 {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(1099511628211);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_socket_sits_beside_the_request_socket() {
        let root = Path::new("/run/user/1000");
        let events = events_socket_path(root, "sig");
        let server = server_socket_path(root, "sig");
        assert_eq!(events.parent(), server.parent());
        assert_eq!(events.file_name().unwrap(), "events.sock");
    }

    #[test]
    fn log_fragment_sanitization_removes_line_breaks_and_controls() {
        assert_eq!(sanitize_log_fragment("a\nb\rc\td\u{7}e"), "a b c d e");
    }

    #[test]
    fn append_switch_log_is_best_effort() {
        append_switch_log("test.event", "field=value");
    }
}
