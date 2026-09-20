//! One encoder per watched window: capture helper → ffmpeg → `HNVF` records.
//!
//! A pipeline is keyed by `(address, codec, width)`, so every client that
//! agrees on those three shares one capture, one encoder and one GOP cache.
//!
//! ## How the pixels get from the helper to ffmpeg
//!
//! The daemon creates a pipe per pipeline and hands its write end to
//! `hyprnav-capture` as fd 3 (`--raw-fd 3`), dup'd into place between fork and
//! exec. The helper writes packed 3-byte rows there and a one-line header per
//! frame on its stdout, so the daemon knows the geometry before the pixels
//! arrive. Nothing is passed at runtime, there is no SCM_RIGHTS dance, and the
//! helper's stdout stays a clean NDJSON stream.
//!
//! This costs one extra Wayland connection per pipeline, which is the price of
//! not inventing an fd-passing protocol for the shared helper. The shared
//! helper remains the one that keeps the address index and serves MJPEG.
//!
//! ffmpeg cannot be started before the first frame, because it needs `-s WxH`.
//! So the pump starts it on the first header it sees, and restarts it — with a
//! fresh CONFIG record — whenever the window's size changes.

use crate::encoder::{self, EncoderChoice};
use crate::frames_config::FramesConfig;
use crate::video::{
    self, AnnexBSplitter, Codec, FrameQueue, GopCache, IvfDemuxer, FLAG_CONFIG, FLAG_KEEPALIVE,
    FLAG_KEYFRAME,
};
use serde_json::json;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::io::FromRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, warn};

/// How many whole records a slow client may fall behind before it is resynced.
const CLIENT_QUEUE: usize = 48;
/// The GOP is 16 frames; twice that bounds a misbehaving encoder.
const GOP_CACHE_LIMIT: usize = 32;
/// How many captures in a row may be taken without a damage report.
///
/// A hidden client only paints when it is handed a frame callback, and the
/// standalone capture render is what hands it one -- but a toolkit needs more
/// than a single callback to turn "this label changed" into a committed
/// buffer, so one capture is not enough to start the loop. Measured in the
/// lab: a GTK4 window produces its first commit on the second or third render.
/// `render_unfocused` covers this on a real session, where the monitor is
/// repainting anyway; on an idle headless output nothing renders at all, so
/// the priming burst is what gets the first picture out. It is bounded, and a
/// window that really is still stops producing after it.
const PRIME_BURST: u32 = 3;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PipelineKey {
    pub address: String,
    pub codec: Codec,
    pub width: u32,
}

/// What one connected client asked for beyond the pipeline key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoRequest {
    pub max_fps: u32,
    pub follow_transient: bool,
}

/// One subscriber: a bounded queue of whole records.
pub struct VideoClient {
    pub id: u64,
    queue: Mutex<FrameQueue>,
    ready: Condvar,
}

impl VideoClient {
    fn new(id: u64) -> Arc<Self> {
        Arc::new(Self {
            id,
            queue: Mutex::new(FrameQueue::new(CLIENT_QUEUE)),
            ready: Condvar::new(),
        })
    }

    /// Block until there is a record, or the stream ended.
    pub fn next(&self) -> Option<Arc<Vec<u8>>> {
        let mut queue = self.queue.lock().ok()?;
        loop {
            if let Some(record) = queue.pop() {
                return Some(record);
            }
            if queue.closed {
                return None;
            }
            queue = self.ready.wait(queue).ok()?;
        }
    }

    pub fn close(&self) {
        if let Ok(mut queue) = self.queue.lock() {
            queue.closed = true;
            self.ready.notify_all();
        }
    }

    /// Queue one record. True means the client was too far behind and its
    /// backlog was thrown away: it needs the GOP cache instead.
    fn offer(&self, record: Arc<Vec<u8>>) -> bool {
        let Ok(mut queue) = self.queue.lock() else { return false };
        let overflowed = queue.push_or_resync(record, &[]);
        self.ready.notify_all();
        overflowed
    }

    fn prime(&self, burst: &[Arc<Vec<u8>>]) {
        if let Ok(mut queue) = self.queue.lock() {
            for record in burst {
                queue.push(record.clone());
            }
            self.ready.notify_all();
        }
    }
}

