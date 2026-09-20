//! Live window frame streaming.
//!
//! The daemon owns a third Unix socket beside the request and event sockets
//! (`frames.sock`). A client sends exactly one JSON line:
//!
//! ```text
//! {"address":"0x55ea1ad9c6d0","fps":8,"quality":60,"max_width":640}
//! ```
//!
//! and gets back a `multipart/x-mixed-replace` byte stream with boundary
//! `frame`, one part per JPEG:
//!
//! ```text
//! --frame\r\nContent-Type: image/jpeg\r\nContent-Length: <n>\r\n\r\n<bytes>\r\n
//! ```
//!
//! The stream ends (connection closed) when the window goes away or the
//! client disconnects. An unknown address gets one JSON line
//! `{"error":"unknown_window"}` and nothing else.
//!
//! ## Where the pixels come from
//!
//! One long-lived `hyprnav-capture` child does the Wayland work: it follows
//! ext-foreign-toplevel-list-v1 (to pair window addresses with capture
//! sources) and runs an ext-image-copy-capture-v1 session per watched window,
//! scaling and JPEG-encoding in C. The daemon only brokers: it refcounts how
//! many clients want each address, sends `start`/`stop` commands on the
//! child's stdin, and fans the frames it reads from the child's stdout out to
//! the sockets.
//!
//! That split is deliberate. A per-frame `grim` would mean a process spawn and
//! a full-resolution encode for every frame of every watched window; the
//! persistent session reuses one shm buffer, encodes at the requested width,
//! and — because the compositor only completes a capture when the window is
//! repainted, and the helper skips re-encoding identical pixels — costs
//! essentially nothing while a window sits still.
//!
//! All clients watching the same address share that one capture session, at
//! the loosest parameters any of them asked for. Each client holds a single
//! latest-frame slot, so a slow reader drops frames instead of queueing them
//! or stalling anyone else.

use crate::encoder::{self, EncoderChoice};
use crate::frames_config::FramesConfig;
use crate::pipeline::{Pipeline, PipelineKey, VideoClient, VideoRequest};
use crate::plugin_link::{DamageSink, PluginLink};
use crate::video::Codec;
use serde_json::json;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, warn};

pub const MIN_FPS: u32 = 1;
pub const MAX_FPS: u32 = 15;
pub const DEFAULT_FPS: u32 = 8;
pub const MIN_QUALITY: u32 = 30;
pub const MAX_QUALITY: u32 = 90;
pub const DEFAULT_QUALITY: u32 = 60;
pub const MIN_WIDTH: u32 = 64;
pub const MAX_WIDTH: u32 = 3840;
pub const DEFAULT_WIDTH: u32 = 640;

/// Multipart boundary; fixed so clients can hard-code `-f mpjpeg`.
pub const BOUNDARY: &str = "frame";

/// A frame bigger than this is treated as a desynchronised helper stream.
const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;

/// The MJPEG capture parameters a client asked for, after clamping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StreamRequest {
    pub fps: u32,
    pub quality: u32,
    pub max_width: u32,
}

impl Default for StreamRequest {
    fn default() -> Self {
        Self {
            fps: DEFAULT_FPS,
            quality: DEFAULT_QUALITY,
            max_width: DEFAULT_WIDTH,
        }
    }
}

/// The whole opening line, for both formats.
///
/// ```text
/// {"address":"0x…","codecs":["av1","h264","mjpeg"],"max_width":640,
///  "max_fps":8,"follow":"transient"}
/// ```
///
/// `"format":"av1"` is accepted as shorthand for a one-element `codecs`, and
/// `fps` as an alias of `max_fps`. An absent `codecs` means MJPEG, which is
/// what every client that predates the video path sends.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientRequest {
    pub address: String,
    pub codecs: Vec<Codec>,
    pub stream: StreamRequest,
    pub follow_transient: bool,
}

impl ClientRequest {
    pub fn video(&self) -> VideoRequest {
        VideoRequest { max_fps: self.stream.fps, follow_transient: self.follow_transient }
    }
}

/// Parse the client's opening line. `None` means "no usable address".
pub fn parse_request(line: &str) -> Option<ClientRequest> {
    let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let address = normalize_address(value.get("address")?.as_str()?)?;
    let number = |key: &str| value.get(key).and_then(|v| v.as_u64()).map(|v| v as u32);
    let mut codecs: Vec<Codec> = value
        .get("codecs")
        .and_then(|v| v.as_array())
        .map(|list| list.iter().filter_map(|v| v.as_str()).filter_map(Codec::parse).collect())
        .unwrap_or_default();
    if codecs.is_empty() {
        codecs = value
            .get("format")
            .and_then(|v| v.as_str())
            .and_then(Codec::parse)
            .map(|codec| vec![codec])
            .unwrap_or_else(|| vec![Codec::Mjpeg]);
    }
    Some(ClientRequest {
        address,
        codecs,
        stream: StreamRequest {
            fps: number("max_fps")
                .or_else(|| number("fps"))
                .unwrap_or(DEFAULT_FPS)
                .clamp(MIN_FPS, MAX_FPS),
            quality: number("quality")
                .unwrap_or(DEFAULT_QUALITY)
                .clamp(MIN_QUALITY, MAX_QUALITY),
            max_width: number("max_width")
                .unwrap_or(DEFAULT_WIDTH)
                .clamp(MIN_WIDTH, MAX_WIDTH),
        },
        follow_transient: value.get("follow").and_then(|v| v.as_str()) == Some("transient"),
    })
}

/// `address:0xAB` and `0xAB` both become `0xab`; anything else is rejected.
pub fn normalize_address(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_start_matches("address:").trim();
    let digits = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))?;
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("0x{:x}", u64::from_str_radix(digits, 16).ok()?))
}

/// One multipart part for `payload`.
pub fn multipart_part(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 96);
    out.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
            payload.len()
        )
        .as_bytes(),
    );
    out.extend_from_slice(payload);
    out.extend_from_slice(b"\r\n");
    out
}

// ---------------------------------------------------------------------------
// the capture helper child
// ---------------------------------------------------------------------------

