//! Push notifications for agent, slot and lock state.
//!
//! The daemon owns a second Unix socket beside the request socket
//! (`events.sock`). Any number of clients may connect; the daemon only ever
//! writes to them and ignores whatever they send. Every event is one JSON
//! object per line:
//!
//! ```text
//! {"event":"hello","ts_ms":…,"version":1}
//! {"event":"agents","ts_ms":…,"agents":[…]}
//! {"event":"slots","ts_ms":…}
//! {"event":"locked","ts_ms":…,"seq":42,
//!  "locked_environment_id":"p.….t.B"|null,"previous_environment_id":"p.….t.A"|null,
//!  "cause":"snapshot|lock_set|lock_clear|workspace_goto|workspace_goto_physical|focus|env_delete|batch_mutate",
//!  "origin":"t3code"|null,
//!  "environment":{"title":…|null,"cwd":…|null,"chain":["p.…","p.….w.…","p.….t.B"]}|null}
//! ```
//!
//! On connect a subscriber receives `hello`, then a full `agents` event, then
//! a `slots` event, then the current lock as `locked` with
//! `cause:"snapshot"` and `previous_environment_id:null`, so it never has to
//! poll for an initial state.
//!
//! `locked` is sent immediately, never coalesced, and only when the locked
//! environment actually changes; setting the same lock twice is silent. `seq`
//! increases by one per change (the snapshot repeats the latest `seq`), so a
//! client can drop stale lines. `origin` is the requester's free-form tag
//! (`hyprnav --origin <tag>`, or `"origin"` on the request), null for the
//! Hyprland focus watcher. `environment` is null when nothing is locked.
//!
//! Mutations only flip a dirty flag and wake the broadcaster thread; the
//! broadcaster waits [`COALESCE_MS`] before it snapshots and fans out, so a
//! chatty agent producing hundreds of beats a second still yields at most
//! twenty events a second. A subscriber whose queue fills (a stopped or
//! wedged client) is dropped rather than allowed to stall the daemon.

use serde_json::json;
use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;
use tracing::{debug, warn};

/// Wire version reported in the `hello` event.
pub const EVENTS_PROTOCOL_VERSION: u32 = 1;

/// Burst window: at most one `agents` and one `slots` event per this many ms.
const COALESCE_MS: u64 = 50;

/// Lines queued for one subscriber before it is considered wedged.
const SUBSCRIBER_QUEUE: usize = 64;

/// A blocked write longer than this means the peer is not draining.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

struct Subscriber {
    id: u64,
    tx: SyncSender<Arc<String>>,
}

#[derive(Default)]
struct BusState {
    subscribers: Vec<Subscriber>,
    agents_dirty: bool,
    slots_dirty: bool,
    next_id: u64,
    /// Number of lock changes seen; the `seq` of the latest `locked` event.
    lock_seq: u64,
    stop: bool,
}

struct Inner {
    state: Mutex<BusState>,
    wake: Condvar,
}