struct Inner {
    clients: Vec<Arc<VideoClient>>,
    gop: GopCache,
    helper_stdin: Option<ChildStdin>,
    /// Helper and, once it exists, ffmpeg; killed together.
    children: Vec<Child>,
    /// The window actually being captured, which is the dialog when
    /// `follow=transient` and one is mapped.
    source: String,
    last_record: Instant,
    last_capture: Instant,
    /// Captures still allowed without a damage report; see PRIME_BURST.
    primes_left: u32,
}

pub struct Pipeline {
    pub key: PipelineKey,
    pub choice: EncoderChoice,
    fps: u32,
    follow_transient: bool,
    paced: bool,
    config: FramesConfig,
    started: Instant,
    stopping: AtomicBool,
    /// How many frames the helper has actually produced: the number the
    /// "a still window costs nothing" claim lives or dies by.
    captures: AtomicU64,
    next_client: AtomicU64,
    inner: Mutex<Inner>,
}

impl Pipeline {
    /// Spawn the helper and the threads that keep this pipeline running.
    pub fn start(
        key: PipelineKey,
        choice: EncoderChoice,
        request: VideoRequest,
        config: FramesConfig,
        helper_program: &str,
        paced: bool,
    ) -> std::io::Result<Arc<Self>> {
        let (read_end, write_end) = make_pipe()?;
        let mut command = Command::new(helper_program);
        command
            .arg("--raw-fd")
            .arg("3")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(write_end, 3) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let spawned = command.spawn();
        // The child holds the write end now; keeping ours open would mean the
        // pump never saw EOF when the helper died.
        unsafe { libc::close(write_end) };
        let mut helper = match spawned {
            Ok(child) => child,
            Err(error) => {
                unsafe { libc::close(read_end) };
                return Err(error);
            }
        };
        let raw = unsafe { std::fs::File::from_raw_fd(read_end) };
        let helper_stdout = helper.stdout.take().expect("piped stdout");
        let helper_stdin = helper.stdin.take();

        let pipeline = Arc::new(Self {
            key: key.clone(),
            choice,
            fps: request.max_fps,
            follow_transient: request.follow_transient,
            paced,
            config,
            started: Instant::now(),
            stopping: AtomicBool::new(false),
            captures: AtomicU64::new(0),
            next_client: AtomicU64::new(0),
            inner: Mutex::new(Inner {
                clients: Vec::new(),
                gop: GopCache::new(GOP_CACHE_LIMIT),
                helper_stdin,
                children: vec![helper],
                source: key.address.clone(),
                last_record: Instant::now(),
                last_capture: Instant::now(),
                primes_left: PRIME_BURST,
            }),
        });

        // The helper's NDJSON tells the pump how big each raw frame is.
        let (sender, receiver) = std::sync::mpsc::channel::<RawHeader>();
        spawn_named("hyprnav-frames-helper", {
            let pipeline = pipeline.clone();
            move || pipeline.read_helper(BufReader::new(helper_stdout), sender)
        });
        spawn_named("hyprnav-frames-pump", {
            let pipeline = pipeline.clone();
            move || pipeline.pump(raw, receiver)
        });
        spawn_named("hyprnav-frames-keepalive", {
            let pipeline = pipeline.clone();
            move || pipeline.keepalive()
        });
        if !paced {
            spawn_named("hyprnav-frames-prime", {
                let pipeline = pipeline.clone();
                move || pipeline.prime()
            });
        }

        pipeline.command(json!({
            "op": "start",
            "addr": key.address,
            "max_width": key.width,
            "max_fps": request.max_fps,
            "mode": "raw",
            "paced": if paced { 1 } else { 0 },
        }));
        Ok(pipeline)
    }

    pub fn captures(&self) -> u64 {
        self.captures.load(Ordering::Relaxed)
    }

    pub fn client_count(&self) -> usize {
        self.inner.lock().map(|inner| inner.clients.len()).unwrap_or(0)
    }

    pub fn source(&self) -> String {
        self.inner
            .lock()
            .map(|inner| inner.source.clone())
            .unwrap_or_else(|_| self.key.address.clone())
    }