/// Capture parameters for one address, as sent to the helper.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureParams {
    pub max_fps: u32,
    pub quality: u32,
    pub max_width: u32,
}

/// What the daemon does with `hyprnav-capture`'s stdout.
pub trait CaptureSink: Send + Sync {
    fn on_add(&self, address: &str, identifier: &str);
    fn on_close(&self, address: &str);
    fn on_frame(&self, address: &str, jpeg: Vec<u8>);
    fn on_capture_failed(&self, address: &str, reason: &str);
}

/// Where `start`/`stop` commands go. A trait so the broker can be tested
/// without a compositor.
pub trait CommandSink: Send + Sync {
    fn start(&self, address: &str, params: CaptureParams);
    fn stop(&self, address: &str);
    /// True once the helper has listed every existing window, so a miss in
    /// the address index really means "no such window".
    fn primed(&self) -> bool;
    fn identifier(&self, address: &str) -> Option<String>;
}

#[derive(Default)]
struct HelperState {
    stdin: Option<ChildStdin>,
    /// Addresses currently being captured, replayed after a restart.
    active: HashMap<String, CaptureParams>,
    by_address: HashMap<String, String>,
    primed: bool,
}

/// Owns the `hyprnav-capture` child: spawns it, restarts it with backoff,
/// writes commands to it and parses its NDJSON.
pub struct CaptureHelper {
    program: String,
    state: Mutex<HelperState>,
}

impl CaptureHelper {
    pub fn new(program: String) -> Arc<Self> {
        Arc::new(Self {
            program,
            state: Mutex::new(HelperState::default()),
        })
    }

    /// Locate the helper next to this executable, else fall back to `$PATH`.
    pub fn default_program() -> String {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("hyprnav-capture")))
            .filter(|candidate| candidate.is_file())
            .map(|candidate| candidate.to_string_lossy().into_owned())
            .unwrap_or_else(|| "hyprnav-capture".to_owned())
    }

    /// Keep the child alive forever, feeding `sink`.
    pub fn spawn(self: &Arc<Self>, sink: Arc<dyn CaptureSink>) {
        let helper = self.clone();
        thread::Builder::new()
            .name("hyprnav-capture-helper".to_owned())
            .spawn(move || {
                let mut backoff = Duration::from_millis(250);
                loop {
                    let started = Instant::now();
                    match helper.run_once(sink.as_ref()) {
                        Ok(()) => debug!("capture helper exited"),
                        Err(error) => warn!("capture helper failed: {error}"),
                    }
                    let dropped = helper.forget_child();
                    for address in dropped {
                        sink.on_capture_failed(&address, "helper_restarted");
                    }
                    if started.elapsed() > Duration::from_secs(30) {
                        backoff = Duration::from_millis(250);
                    }
                    thread::sleep(backoff);
                    backoff = (backoff * 2).min(Duration::from_secs(10));
                }
            })
            .ok();
    }

    /// Forget the dead child and report which addresses it was capturing.
    fn forget_child(&self) -> Vec<String> {
        let Ok(mut state) = self.state.lock() else {
            return Vec::new();
        };
        state.stdin = None;
        state.primed = false;
        state.by_address.clear();
        state.active.keys().cloned().collect()
    }

    fn run_once(&self, sink: &dyn CaptureSink) -> std::io::Result<()> {
        let mut child = Command::new(&self.program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdout = child.stdout.take().expect("piped stdout");
        if let Ok(mut state) = self.state.lock() {
            state.stdin = child.stdin.take();
        }
        let result = self.read_stream(BufReader::new(stdout), sink);
        let _ = child.kill();
        let _ = child.wait();
        result
    }

    /// Parse the helper's NDJSON; `frame` lines are followed by raw bytes.
    fn read_stream<R: BufRead>(&self, mut reader: R, sink: &dyn CaptureSink) -> std::io::Result<()> {
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                return Ok(());
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                continue;
            };
            let string = |key: &str| {
                value
                    .get(key)
                    .and_then(|v| v.as_str())
                    .map(|v| v.to_owned())
            };
            match value.get("ev").and_then(|v| v.as_str()).unwrap_or_default() {
                "ready" => {
                    if let Ok(mut state) = self.state.lock() {
                        state.primed = true;
                    }
                    // Replay what was being captured before a restart.
                    let active = self
                        .state
                        .lock()
                        .map(|state| state.active.clone())
                        .unwrap_or_default();
                    for (address, params) in active {
                        CommandSink::start(self, &address, params);
                    }
                }
                "add" => {
                    let (Some(address), Some(identifier)) = (string("addr"), string("id")) else {
                        continue;
                    };
                    if let Ok(mut state) = self.state.lock() {
                        state.by_address.insert(address.clone(), identifier.clone());
                    }
                    sink.on_add(&address, &identifier);
                }
                "close" => {
                    let Some(address) = string("addr") else { continue };
                    if let Ok(mut state) = self.state.lock() {
                        state.by_address.remove(&address);
                        state.active.remove(&address);
                    }
                    sink.on_close(&address);
                }
                "capture_failed" => {
                    let Some(address) = string("addr") else { continue };
                    let reason = string("reason").unwrap_or_else(|| "unknown".to_owned());
                    if let Ok(mut state) = self.state.lock() {
                        state.active.remove(&address);
                    }
                    sink.on_capture_failed(&address, &reason);
                }
                "frame" => {
                    let Some(address) = string("addr") else { continue };
                    let length = value.get("len").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    if length == 0 || length > MAX_FRAME_BYTES {
                        return Err(std::io::Error::other("capture helper sent a bad frame length"));
                    }
                    let mut jpeg = vec![0u8; length];
                    reader.read_exact(&mut jpeg)?;
                    if let Some(encode_ms) = value.get("enc_ms").and_then(|v| v.as_f64()) {
                        debug!(
                            address = %address,
                            bytes = length,
                            width = value.get("w").and_then(|v| v.as_u64()).unwrap_or(0),
                            height = value.get("h").and_then(|v| v.as_u64()).unwrap_or(0),
                            encode_ms,
                            "captured a frame"
                        );
                    }
                    sink.on_frame(&address, jpeg);
                }
                _ => {}
            }
        }
    }

    fn send(&self, command: serde_json::Value) {
        let Ok(mut state) = self.state.lock() else { return };
        let Some(stdin) = state.stdin.as_mut() else { return };
        if stdin.write_all(format!("{command}\n").as_bytes()).is_err() || stdin.flush().is_err() {
            state.stdin = None;
        }
    }
}