/// Fan-out point for `agents` and `slots` events. Cheap to clone.
#[derive(Clone)]
pub struct EventBus {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self
            .inner
            .state
            .lock()
            .map(|state| state.subscribers.len())
            .unwrap_or(0);
        formatter
            .debug_struct("EventBus")
            .field("subscribers", &count)
            .finish()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(BusState::default()),
                wake: Condvar::new(),
            }),
        }
    }

    /// Mark the agent registry changed; an `agents` event follows within
    /// [`COALESCE_MS`].
    pub fn agents_changed(&self) {
        self.mark(true, false);
    }

    /// Mark slot/environment/stick state changed; a `slots` event follows
    /// within [`COALESCE_MS`].
    pub fn slots_changed(&self) {
        self.mark(false, true);
    }

    pub fn both_changed(&self) {
        self.mark(true, true);
    }

    fn mark(&self, agents: bool, slots: bool) {
        let Ok(mut state) = self.inner.state.lock() else {
            return;
        };
        if state.subscribers.is_empty() {
            // Nobody is listening: do not keep stale dirty flags around, a
            // fresh subscriber gets a full snapshot on connect anyway.
            state.agents_dirty = false;
            state.slots_dirty = false;
            return;
        }
        state.agents_dirty |= agents;
        state.slots_dirty |= slots;
        self.inner.wake.notify_all();
    }

    /// Announce that the locked environment changed. Unlike `agents` and
    /// `slots` this fans out at once: every change matters to followers.
    /// `environment` is only evaluated when someone is listening.
    pub fn lock_changed(
        &self,
        locked: Option<&str>,
        previous: Option<&str>,
        cause: &str,
        origin: Option<&str>,
        environment: impl FnOnce() -> serde_json::Value,
    ) {
        if self.subscriber_count() == 0 {
            if let Ok(mut state) = self.inner.state.lock() {
                state.lock_seq += 1;
            }
            return;
        }
        let environment = environment();
        let Ok(mut state) = self.inner.state.lock() else {
            return;
        };
        state.lock_seq += 1;
        let line = locked_line(state.lock_seq, locked, previous, cause, origin, environment);
        Self::fan_out_locked(&mut state, line);
    }

    fn lock_seq(&self) -> u64 {
        self.inner
            .state
            .lock()
            .map(|state| state.lock_seq)
            .unwrap_or(0)
    }

    /// A raw receiver on the bus, for tests that watch what gets emitted.
    #[cfg(test)]
    pub(crate) fn test_subscribe(&self) -> std::sync::mpsc::Receiver<Arc<String>> {
        let (tx, rx) = sync_channel::<Arc<String>>(SUBSCRIBER_QUEUE);
        let mut state = self.inner.state.lock().unwrap();
        state.next_id += 1;
        let id = state.next_id;
        state.subscribers.push(Subscriber { id, tx });
        rx
    }

    pub fn subscriber_count(&self) -> usize {
        self.inner
            .state
            .lock()
            .map(|state| state.subscribers.len())
            .unwrap_or(0)
    }

    /// Register a stream. The caller supplies the initial burst
    /// (hello + agents + slots) which is queued before any live event.
    fn add_subscriber(&self, stream: UnixStream, initial: Vec<Arc<String>>) {
        let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
        let (tx, rx) = sync_channel::<Arc<String>>(SUBSCRIBER_QUEUE);
        for line in initial {
            if tx.try_send(line).is_err() {
                return;
            }
        }
        let id = {
            let Ok(mut state) = self.inner.state.lock() else {
                return;
            };
            state.next_id += 1;
            let id = state.next_id;
            state.subscribers.push(Subscriber { id, tx });
            id
        };
        let bus = self.clone();
        thread::Builder::new()
            .name(format!("hyprnav-events-{id}"))
            .spawn(move || {
                let mut stream = stream;
                while let Ok(line) = rx.recv() {
                    if stream.write_all(line.as_bytes()).is_err() || stream.flush().is_err() {
                        break;
                    }
                }
                bus.drop_subscriber(id);
                debug!(subscriber = id, "events subscriber gone");
            })
            .ok();
    }

    fn drop_subscriber(&self, id: u64) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.subscribers.retain(|subscriber| subscriber.id != id);
        }
    }

    /// Queue one line on every subscriber, dropping the ones that cannot keep
    /// up. Never blocks.
    fn fan_out(&self, line: Arc<String>) {
        let Ok(mut state) = self.inner.state.lock() else {
            return;
        };
        Self::fan_out_locked(&mut state, line);
    }

    fn fan_out_locked(state: &mut BusState, line: Arc<String>) {
        let mut wedged = Vec::new();
        for subscriber in &state.subscribers {
            match subscriber.tx.try_send(line.clone()) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    warn!(subscriber = subscriber.id, "events subscriber is not draining, dropping it");
                    wedged.push(subscriber.id);
                }
                Err(TrySendError::Disconnected(_)) => wedged.push(subscriber.id),
            }
        }
        if !wedged.is_empty() {
            state
                .subscribers
                .retain(|subscriber| !wedged.contains(&subscriber.id));
        }
    }

    /// Block until something is dirty, then coalesce for [`COALESCE_MS`] and
    /// return which streams to emit. `None` means shutdown.
    fn take_dirty(&self) -> Option<(bool, bool)> {
        let mut state = self.inner.state.lock().ok()?;
        while !state.stop && !state.agents_dirty && !state.slots_dirty {
            state = self.inner.wake.wait(state).ok()?;
        }
        if state.stop {
            return None;
        }
        drop(state);
        // Coalesce the burst: everything that lands in this window collapses
        // into the single snapshot taken after it.
        thread::sleep(Duration::from_millis(COALESCE_MS));
        let mut state = self.inner.state.lock().ok()?;
        let dirty = (state.agents_dirty, state.slots_dirty);
        state.agents_dirty = false;
        state.slots_dirty = false;
        Some(dirty)
    }
}