    pub fn follows_transient(&self) -> bool {
        self.follow_transient
    }

    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::Relaxed)
    }

    /// Add a client and hand it the GOP cache, so it decodes immediately.
    pub fn join(&self) -> Arc<VideoClient> {
        let client = VideoClient::new(self.next_client.fetch_add(1, Ordering::Relaxed));
        let burst = {
            let Ok(mut inner) = self.inner.lock() else { return client };
            inner.clients.push(client.clone());
            burst_of(&inner.gop)
        };
        client.prime(&burst);
        client
    }

    /// Returns true when the pipeline has no clients left.
    pub fn leave(&self, client_id: u64) -> bool {
        let Ok(mut inner) = self.inner.lock() else { return true };
        inner.clients.retain(|client| client.id != client_id);
        inner.clients.is_empty()
    }

    pub fn stop(&self) {
        if self.stopping.swap(true, Ordering::Relaxed) {
            return;
        }
        let Ok(mut inner) = self.inner.lock() else { return };
        inner.helper_stdin = None;
        for mut child in std::mem::take(&mut inner.children) {
            let _ = child.kill();
            let _ = child.wait();
        }
        for client in std::mem::take(&mut inner.clients) {
            client.close();
        }
    }

    /// The compositor says this window repainted: take exactly one frame.
    pub fn on_damage(&self, address: &str) {
        if self.paced || self.source() != address {
            return;
        }
        if let Ok(mut inner) = self.inner.lock() {
            inner.primes_left = PRIME_BURST;
        }
        self.capture(address);
    }

    fn capture(&self, address: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.last_capture = Instant::now();
        }
        self.command(json!({"op": "capture", "addr": address}));
    }

    /// Hand the client enough frame callbacks to start painting, then stop.
    fn prime(self: Arc<Self>) {
        let interval = Duration::from_millis((1000 / self.fps.max(1)).max(16) as u64);
        loop {
            thread::sleep(interval);
            if self.is_stopping() {
                return;
            }
            let due = match self.inner.lock() {
                Ok(mut inner) => {
                    if inner.primes_left > 0 && inner.last_capture.elapsed() >= interval * 2 {
                        inner.primes_left -= 1;
                        true
                    } else {
                        false
                    }
                }
                Err(_) => return,
            };
            if due {
                let source = self.source();
                self.capture(&source);
            }
        }
    }

    /// Point the capture at `address`. The encoder is left alone; if the new
    /// window is a different size the pump restarts it on the next frame.
    pub fn retarget(&self, address: &str) {
        let previous = {
            let Ok(mut inner) = self.inner.lock() else { return };
            if inner.source == address {
                return;
            }
            std::mem::replace(&mut inner.source, address.to_owned())
        };
        if let Ok(mut inner) = self.inner.lock() {
            inner.primes_left = PRIME_BURST;
        }
        self.command(json!({"op": "stop", "addr": previous}));
        self.command(json!({
            "op": "start",
            "addr": address,
            "max_width": self.key.width,
            "max_fps": self.fps,
            "mode": "raw",
            "paced": if self.paced { 1 } else { 0 },
        }));
        debug!(from = %previous, to = %address, "frames pipeline followed a transient");
    }

    fn command(&self, value: serde_json::Value) {
        let Ok(mut inner) = self.inner.lock() else { return };
        let Some(stdin) = inner.helper_stdin.as_mut() else { return };
        if stdin.write_all(format!("{value}\n").as_bytes()).is_err() || stdin.flush().is_err() {
            inner.helper_stdin = None;
        }
    }

    fn read_helper<R: BufRead>(
        self: Arc<Self>,
        mut reader: R,
        sender: std::sync::mpsc::Sender<RawHeader>,
    ) {
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else { continue };
            let number = |key: &str| value.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
            match value.get("ev").and_then(|v| v.as_str()).unwrap_or_default() {
                "raw" => {
                    let count = self.captures.fetch_add(1, Ordering::Relaxed) + 1;
                    debug!(
                        count,
                        bytes = number("len"),
                        width = number("w"),
                        height = number("h"),
                        "captured a raw frame"
                    );
                    let header = RawHeader {
                        len: number("len") as usize,
                        width: number("w") as u32,
                        height: number("h") as u32,
                        real_height: number("real_h").max(1) as u32,
                        pix: value
                            .get("pix")
                            .and_then(|v| v.as_str())
                            .unwrap_or("bgr24")
                            .to_owned(),
                    };
                    if sender.send(header).is_err() {
                        break;
                    }
                }
                "close" | "capture_failed" => {
                    let gone = value.get("addr").and_then(|v| v.as_str()).unwrap_or_default();
                    if gone == self.source() {
                        warn!(address = gone, "frames capture ended");
                        break;
                    }
                }
                _ => {}
            }
        }
        self.stop();
    }

    /// Read raw frames off the pipe and keep an ffmpeg fed with them.
    ///
    /// ffmpeg holds a picture until the next one arrives: measured, one raw
    /// frame in produces zero bytes out, two produce both. A damage-driven
    /// stream would therefore always be one change behind, and a window that
    /// changes once and then sits still would show nothing at all. So when no
    /// new frame turns up promptly, the last one is written a second time to
    /// push it through. The duplicate costs one encode and codes to a handful
    /// of bytes, and it only happens when the window has gone quiet.
    fn pump(self: Arc<Self>, mut raw: std::fs::File, headers: std::sync::mpsc::Receiver<RawHeader>) {
        let mut encoder: Option<RunningEncoder> = None;
        let mut buffer = Vec::new();
        let grace = Duration::from_millis(((1000 / self.fps.max(1)).max(150)) as u64);
        let mut unflushed = false;
        loop {
            let header = match headers.recv_timeout(grace) {
                Ok(header) => header,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if unflushed && !self.is_stopping() {
                        if let Some(running) = encoder.as_mut() {
                            if running.write(&buffer).is_err() {
                                encoder = None;
                            }
                        }
                        unflushed = false;
                    }
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };
            if self.is_stopping() {
                break;
            }
            buffer.resize(header.len, 0);
            if raw.read_exact(&mut buffer).is_err() {
                break;
            }
            if encoder.as_ref().map(|running| !running.matches(&header)).unwrap_or(true) {
                if let Some(previous) = encoder.take() {
                    previous.stop();
                }
                match self.spawn_encoder(&header) {
                    Ok(running) => encoder = Some(running),
                    Err(error) => {
                        warn!("frames encoder failed to start: {error}");
                        break;
                    }
                }
            }
            let Some(running) = encoder.as_mut() else { break };
            if running.write(&buffer).is_err() {
                encoder = None;
            }
            unflushed = true;
        }
        if let Some(running) = encoder.take() {
            running.stop();
        }
        self.stop();
    }

    fn spawn_encoder(self: &Arc<Self>, header: &RawHeader) -> std::io::Result<RunningEncoder> {
        let args = encoder::ffmpeg_args(
            &self.config,
            self.choice,
            &header.pix,
            header.width,
            header.height,
            self.fps,
        );
        debug!(args = %args.join(" "), "starting ffmpeg");
        let mut child = Command::new("ffmpeg")
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // ffmpeg's complaints are the only diagnosis when a pipeline goes
            // quiet, and at -loglevel error there are none when it is happy.
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let pid = child.id();
        if let Ok(mut inner) = self.inner.lock() {
            // A new encoder means a new sequence header: nothing cached from
            // the old one is decodable by a client that joins after it.
            inner.gop = GopCache::new(GOP_CACHE_LIMIT);
            inner.children.push(child);
        }
        // The record header carries the picture height, not the padded height
        // the encoder was given; clients that care crop to it.
        let height = header.real_height.min(u16::MAX as u32) as u16;
        let width = header.width.min(u16::MAX as u32) as u16;
        spawn_named("hyprnav-frames-encoder", {
            let pipeline = self.clone();
            move || pipeline.read_encoder(stdout, width, height)
        });
        Ok(RunningEncoder {
            stdin: Some(stdin),
            pid,
            width: header.width,
            height: header.height,
            pix: header.pix.clone(),
        })
    }

    /// Demux the encoder's container into one record per picture.
    fn read_encoder(self: Arc<Self>, mut stdout: std::process::ChildStdout, width: u16, height: u16) {
        let mut chunk = vec![0u8; 64 * 1024];
        let mut ivf = IvfDemuxer::default();
        let mut annexb = AnnexBSplitter::default();
        let mut pending = Vec::new();
        loop {
            let read = match stdout.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            if self.is_stopping() {
                return;
            }
            let units: Vec<Vec<u8>> = match self.key.codec {
                Codec::Av1 => {
                    pending.extend_from_slice(&chunk[..read]);
                    ivf.drain(&mut pending)
                }
                Codec::H264 => {
                    annexb.push(&chunk[..read]);
                    annexb.drain()
                }
                Codec::Mjpeg => continue,
            };
            for unit in units {
                self.emit_unit(&unit, width, height);
            }
        }
        if let Some(tail) = annexb.flush() {
            if !tail.is_empty() && !self.is_stopping() {
                self.emit_unit(&tail, width, height);
            }
        }
    }

    fn emit_unit(&self, unit: &[u8], width: u16, height: u16) {
        let (keyframe, out_of_band) = match self.key.codec {
            Codec::Av1 => {
                let parsed = video::parse_av1_unit(unit);
                (parsed.keyframe, parsed.sequence_header)
            }
            Codec::H264 => {
                let parsed = video::parse_h264_unit(unit);
                (parsed.keyframe, parsed.parameter_sets)
            }
            Codec::Mjpeg => (true, None),
        };
        let pts_us = self.started.elapsed().as_micros() as u64;
        let need_config = match self.inner.lock() {
            Ok(inner) => !inner.gop.has_config(),
            Err(_) => return,
        };
        if need_config {
            // A client cannot configure a decoder without this, so it goes out
            // before the picture it describes.
            let payload =
                video::config_payload(self.key.codec, out_of_band.as_deref().unwrap_or(&[]));
            let record = video::encode_record(FLAG_CONFIG, pts_us, width, height, &payload);
            if let Ok(mut inner) = self.inner.lock() {
                inner.gop.set_config(record.clone());
            }
            self.fan_out(record);
        }
        let flags = if keyframe { FLAG_KEYFRAME } else { 0 };
        debug!(bytes = unit.len(), keyframe, "encoded a record");
        let record = video::encode_record(flags, pts_us, width, height, unit);
        if let Ok(mut inner) = self.inner.lock() {
            inner.gop.push(record.clone(), keyframe);
        }
        self.fan_out(record);
    }

    fn fan_out(&self, record: Vec<u8>) {
        let record = Arc::new(record);
        let clients = {
            let Ok(mut inner) = self.inner.lock() else { return };
            inner.last_record = Instant::now();
            inner.clients.clone()
        };
        let mut behind = Vec::new();
        for client in clients {
            if client.offer(record.clone()) {
                behind.push(client);
            }
        }
        if behind.is_empty() {
            return;
        }
        // Only now is the burst worth building: it is a whole GOP of clones.
        let burst = match self.inner.lock() {
            Ok(inner) => burst_of(&inner.gop),
            Err(_) => return,
        };
        for client in behind {
            client.prime(&burst);
        }
    }

    /// Tell "static" apart from "dead" while a window sits still.
    fn keepalive(self: Arc<Self>) {
        loop {
            thread::sleep(Duration::from_secs(1));
            if self.is_stopping() {
                return;
            }
            let due = match self.inner.lock() {
                Ok(inner) => inner.last_record.elapsed() >= video::KEEPALIVE,
                Err(_) => return,
            };
            if due {
                let pts_us = self.started.elapsed().as_micros() as u64;
                self.fan_out(video::encode_record(FLAG_KEEPALIVE, pts_us, 0, 0, &[]));
            }
        }
    }
}