impl CommandSink for CaptureHelper {
    fn start(&self, address: &str, params: CaptureParams) {
        if let Ok(mut state) = self.state.lock() {
            state.active.insert(address.to_owned(), params);
        }
        self.send(json!({
            "op": "start",
            "addr": address,
            "max_width": params.max_width,
            "quality": params.quality,
            "max_fps": params.max_fps,
        }));
    }

    fn stop(&self, address: &str) {
        if let Ok(mut state) = self.state.lock() {
            state.active.remove(address);
        }
        self.send(json!({"op": "stop", "addr": address}));
    }

    fn primed(&self) -> bool {
        self.state.lock().map(|state| state.primed).unwrap_or(false)
    }

    fn identifier(&self, address: &str) -> Option<String> {
        self.state
            .lock()
            .ok()?
            .by_address
            .get(address)
            .cloned()
    }
}

// ---------------------------------------------------------------------------
// broker
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Slot {
    latest: Option<Arc<Vec<u8>>>,
    closed: bool,
}

/// One connected client: a single latest-frame slot, so it drops rather than
/// queues when it cannot keep up.
struct Client {
    id: u64,
    request: StreamRequest,
    slot: Mutex<Slot>,
    ready: Condvar,
}

impl Client {
    fn offer(&self, frame: Arc<Vec<u8>>) {
        if let Ok(mut slot) = self.slot.lock() {
            slot.latest = Some(frame);
            self.ready.notify_all();
        }
    }

    fn close(&self) {
        if let Ok(mut slot) = self.slot.lock() {
            slot.closed = true;
            self.ready.notify_all();
        }
    }

    /// Block until there is a frame, or the stream ended.
    fn next_frame(&self) -> Option<Arc<Vec<u8>>> {
        let mut slot = self.slot.lock().ok()?;
        loop {
            if let Some(frame) = slot.latest.take() {
                return Some(frame);
            }
            if slot.closed {
                return None;
            }
            slot = self.ready.wait(slot).ok()?;
        }
    }
}

#[derive(Default)]
struct Stream {
    clients: Vec<Arc<Client>>,
    params: Option<CaptureParams>,
    /// Last frame seen for this address. Capture is damage-driven, so a still
    /// window produces nothing new; a client joining later would stare at an
    /// empty stream without this.
    latest: Option<Arc<Vec<u8>>>,
}

impl Stream {
    /// The loosest parameters any client asked for, so nobody is shortchanged.
    fn wanted(&self) -> Option<CaptureParams> {
        let max_fps = self.clients.iter().map(|c| c.request.fps).max()?;
        Some(CaptureParams {
            max_fps,
            quality: self.clients.iter().map(|c| c.request.quality).max()?,
            max_width: self.clients.iter().map(|c| c.request.max_width).max()?,
        })
    }
}

/// Refcounts captures per address and fans frames out to the socket clients.
pub struct FrameHub {
    commands: Arc<dyn CommandSink>,
    streams: Mutex<HashMap<String, Stream>>,
    next_client_id: AtomicU64,
    /// Hyprland instance to drive `render_unfocused` on; `None` in tests.
    instance: Option<String>,
    config: FramesConfig,
    /// What this machine turned out to be able to encode, in the order the
    /// config prefers. Empty until the startup probe finishes.
    encoders: Mutex<Vec<EncoderChoice>>,
    /// Set once, after construction: the link needs the hub as its sink.
    plugin: std::sync::OnceLock<Arc<PluginLink>>,
    helper_program: String,
    pipelines: Mutex<HashMap<PipelineKey, Arc<Pipeline>>>,
}

impl FrameHub {
    pub fn new(commands: Arc<dyn CommandSink>) -> Arc<Self> {
        Arc::new(Self {
            commands,
            streams: Mutex::new(HashMap::new()),
            next_client_id: AtomicU64::new(0),
            instance: None,
            config: FramesConfig::default(),
            encoders: Mutex::new(Vec::new()),
            plugin: std::sync::OnceLock::new(),
            helper_program: CaptureHelper::default_program(),
            pipelines: Mutex::new(HashMap::new()),
        })
    }

