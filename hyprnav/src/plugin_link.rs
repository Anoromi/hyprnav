//! The daemon's end of the plugin's damage channel.
//!
//! hyprnav-plugin listens on `spawn.sock`, which the daemon otherwise uses one
//! request at a time. For frames it holds a *persistent* connection instead:
//! it sends `frames_watch` to arm a window, and the plugin pushes
//!
//! ```text
//! {"ev":"window_damaged","addr":"0x…"}
//! {"ev":"transient_mapped","addr":"0x…","parent":"0x…"}
//! {"ev":"transient_unmapped","addr":"0x…"}
//! ```
//!
//! down the same connection until it closes. Whether that connection can be
//! made at all is also how the daemon knows the plugin is loaded: without it
//! the capture helper falls back to its paced loop.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tracing::{debug, warn};

/// What the daemon does with the events the plugin pushes.
pub trait DamageSink: Send + Sync {
    fn on_window_damaged(&self, address: &str);
    fn on_transient_mapped(&self, address: &str, parent: &str);
    fn on_transient_unmapped(&self, address: &str);
}

pub struct PluginLink {
    path: PathBuf,
    writer: Mutex<Option<UnixStream>>,
    watches: Mutex<HashMap<String, u32>>,
    available: AtomicBool,
}

impl PluginLink {
    /// Try the socket once, then keep trying in the background. The first
    /// attempt is synchronous so that the first client to arrive already knows
    /// whether damage-driven capture is on the table.
    pub fn start(path: &Path, sink: Arc<dyn DamageSink>) -> Arc<Self> {
        let link = Arc::new(Self {
            path: path.to_owned(),
            writer: Mutex::new(None),
            watches: Mutex::new(HashMap::new()),
            available: AtomicBool::new(false),
        });
        link.available.store(probe(path), Ordering::Relaxed);
        let background = link.clone();
        thread::Builder::new()
            .name("hyprnav-frames-plugin".to_owned())
            .spawn(move || background.run(sink))
            .ok();
        link
    }

    /// A link that never connects, for tests and for `force_fallback`.
    pub fn disabled() -> Arc<Self> {
        Arc::new(Self {
            path: PathBuf::new(),
            writer: Mutex::new(None),
            watches: Mutex::new(HashMap::new()),
            available: AtomicBool::new(false),
        })
    }

    pub fn available(&self) -> bool {
        self.available.load(Ordering::Relaxed)
    }

    /// Ask the compositor to report this window's damage, at most that often.
    pub fn watch(&self, address: &str, interval_ms: u32) {
        if let Ok(mut watches) = self.watches.lock() {
            watches.insert(address.to_owned(), interval_ms);
        }
        self.send(address, true, interval_ms);
    }

    pub fn unwatch(&self, address: &str) {
        if let Ok(mut watches) = self.watches.lock() {
            watches.remove(address);
        }
        self.send(address, false, 0);
    }

    fn send(&self, address: &str, on: bool, interval_ms: u32) {
        let Ok(mut writer) = self.writer.lock() else { return };
        let Some(stream) = writer.as_mut() else { return };
        let line = format!(
            "{{\"op\":\"frames_watch\",\"addr\":\"{address}\",\"on\":{on},\"interval_ms\":{interval_ms}}}\n"
        );
        if stream.write_all(line.as_bytes()).is_err() || stream.flush().is_err() {
            *writer = None;
        }
    }

    fn run(self: Arc<Self>, sink: Arc<dyn DamageSink>) {
        let mut backoff = Duration::from_millis(250);
        loop {
            match UnixStream::connect(&self.path) {
                Ok(stream) => {
                    backoff = Duration::from_millis(250);
                    self.available.store(true, Ordering::Relaxed);
                    if let Ok(reader) = stream.try_clone() {
                        if let Ok(mut writer) = self.writer.lock() {
                            *writer = Some(stream);
                        }
                        // A reconnected plugin knows nothing about the windows
                        // that were already being watched.
                        let watches = self
                            .watches
                            .lock()
                            .map(|watches| watches.clone())
                            .unwrap_or_default();
                        for (address, interval) in watches {
                            self.send(&address, true, interval);
                        }
                        self.read(reader, sink.as_ref());
                    }
                    if let Ok(mut writer) = self.writer.lock() {
                        *writer = None;
                    }
                    self.available.store(false, Ordering::Relaxed);
                    debug!("plugin damage channel closed");
                }
                Err(error) => {
                    self.available.store(false, Ordering::Relaxed);
                    debug!(path = %self.path.display(), "plugin damage channel unavailable: {error}");
                }
            }
            thread::sleep(backoff);
            backoff = (backoff * 2).min(Duration::from_secs(5));
        }
    }

    fn read(&self, stream: UnixStream, sink: &dyn DamageSink) {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else { continue };
            let text = |key: &str| value.get(key).and_then(|v| v.as_str()).unwrap_or_default();
            match value.get("ev").and_then(|v| v.as_str()).unwrap_or_default() {
                "window_damaged" => sink.on_window_damaged(text("addr")),
                "transient_mapped" => sink.on_transient_mapped(text("addr"), text("parent")),
                "transient_unmapped" => sink.on_transient_unmapped(text("addr")),
                "" => {} // a reply to one of our own frames_watch lines
                other => warn!(event = other, "unknown plugin event"),
            }
        }
    }
}

/// One `ping`, to see whether the plugin is loaded at all.
fn probe(path: &Path) -> bool {
    let Ok(mut stream) = UnixStream::connect(path) else { return false };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    if stream.write_all(b"{\"op\":\"ping\"}\n").is_err() {
        return false;
    }
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).is_ok() && line.contains("\"ok\":true")
}