fn burst_of(gop: &GopCache) -> Vec<Arc<Vec<u8>>> {
    gop.burst().into_iter().map(Arc::new).collect()
}

#[derive(Clone, Debug)]
struct RawHeader {
    len: usize,
    width: u32,
    height: u32,
    real_height: u32,
    pix: String,
}

struct RunningEncoder {
    stdin: Option<ChildStdin>,
    pid: u32,
    width: u32,
    height: u32,
    pix: String,
}

impl RunningEncoder {
    fn matches(&self, header: &RawHeader) -> bool {
        self.width == header.width && self.height == header.height && self.pix == header.pix
    }

    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(std::io::Error::other("encoder gone"));
        };
        stdin.write_all(bytes)?;
        stdin.flush()
    }

    fn stop(mut self) {
        self.stdin = None; // closing stdin lets ffmpeg drain and exit
        unsafe { libc::kill(self.pid as i32, libc::SIGTERM) };
    }
}

fn make_pipe() -> std::io::Result<(i32, i32)> {
    let mut fds = [0i32; 2];
    // CLOEXEC on both: the write end lands on fd 3 through dup2 in the child,
    // which clears it there, and the read end must not leak anywhere.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((fds[0], fds[1]))
}

fn spawn_named<F: FnOnce() + Send + 'static>(name: &str, body: F) {
    thread::Builder::new().name(name.to_owned()).spawn(body).ok();
}