    /// The daemon's constructor: a config, a helper to spawn per pipeline, and
    /// an instance to drive `render_unfocused` on.
    pub fn with_config(
        commands: Arc<dyn CommandSink>,
        instance: String,
        config: FramesConfig,
        helper_program: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            commands,
            streams: Mutex::new(HashMap::new()),
            next_client_id: AtomicU64::new(0),
            instance: Some(instance),
            config,
            encoders: Mutex::new(Vec::new()),
            plugin: std::sync::OnceLock::new(),
            helper_program,
            pipelines: Mutex::new(HashMap::new()),
        })
    }

    pub fn attach_plugin(&self, link: Arc<PluginLink>) {
        let _ = self.plugin.set(link);
    }

    fn plugin(&self) -> Option<&Arc<PluginLink>> {
        self.plugin.get()
    }

    /// True when captures must be driven by a timer rather than by damage.
    fn paced(&self) -> bool {
        self.config.force_fallback || !self.plugin().map(|link| link.available()).unwrap_or(false)
    }

    pub fn with_instance(commands: Arc<dyn CommandSink>, instance: String) -> Arc<Self> {
        Arc::new(Self {
            commands,
            streams: Mutex::new(HashMap::new()),
            next_client_id: AtomicU64::new(0),
            instance: Some(instance),
            config: FramesConfig::default(),
            encoders: Mutex::new(Vec::new()),
            plugin: std::sync::OnceLock::new(),
            helper_program: CaptureHelper::default_program(),
            pipelines: Mutex::new(HashMap::new()),
        })
    }

    /// Ask Hyprland to keep repainting a window nobody is looking at, for as
    /// long as somebody is watching its stream.
    ///
    /// With the toplevel-export backend this is belt and braces: rendering a
    /// window standalone already hands it frame callbacks, so the lab's
    /// countdown ticks without it. It is cheap, it costs two `hyprctl` runs
    /// per stream, and it keeps clients that only repaint on a vblank honest.
    fn set_render_unfocused(&self, address: &str, on: bool) {
        let Some(instance) = self.instance.clone() else { return };
        let address = address.to_owned();
        thread::Builder::new()
            .name("hyprnav-frames-prop".to_owned())
            .spawn(move || {
                let value = if on { "1" } else { "0" };
                let dispatch = format!(
                    "hl.dsp.window.set_prop({{window=\"address:{address}\", \
                     prop=\"render_unfocused\", value=\"{value}\"}})"
                );
                let result = Command::new("hyprctl")
                    .env("HYPRLAND_INSTANCE_SIGNATURE", instance)
                    .arg("dispatch")
                    .arg(&dispatch)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                if let Ok(status) = result {
                    if !status.success() {
                        debug!(address = %address, on, "render_unfocused dispatch failed");
                    }
                }
            })
            .ok();
    }

    pub fn active_streams(&self) -> usize {
        self.streams.lock().map(|streams| streams.len()).unwrap_or(0)
    }

    /// Join `address`'s capture, starting it if this is the first client.
    /// `None` means the address does not name a live window.
    fn join(self: &Arc<Self>, address: &str, request: StreamRequest) -> Option<Arc<Client>> {
        if self.commands.identifier(address).is_none() && self.commands.primed() {
            return None;
        }
        let client = Arc::new(Client {
            id: self.next_client_id.fetch_add(1, Ordering::Relaxed),
            request,
            slot: Mutex::new(Slot::default()),
            ready: Condvar::new(),
        });
        let first;
        let (command, latest) = {
            let mut streams = self.streams.lock().ok()?;
            let stream = streams.entry(address.to_owned()).or_default();
            stream.clients.push(client.clone());
            first = stream.clients.len() == 1;
            let wanted = stream.wanted();
            // Only talk to the helper when the aggregate actually moved.
            let command = if wanted != stream.params {
                stream.params = wanted;
                wanted
            } else {
                None
            };
            (command, stream.latest.clone())
        };
        // Hand a late joiner what the window looks like right now.
        if let Some(frame) = latest {
            client.offer(frame);
        }
        if let Some(params) = command {
            if first {
                self.set_render_unfocused(address, true);
            }
            self.commands.start(address, params);
            self.warn_if_no_frame_arrives(address);
        }
        Some(client)
    }

    fn leave(&self, address: &str, client_id: u64) {
        let action = {
            let Ok(mut streams) = self.streams.lock() else { return };
            let Some(stream) = streams.get_mut(address) else { return };
            stream.clients.retain(|client| client.id != client_id);
            if stream.clients.is_empty() {
                streams.remove(address);
                Some(None)
            } else {
                let wanted = stream.wanted();
                if wanted != stream.params {
                    stream.params = wanted;
                    Some(wanted)
                } else {
                    None
                }
            }
        };
        match action {
            Some(None) => {
                self.commands.stop(address);
                self.set_render_unfocused(address, false);
            }
            Some(Some(params)) => self.commands.start(address, params),
            _ => {}
        }
    }

    /// Say so when a capture was asked for but produced nothing, which is
    /// otherwise indistinguishable from a window that simply sits still.
    fn warn_if_no_frame_arrives(self: &Arc<Self>, address: &str) {
        let hub = self.clone();
        let address = address.to_owned();
        thread::Builder::new()
            .name("hyprnav-frames-watchdog".to_owned())
            .spawn(move || {
                thread::sleep(Duration::from_secs(2));
                let Ok(streams) = hub.streams.lock() else { return };
                if let Some(stream) = streams.get(&address) {
                    if stream.latest.is_none() {
                        warn!(
                            address = %address,
                            clients = stream.clients.len(),
                            "capture produced no frame within 2 s"
                        );
                    }
                }
            })
            .ok();
    }

    /// End a stream and every client on it.
    fn end(&self, address: &str) {
        let clients = {
            let Ok(mut streams) = self.streams.lock() else { return };
            match streams.remove(address) {
                Some(stream) => stream.clients,
                None => return,
            }
        };
        self.set_render_unfocused(address, false);
        for client in clients {
            client.close();
        }
    }

    // ----------------------------------------------------------- video

    /// Run the encoder probe once, in the background: `auto` costs a few
    /// hundred milliseconds of null encodes and must not delay startup.
    pub fn probe_encoders(self: &Arc<Self>) {
        let hub = self.clone();
        thread::Builder::new()
            .name("hyprnav-frames-probe".to_owned())
            .spawn(move || {
                let found = encoder::probe(&hub.config);
                debug!(
                    encoders = found
                        .iter()
                        .map(|choice| format!(
                            "{}{}",
                            choice.codec.name(),
                            if choice.vaapi { "/vaapi" } else { "/sw" }
                        ))
                        .collect::<Vec<_>>()
                        .join(","),
                    "frame encoders probed"
                );
                if let Ok(mut encoders) = hub.encoders.lock() {
                    *encoders = found;
                }
            })
            .ok();
    }

    /// The first codec the client wants that the config allows and this
    /// machine can actually encode.
    fn choose_encoder(&self, wanted: &[Codec]) -> Option<EncoderChoice> {
        let encoders = self.encoders.lock().ok()?;
        wanted.iter().find_map(|codec| {
            encoders.iter().copied().find(|choice| choice.codec == *codec)
        })
    }

    pub fn pipeline_count(&self) -> usize {
        self.pipelines.lock().map(|pipelines| pipelines.len()).unwrap_or(0)
    }

    /// Join, or start, the pipeline for this request.
    fn join_video(
        self: &Arc<Self>,
        request: &ClientRequest,
    ) -> Result<(PipelineKey, Arc<Pipeline>, Arc<VideoClient>), &'static str> {
        if self.commands.identifier(&request.address).is_none() && self.commands.primed() {
            return Err("unknown_window");
        }
        let choice = self.choose_encoder(&request.codecs).ok_or("no_codec")?;
        let key = PipelineKey {
            address: request.address.clone(),
            codec: choice.codec,
            width: request.stream.max_width,
        };
        let mut pipelines = self.pipelines.lock().map_err(|_| "busy")?;
        pipelines.retain(|_, pipeline| !pipeline.is_stopping());
        if let Some(pipeline) = pipelines.get(&key) {
            let client = pipeline.join();
            return Ok((key, pipeline.clone(), client));
        }
        if pipelines.len() >= self.config.max_pipelines {
            // Over budget: offer the nearest existing pipeline for this window
            // if the client said it would accept that codec, else refuse.
            let nearest = pipelines
                .iter()
                .find(|(other, _)| {
                    other.address == request.address && request.codecs.contains(&other.codec)
                })
                .map(|(other, pipeline)| (other.clone(), pipeline.clone()));
            let Some((other_key, pipeline)) = nearest else { return Err("busy") };
            let client = pipeline.join();
            return Ok((other_key, pipeline, client));
        }
        let paced = self.paced();
        let pipeline = Pipeline::start(
            key.clone(),
            choice,
            request.video(),
            self.config.clone(),
            &self.helper_program,
            paced,
        )
        .map_err(|_| "capture_failed")?;
        pipelines.insert(key.clone(), pipeline.clone());
        drop(pipelines);
        debug!(
            address = %request.address,
            codec = choice.codec.name(),
            width = key.width,
            paced,
            "frames pipeline started"
        );
        self.set_render_unfocused(&request.address, true);
        if !paced {
            // 1000/max_fps is the coalescing window the plugin applies.
            let interval = (1000 / request.stream.fps.max(1)).max(1);
            if let Some(link) = self.plugin() {
                link.watch(&request.address, interval);
            }
        }
        let client = pipeline.join();
        Ok((key, pipeline, client))
    }

    fn leave_video(&self, key: &PipelineKey, client_id: u64) {
        let gone = {
            let Ok(mut pipelines) = self.pipelines.lock() else { return };
            let Some(pipeline) = pipelines.get(key) else { return };
            if !pipeline.leave(client_id) {
                return;
            }
            pipelines.remove(key)
        };
        let Some(pipeline) = gone else { return };
        let source = pipeline.source();
        pipeline.stop();
        if let Some(link) = self.plugin() {
            link.unwatch(&key.address);
            if source != key.address {
                link.unwatch(&source);
            }
        }
        self.set_render_unfocused(&key.address, false);
        if source != key.address {
            self.set_render_unfocused(&source, false);
        }
    }

    fn for_each_pipeline<F: FnMut(&Arc<Pipeline>)>(&self, mut body: F) {
        let Ok(pipelines) = self.pipelines.lock() else { return };
        for pipeline in pipelines.values() {
            body(pipeline);
        }
    }

    /// Serve one connected client: read its request line, then write frames.
    pub fn serve(self: &Arc<Self>, stream: UnixStream) {
        let hub = self.clone();
        thread::Builder::new()
            .name("hyprnav-frames-client".to_owned())
            .spawn(move || {
                if let Err(error) = hub.serve_blocking(stream) {
                    debug!("frames client ended: {error}");
                }
            })
            .ok();
    }

    fn serve_blocking(self: &Arc<Self>, stream: UnixStream) -> std::io::Result<()> {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let mut writer = stream;
        let _ = writer.set_read_timeout(None);

        let Some(request) = parse_request(&line) else {
            writer.write_all(b"{\"error\":\"unknown_window\"}\n")?;
            return Ok(());
        };
        if request.codecs.iter().any(|codec| codec.is_video()) {
            return self.serve_video(reader, writer, request);
        }
        let address = request.address.clone();
        let Some(client) = self.join(&address, request.stream) else {
            writer.write_all(b"{\"error\":\"unknown_window\"}\n")?;
            return Ok(());
        };

        // Watch for the client hanging up. Waiting for a write to fail is not
        // enough: a still window produces no frames at all, so without this the
        // capture would run forever behind a client that is long gone.
        {
            let hub = self.clone();
            let client = client.clone();
            let address = address.clone();
            thread::Builder::new()
                .name("hyprnav-frames-hangup".to_owned())
                .spawn(move || {
                    let mut sink = [0u8; 256];
                    while let Ok(read) = reader.get_mut().read(&mut sink) {
                        if read == 0 {
                            break;
                        }
                    }
                    hub.leave(&address, client.id);
                    client.close();
                })
                .ok();
        }

        while let Some(frame) = client.next_frame() {
            if writer.write_all(&frame).is_err() || writer.flush().is_err() {
                break;
            }
        }
        self.leave(&address, client.id);
        client.close();
        Ok(())
    }

    /// The record stream: a CONFIG record, the GOP cache, then live frames.
    fn serve_video(
        self: &Arc<Self>,
        reader: BufReader<UnixStream>,
        mut writer: UnixStream,
        request: ClientRequest,
    ) -> std::io::Result<()> {
        let (key, _pipeline, client) = match self.join_video(&request) {
            Ok(joined) => joined,
            Err(reason) => {
                writer.write_all(format!("{{\"error\":\"{reason}\"}}\n").as_bytes())?;
                return Ok(());
            }
        };

        // A still window sends nothing for ten seconds at a time, so a client
        // that hangs up has to be noticed by reading, not by a failing write.
        {
            let hub = self.clone();
            let client = client.clone();
            let key = key.clone();
            let mut reader = reader;
            thread::Builder::new()
                .name("hyprnav-frames-hangup".to_owned())
                .spawn(move || {
                    let mut sink = [0u8; 256];
                    while let Ok(read) = reader.get_mut().read(&mut sink) {
                        if read == 0 {
                            break;
                        }
                    }
                    hub.leave_video(&key, client.id);
                    client.close();
                })
                .ok();
        }

        while let Some(record) = client.next() {
            if writer.write_all(&record).is_err() || writer.flush().is_err() {
                break;
            }
        }
        self.leave_video(&key, client.id);
        client.close();
        Ok(())
    }
}