pub fn hello_line() -> Arc<String> {
    line(json!({
        "event": "hello",
        "ts_ms": now_ms(),
        "version": EVENTS_PROTOCOL_VERSION,
    }))
}

pub fn agents_line(agents: serde_json::Value) -> Arc<String> {
    line(json!({
        "event": "agents",
        "ts_ms": now_ms(),
        "agents": agents,
    }))
}

pub fn slots_line() -> Arc<String> {
    line(json!({
        "event": "slots",
        "ts_ms": now_ms(),
    }))
}

pub fn locked_line(
    seq: u64,
    locked: Option<&str>,
    previous: Option<&str>,
    cause: &str,
    origin: Option<&str>,
    environment: serde_json::Value,
) -> Arc<String> {
    line(json!({
        "event": "locked",
        "ts_ms": now_ms(),
        "seq": seq,
        "locked_environment_id": locked,
        "previous_environment_id": previous,
        "cause": cause,
        "origin": origin,
        "environment": environment,
    }))
}

fn line(value: serde_json::Value) -> Arc<String> {
    Arc::new(format!("{value}\n"))
}

/// Accept subscribers on `path` and fan events out to them.
///
/// `agents_snapshot` returns the same JSON `agents_list` does.
/// `lock_snapshot` returns the locked environment id and its `environment`
/// object, for the `locked` line of the connect burst.
pub fn start_event_server<F, L>(
    bus: EventBus,
    listener: UnixListener,
    agents_snapshot: F,
    lock_snapshot: L,
) where
    F: Fn() -> serde_json::Value + Send + Sync + 'static,
    L: Fn() -> (Option<String>, serde_json::Value) + Send + 'static,
{
    let snapshot = Arc::new(agents_snapshot);

    {
        let bus = bus.clone();
        let snapshot = snapshot.clone();
        thread::Builder::new()
            .name("hyprnav-events-accept".to_owned())
            .spawn(move || {
                for stream in listener.incoming() {
                    match stream {
                        Ok(stream) => {
                            let (locked, environment) = lock_snapshot();
                            let initial = vec![
                                hello_line(),
                                agents_line(snapshot()),
                                slots_line(),
                                locked_line(
                                    bus.lock_seq(),
                                    locked.as_deref(),
                                    None,
                                    "snapshot",
                                    None,
                                    environment,
                                ),
                            ];
                            bus.add_subscriber(stream, initial);
                        }
                        Err(error) => warn!("events accept failed: {error}"),
                    }
                }
            })
            .ok();
    }

    thread::Builder::new()
        .name("hyprnav-events-broadcast".to_owned())
        .spawn(move || {
            while let Some((agents_dirty, slots_dirty)) = bus.take_dirty() {
                if agents_dirty {
                    bus.fan_out(agents_line(snapshot()));
                }
                if slots_dirty {
                    bus.fan_out(slots_line());
                }
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::time::Instant;

    fn temp_socket(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "hyprnav-events-test-{label}-{}-{}.sock",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn subscriber_receives_hello_agents_slots_locked_on_connect() {
        let path = temp_socket("hello");
        let listener = UnixListener::bind(&path).unwrap();
        let bus = EventBus::new();
        start_event_server(bus.clone(), listener, || json!([]), || (None, json!(null)));

        let client = UnixStream::connect(&path).unwrap();
        let mut reader = BufReader::new(client);
        let mut names = Vec::new();
        let mut last = serde_json::Value::Null;
        for _ in 0..4 {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let value: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            assert!(value["ts_ms"].as_u64().unwrap() > 0);
            names.push(value["event"].as_str().unwrap().to_owned());
            last = value;
        }
        assert_eq!(names, vec!["hello", "agents", "slots", "locked"]);
        assert_eq!(last["cause"], "snapshot");
        assert_eq!(last["seq"], 0);
        assert!(last["locked_environment_id"].is_null());
        assert!(last["environment"].is_null());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bursts_are_coalesced_into_one_event() {
        let path = temp_socket("coalesce");
        let listener = UnixListener::bind(&path).unwrap();
        let bus = EventBus::new();
        start_event_server(bus.clone(), listener, || json!([]), || (None, json!(null)));

        let client = UnixStream::connect(&path).unwrap();
        let mut reader = BufReader::new(client);
        for _ in 0..4 {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
        }

        for _ in 0..200 {
            if bus.subscriber_count() == 0 {
                thread::sleep(Duration::from_millis(5));
            }
            bus.agents_changed();
        }
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(line.contains("\"agents\""));

        // A second event would only arrive if the burst was not coalesced.
        reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        let mut extra = String::new();
        assert!(reader.read_line(&mut extra).is_err() || extra.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn wedged_subscriber_is_dropped_and_healthy_one_survives() {
        let bus = EventBus::new();
        // A subscriber whose writer never drains (a stopped process) and one
        // that drains every line.
        let (wedged_tx, _wedged_rx) = sync_channel::<Arc<String>>(SUBSCRIBER_QUEUE);
        let (healthy_tx, healthy_rx) = sync_channel::<Arc<String>>(SUBSCRIBER_QUEUE);
        {
            let mut state = bus.inner.state.lock().unwrap();
            state.subscribers.push(Subscriber { id: 1, tx: wedged_tx });
            state.subscribers.push(Subscriber { id: 2, tx: healthy_tx });
        }

        let drainer = thread::spawn(move || {
            let mut seen = 0;
            while healthy_rx.recv().is_ok() {
                seen += 1;
            }
            seen
        });

        let started = Instant::now();
        for _ in 0..(SUBSCRIBER_QUEUE * 4) {
            bus.fan_out(slots_line());
        }
        // fan_out must never block on the wedged peer.
        assert!(started.elapsed() < Duration::from_secs(1));

        let remaining: Vec<u64> = bus
            .inner
            .state
            .lock()
            .unwrap()
            .subscribers
            .iter()
            .map(|subscriber| subscriber.id)
            .collect();
        assert_eq!(remaining, vec![2], "wedged subscriber should have been dropped");

        bus.inner.state.lock().unwrap().subscribers.clear();
        let seen = drainer.join().unwrap();
        assert!(seen >= SUBSCRIBER_QUEUE, "healthy subscriber lost events: {seen}");
    }

    #[test]
    fn lock_changes_fan_out_immediately_with_increasing_seq() {
        let bus = EventBus::new();
        // Without subscribers only the counter moves; the environment is not built.
        bus.lock_changed(Some("a"), None, "lock_set", None, || unreachable!());
        let rx = bus.test_subscribe();
        bus.lock_changed(
            Some("b"),
            Some("a"),
            "focus",
            Some("t3code"),
            || json!({"title":"B"}),
        );
        let value: serde_json::Value = serde_json::from_str(rx.try_recv().unwrap().trim()).unwrap();
        assert_eq!(value["event"], "locked");
        assert_eq!(value["seq"], 2);
        assert_eq!(value["locked_environment_id"], "b");
        assert_eq!(value["previous_environment_id"], "a");
        assert_eq!(value["origin"], "t3code");
        assert_eq!(value["environment"]["title"], "B");
    }

    #[test]
    fn marking_without_subscribers_does_not_queue_work() {
        let bus = EventBus::new();
        bus.agents_changed();
        bus.slots_changed();
        let state = bus.inner.state.lock().unwrap();
        assert!(!state.agents_dirty);
        assert!(!state.slots_dirty);
    }
}
