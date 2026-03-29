use anyhow::{Context, Result};
use std::ffi::OsStr;
use std::fs;
use std::hash::Hasher;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub const DEFAULT_RUNTIME_ROOT: &str = "/run/user";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimePaths {
    pub runtime_root: PathBuf,
    pub instance_signature: String,
    pub runtime_dir: PathBuf,
    pub preview_socket_path: PathBuf,
    pub switcher_socket_path: PathBuf,
    pub hypr_event_socket_path: PathBuf,
}

pub fn runtime_root() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("{DEFAULT_RUNTIME_ROOT}/{}", rustix_like_getuid())))
}

pub fn rustix_like_getuid() -> u32 {
    unsafe { libc::geteuid() }
}

pub fn runtime_directory(runtime_dir: &Path, instance_signature: &str) -> PathBuf {
    let hashed = fnv1a_64(if instance_signature.is_empty() { "default" } else { instance_signature });
    runtime_dir.join("hx").join(format!("{hashed:016x}"))
}

pub fn preview_socket_path(runtime_dir: &Path, instance_signature: &str) -> PathBuf {
    runtime_directory(runtime_dir, instance_signature).join("preview.sock")
}

pub fn switcher_socket_path(runtime_dir: &Path, instance_signature: &str) -> PathBuf {
    runtime_directory(runtime_dir, instance_signature).join("switcher.sock")
}

pub fn hyprland_event_socket_path(runtime_dir: &Path, instance_signature: &str) -> PathBuf {
    runtime_dir.join("hypr").join(instance_signature).join(".socket2.sock")
}

pub fn hyprland_socket_path(runtime_dir: &Path, instance_signature: &str) -> PathBuf {
    runtime_dir.join("hypr").join(instance_signature).join(".socket.sock")
}

pub fn preview_path(runtime_dir: &Path, instance_signature: &str, workspace_id: i32) -> PathBuf {
    runtime_directory(runtime_dir, instance_signature).join(format!("workspace-{workspace_id}.jpg"))
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
            Some((_, best_secs, best_nsecs)) if (*best_secs, *best_nsecs) >= (mtime, mtime_nsec) => {}
            _ => newest = Some((signature.to_owned(), mtime, mtime_nsec)),
        }
    }

    newest.map(|value| value.0).unwrap_or_default()
}

pub fn resolve_runtime_paths() -> RuntimePaths {
    let runtime_root = runtime_root();
    let hinted = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok();
    let instance_signature = discover_hyprland_instance_signature(&runtime_root, hinted.as_deref());

    RuntimePaths {
        preview_socket_path: preview_socket_path(&runtime_root, &instance_signature),
        switcher_socket_path: switcher_socket_path(&runtime_root, &instance_signature),
        hypr_event_socket_path: hyprland_event_socket_path(&runtime_root, &instance_signature),
        runtime_dir: runtime_directory(&runtime_root, &instance_signature),
        runtime_root,
        instance_signature,
    }
}

pub fn fallback_switcher_socket_paths(runtime_dir: &Path) -> Vec<PathBuf> {
    let root = runtime_dir.join("hx");
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };

    let mut entries = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path().join("switcher.sock");
            fs::metadata(&path).ok().map(|metadata| (path, metadata.mtime(), metadata.mtime_nsec()))
        })
        .collect::<Vec<_>>();

    entries.sort_by(|left, right| (right.1, right.2).cmp(&(left.1, left.2)));
    entries.into_iter().map(|entry| entry.0).collect()
}

pub fn ensure_parent_dir(path: &Path) -> Result<()> {
    let parent = path.parent().context("path had no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))
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