impl DamageSink for FrameHub {
    fn on_window_damaged(&self, address: &str) {
        self.for_each_pipeline(|pipeline| pipeline.on_damage(address));
    }

    /// A dialog opened under a watched window: `follow=transient` pipelines
    /// switch to it without restarting the encoder unless the size changed.
    fn on_transient_mapped(&self, address: &str, parent: &str) {
        let mut follow = Vec::new();
        self.for_each_pipeline(|pipeline| {
            if pipeline.follows_transient() && pipeline.key.address == parent {
                follow.push((pipeline.clone(), pipeline.key.codec));
            }
        });
        if follow.is_empty() {
            return;
        }
        if let Some(link) = self.plugin() {
            // The dialog needs its own damage reports now.
            link.watch(address, 125);
        }
        self.set_render_unfocused(address, true);
        for (pipeline, _) in follow {
            pipeline.retarget(address);
        }
    }

    fn on_transient_unmapped(&self, address: &str) {
        let mut back = Vec::new();
        self.for_each_pipeline(|pipeline| {
            if pipeline.follows_transient() && pipeline.source() == address {
                back.push(pipeline.clone());
            }
        });
        if back.is_empty() {
            return;
        }
        if let Some(link) = self.plugin() {
            link.unwatch(address);
        }
        for pipeline in back {
            let home = pipeline.key.address.clone();
            pipeline.retarget(&home);
        }
    }
}

impl CaptureSink for FrameHub {
    fn on_add(&self, _address: &str, _identifier: &str) {}

    fn on_close(&self, address: &str) {
        self.end(address);
    }

    fn on_capture_failed(&self, address: &str, reason: &str) {
        debug!(address, reason, "capture ended");
        self.end(address);
    }

    fn on_frame(&self, address: &str, jpeg: Vec<u8>) {
        let part = Arc::new(multipart_part(&jpeg));
        let clients = {
            let Ok(mut streams) = self.streams.lock() else { return };
            let Some(stream) = streams.get_mut(address) else { return };
            stream.latest = Some(part.clone());
            stream.clients.clone()
        };
        for client in clients {
            client.offer(part.clone());
        }
    }
}

/// Accept frame clients on `listener` forever.
pub fn start_frame_server(hub: Arc<FrameHub>, listener: UnixListener) {
    thread::Builder::new()
        .name("hyprnav-frames-accept".to_owned())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => hub.serve(stream),
                    Err(error) => warn!("frames accept failed: {error}"),
                }
            }
        })
        .ok();
}

/// Spawn the helper and the accept loop; returns the hub for the daemon.
pub fn start(
    listener: UnixListener,
    instance: String,
    spawn_socket: &std::path::Path,
) -> Arc<FrameHub> {
    let config = FramesConfig::default_path()
        .map(|path| FramesConfig::load(&path))
        .unwrap_or_default();
    let program = CaptureHelper::default_program();
    let helper = CaptureHelper::new(program.clone());
    let hub = FrameHub::with_config(helper.clone(), instance, config.clone(), program);
    helper.spawn(hub.clone());
    if config.force_fallback {
        debug!("frames: force_fallback is set, captures will be paced");
        hub.attach_plugin(PluginLink::disabled());
    } else {
        hub.attach_plugin(PluginLink::start(spawn_socket, hub.clone()));
    }
    hub.probe_encoders();
    start_frame_server(hub.clone(), listener);
    hub
}

/// Connect and send the opening line.
fn open_stream(socket: &std::path::Path, request: &ClientRequest) -> anyhow::Result<UnixStream> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|error| anyhow::anyhow!("connecting to {}: {error}", socket.display()))?;
    let mut line = json!({
        "address": request.address,
        "codecs": request.codecs.iter().map(|codec| codec.name()).collect::<Vec<_>>(),
        "max_fps": request.stream.fps,
        "quality": request.stream.quality,
        "max_width": request.stream.max_width,
    });
    if request.follow_transient {
        line["follow"] = json!("transient");
    }
    stream.write_all(format!("{line}\n").as_bytes())?;
    stream.flush()?;
    Ok(stream)
}

/// Client side of `frames.sock`: send the request line, copy the stream out.
pub fn stream_frames<W: Write>(
    socket: &std::path::Path,
    request: &ClientRequest,
    out: &mut W,
) -> anyhow::Result<()> {
    let mut stream = open_stream(socket, request)?;
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        out.write_all(&buffer[..read])?;
        out.flush()?;
    }
}

/// Same, but unwrap the records into an IVF file `ffprobe` and `ffplay` read.
///
/// The CONFIG record carries no picture, so it is dropped; everything else
/// becomes one IVF frame. The file header is patched with the real geometry
/// and frame count at the end, when the output is seekable.
pub fn stream_to_ivf(
    socket: &std::path::Path,
    request: &ClientRequest,
    out: &mut std::fs::File,
) -> anyhow::Result<()> {
    use crate::video::{RecordHeader, FLAG_CONFIG, FLAG_KEEPALIVE, HEADER_BYTES};
    let codec = request.codecs.first().copied().unwrap_or(Codec::Av1);
    let fourcc: &[u8; 4] = match codec {
        Codec::H264 => b"H264",
        _ => b"AV01",
    };
    let mut stream = open_stream(socket, request)?;
    out.write_all(&crate::video::ivf_header(fourcc, 0, 0, request.stream.fps, 0))?;

    let mut pending = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    let mut frames = 0u64;
    let (mut width, mut height) = (0u16, 0u16);
    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        pending.extend_from_slice(&chunk[..read]);
        loop {
            let Some(header) = RecordHeader::parse(&pending) else {
                if pending.len() >= HEADER_BYTES {
                    return Err(anyhow::anyhow!("frames stream is not HNVF records"));
                }
                break;
            };
            if pending.len() < HEADER_BYTES + header.len {
                break;
            }
            let payload = pending[HEADER_BYTES..HEADER_BYTES + header.len].to_vec();
            pending.drain(..HEADER_BYTES + header.len);
            if header.flags & (FLAG_CONFIG | FLAG_KEEPALIVE) != 0 {
                continue;
            }
            if header.width > 0 {
                width = header.width;
                height = header.height;
            }
            out.write_all(&crate::video::ivf_frame(frames, &payload))?;
            frames += 1;
        }
    }
    out.flush()?;
    use std::io::Seek;
    if out.seek(std::io::SeekFrom::Start(0)).is_ok() {
        out.write_all(&crate::video::ivf_header(
            fourcc,
            width,
            height,
            request.stream.fps,
            frames.min(u32::MAX as u64) as u32,
        ))?;
        out.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingSink {
        commands: Mutex<Vec<String>>,
        known: Mutex<HashMap<String, String>>,
        primed: Mutex<bool>,
    }

    impl RecordingSink {
        fn with_window(address: &str) -> Arc<Self> {
            let sink = Arc::new(Self::default());
            sink.known
                .lock()
                .unwrap()
                .insert(address.to_owned(), "18000004".to_owned());
            *sink.primed.lock().unwrap() = true;
            sink
        }

        fn commands(&self) -> Vec<String> {
            self.commands.lock().unwrap().clone()
        }
    }

    impl CommandSink for RecordingSink {
        fn start(&self, address: &str, params: CaptureParams) {
            self.commands.lock().unwrap().push(format!(
                "start {address} {} {} {}",
                params.max_fps, params.quality, params.max_width
            ));
        }
        fn stop(&self, address: &str) {
            self.commands.lock().unwrap().push(format!("stop {address}"));
        }
        fn primed(&self) -> bool {
            *self.primed.lock().unwrap()
        }
        fn identifier(&self, address: &str) -> Option<String> {
            self.known.lock().unwrap().get(address).cloned()
        }
    }

    #[test]
    fn addresses_are_normalized_to_lowercase_hyprctl_form() {
        assert_eq!(normalize_address("0x55EA1AD9C6D0").unwrap(), "0x55ea1ad9c6d0");
        assert_eq!(normalize_address("address:0x0055ea").unwrap(), "0x55ea");
        assert_eq!(normalize_address(" 0x1 ").unwrap(), "0x1");
        assert!(normalize_address("55ea").is_none());
        assert!(normalize_address("0xzz").is_none());
        assert!(normalize_address("0x").is_none());
    }

    #[test]
    fn request_defaults_and_clamps_every_field() {
        let request = parse_request("{\"address\":\"0xAB\"}").unwrap();
        assert_eq!(request.address, "0xab");
        assert_eq!(request.stream, StreamRequest::default());
        assert_eq!(request.codecs, vec![Codec::Mjpeg], "old clients still get JPEG");
        assert!(!request.follow_transient);

        let request =
            parse_request("{\"address\":\"0xab\",\"fps\":99,\"quality\":5,\"max_width\":2}")
                .unwrap();
        assert_eq!(request.stream.fps, MAX_FPS);
        assert_eq!(request.stream.quality, MIN_QUALITY);
        assert_eq!(request.stream.max_width, MIN_WIDTH);

        assert!(parse_request("{\"fps\":8}").is_none());
        assert!(parse_request("not json").is_none());
    }

    /// The line the browser client sends, and the CLI shorthand.
    #[test]
    fn a_video_request_carries_codecs_max_fps_and_follow() {
        let request = parse_request(
            "{\"address\":\"0x1\",\"codecs\":[\"vp9\",\"av1\",\"h264\",\"mjpeg\"],\
             \"max_width\":960,\"max_fps\":12,\"follow\":\"transient\"}",
        )
        .unwrap();
        // vp9 is not a codec this daemon knows; it is dropped, not fatal.
        assert_eq!(request.codecs, vec![Codec::Av1, Codec::H264, Codec::Mjpeg]);
        assert_eq!(request.stream.max_width, 960);
        assert_eq!(request.stream.fps, 12);
        assert!(request.follow_transient);

        let shorthand = parse_request("{\"address\":\"0x1\",\"format\":\"av1\"}").unwrap();
        assert_eq!(shorthand.codecs, vec![Codec::Av1]);
        assert!(!shorthand.follow_transient);
    }

    #[test]
    fn multipart_part_carries_the_fixed_boundary_and_length() {
        let part = multipart_part(&[1, 2, 3]);
        let text = String::from_utf8_lossy(&part[..part.len() - 5]);
        assert_eq!(
            text,
            "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: 3\r\n\r\n"
        );
        assert_eq!(&part[part.len() - 5..], &[1, 2, 3, b'\r', b'\n']);
    }

    /// Two clients on one window mean one capture, started once and stopped
    /// only when the last of them leaves.
    #[test]
    fn clients_of_one_window_share_a_single_capture() {
        let commands = RecordingSink::with_window("0xab");
        let hub = FrameHub::new(commands.clone());

        let first = hub.join("0xab", StreamRequest::default()).unwrap();
        let second = hub.join("0xab", StreamRequest::default()).unwrap();
        assert_eq!(hub.active_streams(), 1);
        assert_eq!(commands.commands(), vec!["start 0xab 8 60 640"]);

        hub.on_frame("0xab", vec![0xff, 0xd8, 1]);
        assert!(first.next_frame().is_some());
        assert!(second.next_frame().is_some());

        hub.leave("0xab", first.id);
        assert_eq!(commands.commands(), vec!["start 0xab 8 60 640"]);
        hub.leave("0xab", second.id);
        assert_eq!(
            commands.commands(),
            vec!["start 0xab 8 60 640", "stop 0xab"]
        );
        assert_eq!(hub.active_streams(), 0);
    }

    /// The shared capture runs at the loosest settings anyone asked for.
    #[test]
    fn capture_parameters_are_the_maximum_any_client_wants() {
        let commands = RecordingSink::with_window("0xab");
        let hub = FrameHub::new(commands.clone());
        let slow = hub
            .join("0xab", StreamRequest { fps: 2, quality: 40, max_width: 320 })
            .unwrap();
        let fast = hub
            .join("0xab", StreamRequest { fps: 12, quality: 80, max_width: 1280 })
            .unwrap();
        assert_eq!(
            commands.commands(),
            vec!["start 0xab 2 40 320", "start 0xab 12 80 1280"]
        );
        hub.leave("0xab", fast.id);
        // Back down to what the remaining client asked for.
        assert_eq!(commands.commands().last().unwrap(), "start 0xab 2 40 320");
        hub.leave("0xab", slow.id);
        assert_eq!(commands.commands().last().unwrap(), "stop 0xab");
    }

    #[test]
    fn an_unknown_address_never_starts_a_capture() {
        let commands = RecordingSink::with_window("0xab");
        let hub = FrameHub::new(commands.clone());
        assert!(hub.join("0xdeadbeef", StreamRequest::default()).is_none());
        assert_eq!(hub.active_streams(), 0);
        assert!(commands.commands().is_empty());
    }

    /// A client that never reads keeps only the newest frame.
    #[test]
    fn a_slow_client_drops_frames_instead_of_queueing_them() {
        let commands = RecordingSink::with_window("0xab");
        let hub = FrameHub::new(commands);
        let client = hub.join("0xab", StreamRequest::default()).unwrap();
        for n in 0..50u8 {
            hub.on_frame("0xab", vec![0xff, 0xd8, n]);
        }
        let frame = client.next_frame().expect("a frame is waiting");
        assert_eq!(frame[frame.len() - 3], 49, "buffered frame is stale");
        // Nothing else is queued behind it.
        client.close();
        assert!(client.next_frame().is_none());
    }

    /// A client joining a still window sees the last frame straight away, and
    /// a second stop/start cycle works the same as the first.
    #[test]
    fn a_late_client_gets_the_cached_frame_and_restarts_work() {
        let commands = RecordingSink::with_window("0xab");
        let hub = FrameHub::new(commands.clone());

        let first = hub.join("0xab", StreamRequest::default()).unwrap();
        hub.on_frame("0xab", vec![0xff, 0xd8, 7]);
        assert!(first.next_frame().is_some());

        // A second client of the same, now still, window is served at once.
        let second = hub.join("0xab", StreamRequest::default()).unwrap();
        let frame = second.next_frame().expect("late client gets the cache");
        assert_eq!(frame[frame.len() - 3], 7);

        hub.leave("0xab", first.id);
        hub.leave("0xab", second.id);
        assert_eq!(hub.active_streams(), 0);
        assert_eq!(commands.commands().last().unwrap(), "stop 0xab");

        // Starting over asks the helper again and streams as before.
        let third = hub.join("0xab", StreamRequest::default()).unwrap();
        assert_eq!(
            commands.commands(),
            vec!["start 0xab 8 60 640", "stop 0xab", "start 0xab 8 60 640"]
        );
        hub.on_frame("0xab", vec![0xff, 0xd8, 9]);
        let frame = third.next_frame().expect("restarted stream delivers");
        assert_eq!(frame[frame.len() - 3], 9);
    }

    /// The window closing ends every stream on it.
    #[test]
    fn closing_the_window_ends_its_clients() {
        let commands = RecordingSink::with_window("0xab");
        let hub = FrameHub::new(commands);
        let client = hub.join("0xab", StreamRequest::default()).unwrap();
        hub.on_close("0xab");
        assert!(client.next_frame().is_none());
        assert_eq!(hub.active_streams(), 0);
    }

    /// The helper's NDJSON, including a frame record and its raw payload.
    #[test]
    fn helper_output_is_parsed_including_inline_frame_bytes() {
        #[derive(Default)]
        struct Collector {
            events: Mutex<Vec<String>>,
        }
        impl CaptureSink for Collector {
            fn on_add(&self, address: &str, identifier: &str) {
                self.events.lock().unwrap().push(format!("add {address} {identifier}"));
            }
            fn on_close(&self, address: &str) {
                self.events.lock().unwrap().push(format!("close {address}"));
            }
            fn on_frame(&self, address: &str, jpeg: Vec<u8>) {
                self.events
                    .lock()
                    .unwrap()
                    .push(format!("frame {address} {:?}", jpeg));
            }
            fn on_capture_failed(&self, address: &str, reason: &str) {
                self.events.lock().unwrap().push(format!("failed {address} {reason}"));
            }
        }

        let helper = CaptureHelper::new("/nonexistent-hyprnav-capture".to_owned());
        let collector = Collector::default();
        let mut stream: Vec<u8> = Vec::new();
        stream.extend_from_slice(
            b"{\"ev\":\"add\",\"addr\":\"0xab\",\"id\":\"18000004\",\"app\":\"a\",\"title\":\"t\"}\n",
        );
        stream.extend_from_slice(b"{\"ev\":\"ready\"}\n");
        stream.extend_from_slice(
            b"{\"ev\":\"frame\",\"addr\":\"0xab\",\"len\":3,\"w\":4,\"h\":5,\"enc_ms\":1.5}\n",
        );
        stream.extend_from_slice(&[0xff, 0xd8, 0x42]);
        stream.extend_from_slice(b"\n{\"ev\":\"capture_failed\",\"addr\":\"0xab\",\"reason\":\"stopped\"}\n");
        stream.extend_from_slice(b"{\"ev\":\"close\",\"addr\":\"0xab\"}\n");

        helper
            .read_stream(BufReader::new(std::io::Cursor::new(stream)), &collector)
            .unwrap();

        assert_eq!(
            collector.events.into_inner().unwrap(),
            vec![
                "add 0xab 18000004".to_owned(),
                "frame 0xab [255, 216, 66]".to_owned(),
                "failed 0xab stopped".to_owned(),
                "close 0xab".to_owned(),
            ]
        );
        assert!(helper.primed());
        assert_eq!(helper.identifier("0xab"), None, "close forgets the window");
    }
}
