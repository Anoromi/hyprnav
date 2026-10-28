//! Local Firefox/Chromium native-messaging transport. No listening TCP port.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const LIMIT: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum BrowserKind {
    #[default]
    Firefox,
    Chromium,
}
impl BrowserKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Firefox => "firefox",
            Self::Chromium => "chromium",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BrowserTarget {
    #[serde(default)]
    pub browser: BrowserKind,
    pub name: String,
    pub workspace: String,
}

/// A running native host: the browser that launched it and its extension kind.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Host {
    pub kind: BrowserKind,
    pub name: String,
}
impl Host {
    fn file(&self, suffix: &str) -> PathBuf {
        bridge_dir().join(format!("{}.{}.{suffix}", self.kind.as_str(), self.name))
    }
}

fn bridge_dir() -> PathBuf {
    crate::runtime_paths::runtime_root().join("hyprnav-browser")
}

/// Which browser serves hyprnav's tabs, and the tabs `tab open` registered, so
/// any browser can serve a slot after the primary changes.
#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    primary: Option<String>,
    #[serde(default)]
    tabs: BTreeMap<String, TabConfig>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct TabConfig {
    url: String,
    param: String,
}

fn state_path() -> PathBuf {
    crate::runtime_paths::state_root().join("browser.json")
}
fn load_state() -> Result<State> {
    match fs::read(state_path()) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("invalid browser state"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(error) => Err(error.into()),
    }
}
fn save_state(state: &State) -> Result<()> {
    let path = state_path();
    crate::runtime_paths::ensure_parent_dir(&path)?;
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&temporary, serde_json::to_vec_pretty(state)?)?;
    fs::rename(&temporary, &path)?;
    Ok(())
}

fn try_lock(file: &fs::File) -> bool {
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// Hosts whose lock is held, i.e. whose browser is running with the extension.
pub fn connected() -> Result<Vec<Host>> {
    let entries = match fs::read_dir(bridge_dir()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut hosts = Vec::new();
    for entry in entries {
        let file_name = entry?.file_name();
        let Some((kind, name)) = file_name
            .to_str()
            .and_then(|file| file.strip_suffix(".lock"))
            .and_then(|stem| stem.split_once('.'))
        else {
            continue;
        };
        let Ok(kind) = <BrowserKind as clap::ValueEnum>::from_str(kind, false) else {
            continue;
        };
        let host = Host {
            kind,
            name: name.to_owned(),
        };
        // Taking the lock means its host has exited; dropping the file frees it.
        if !try_lock(&fs::File::open(host.file("lock"))?) {
            hosts.push(host);
        }
    }
    hosts.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(hosts)
}

/// The primary browser when it runs, else a browser of the requested kind (any
/// browser when none of that kind runs). The note says why a fallback was used.
fn choose(
    kind: BrowserKind,
    hosts: &[Host],
    primary: Option<&str>,
) -> Result<(Host, Option<String>)> {
    if let Some(host) = hosts
        .iter()
        .find(|host| Some(host.name.as_str()) == primary)
    {
        return Ok((host.clone(), None));
    }
    let same_kind: Vec<&Host> = hosts.iter().filter(|host| host.kind == kind).collect();
    let candidates = if same_kind.is_empty() {
        hosts.iter().collect()
    } else {
        same_kind
    };
    let Some(&host) = candidates.first() else {
        bail!(
            "no browser with the hyprnav extension is running; start one, or run `hyprnav tab install` and load the extension"
        );
    };
    let note = match primary {
        Some(primary) => Some(format!(
            "primary browser {primary} is not running; using {}",
            host.name
        )),
        None if hosts.len() > 1 => Some(format!(
            "no primary browser is set; using {} (set one in the hyprnav extension's popup)",
            host.name
        )),
        None => None,
    };
    Ok((host.clone(), note))
}

fn read_frame(reader: &mut impl Read) -> Result<Value> {
    let mut header = [0; 4];
    reader.read_exact(&mut header)?;
    let length = u32::from_ne_bytes(header) as usize;
    if length == 0 || length > LIMIT {
        bail!("invalid browser message size");
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

fn write_frame(writer: &mut impl Write, value: &Value) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    if body.len() > LIMIT {
        bail!("browser message too large");
    }
    writer.write_all(&(body.len() as u32).to_ne_bytes())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

pub fn request(browser: BrowserKind, mut value: Value) -> Result<Value> {
    let state = load_state()?;
    let (host, note) = choose(browser, &connected()?, state.primary.as_deref())?;
    if let Some(note) = note {
        tracing::warn!("{note}");
        eprintln!("hyprnav: {note}");
    }
    // A browser that never saw `tab open` for this name still knows the tab.
    if value["op"] == "goto" {
        if let Some(config) = value["name"].as_str().and_then(|name| state.tabs.get(name)) {
            value["url"] = json!(config.url);
            value["param"] = json!(config.param);
        }
    }
    let mut stream = UnixStream::connect(host.file("sock"))
        .with_context(|| format!("browser bridge of {} unavailable", host.name))?;
    stream.set_read_timeout(Some(TIMEOUT + Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    write_frame(&mut stream, &value)?;
    let response = read_frame(&mut stream)?;
    if let Some(error) = response.get("error").and_then(Value::as_str) {
        bail!("{}: {error}", host.name);
    }
    let mut result = response["result"].clone();
    if let Some(object) = result.as_object_mut() {
        object.insert("browser".into(), json!(host.name));
    }
    Ok(result)
}

pub fn open(browser: BrowserKind, name: String, url: String, param: String) -> Result<Value> {
    let result = request(
        browser,
        json!({"op":"open", "name":name, "url":url, "param":param}),
    )?;
    let mut state = load_state()?;
    state.tabs.insert(name, TabConfig { url, param });
    save_state(&state)?;
    Ok(result)
}

pub fn navigate(target: &BrowserTarget) -> Result<Value> {
    request(
        target.browser,
        json!({"op":"goto", "name":target.name, "workspace":target.workspace}),
    )
}

/// The launching browser's executable name ("Island", "chromium", "zen"). The
/// native host launchers `exec`, so the browser is the direct parent.
fn parent_browser() -> Result<String> {
    let exe = fs::read_link(format!("/proc/{}/exe", std::os::unix::process::parent_id()))
        .context("cannot read the launching browser's executable")?;
    let name: String = exe
        .file_name()
        .context("launching browser has no executable name")?
        .to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Ok(name)
}

/// Requests the extension sends its own native host (from the popup).
fn host_request(host: &Host, message: &Value) -> Result<Value> {
    match message["op"].as_str() {
        Some("status") => {}
        Some("set_primary") => {
            let mut state = load_state()?;
            let on = message["primary"]
                .as_bool()
                .context("primary must be a boolean")?;
            if on {
                state.primary = Some(host.name.clone());
            } else if state.primary.as_deref() == Some(host.name.as_str()) {
                state.primary = None;
            }
            save_state(&state)?;
        }
        _ => bail!("unknown native host operation"),
    }
    let connected: Vec<String> = connected()?.into_iter().map(|host| host.name).collect();
    Ok(json!({"browser": host.name, "primary": load_state()?.primary, "connected": connected}))
}

fn answer_host_request(stdout: &mut impl Write, host: &Host, message: &Value) -> Result<()> {
    let reply = match host_request(host, message) {
        Ok(result) => json!({"ref": message["ref"], "result": result}),
        Err(error) => json!({"ref": message["ref"], "error": format!("{error:#}")}),
    };
    write_frame(stdout, &reply)
}

pub fn native_host(browser: BrowserKind) -> Result<()> {
    let host = Host {
        kind: browser,
        name: parent_browser()?,
    };
    let path = host.file("sock");
    let parent = bridge_dir();
    fs::create_dir_all(&parent)?;
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(host.file("lock"))?;
    // Hold an exclusive lock before removing a stale socket. A second profile
    // of the same browser must never steal the active profile's connection.
    if !try_lock(&lock) {
        bail!("another profile of {} owns its hyprnav bridge", host.name);
    }
    if path.exists() {
        fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path)?;
    listener.set_nonblocking(true)?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        while let Ok(message) = read_frame(&mut stdin) {
            if tx.send(message).is_err() {
                break;
            }
        }
    });
    let mut stdout = std::io::stdout().lock();
    let mut serial = 0u64;
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_read_timeout(Some(TIMEOUT))?;
                stream.set_write_timeout(Some(TIMEOUT))?;
                let result = (|| -> Result<Value> {
                    let mut message = read_frame(&mut stream)?;
                    if !message.is_object() {
                        bail!("expected browser request object");
                    }
                    serial += 1;
                    message["id"] = json!(serial);
                    write_frame(&mut stdout, &message)?;
                    let deadline = Instant::now() + TIMEOUT;
                    loop {
                        let reply = rx
                            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                            .context("browser did not respond")?;
                        if reply.get("ref").is_some() {
                            answer_host_request(&mut stdout, &host, &reply)?;
                        } else if reply["id"] == serial {
                            return Ok(reply);
                        }
                    }
                })();
                let reply = result.unwrap_or_else(|error| json!({"error":format!("{error:#}")}));
                let _ = write_frame(&mut stream, &reply);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                match rx.recv_timeout(Duration::from_millis(25)) {
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Ok(message) if message.get("ref").is_some() => {
                        answer_host_request(&mut stdout, &host, &message)?
                    }
                    _ => {}
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    fs::remove_file(path)?;
    Ok(())
}

pub fn install(browser: BrowserKind, host_dir: Option<PathBuf>) -> Result<()> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is unset")?);
    let directory = host_dir.unwrap_or_else(|| match browser {
        BrowserKind::Firefox => home.join(".mozilla/native-messaging-hosts"),
        BrowserKind::Chromium => std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("chromium/NativeMessagingHosts"),
    });
    fs::create_dir_all(&directory)?;
    let launcher = directory.join("hyprnav-browser");
    // Follow the user's local wrapper so dev builds remain authoritative.
    let wrapper = home.join(".nix-profile/bin/hyprnav");
    let wrapper = if wrapper.is_file() {
        wrapper
    } else {
        PathBuf::from("/etc/profiles/per-user")
            .join(std::env::var("USER")?)
            .join("bin/hyprnav")
    };
    if !wrapper.is_file() {
        bail!("cannot locate local hyprnav wrapper");
    }
    fs::write(
        &launcher,
        format!(
            "#!/bin/sh\nexec {} tab native-host --browser {}\n",
            shell_escape::escape(wrapper.to_string_lossy()),
            browser.as_str()
        ),
    )?;
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700))?;
    let manifest = directory.join("hyprnav_browser.json");
    let mut registration = json!({
        "name":"hyprnav_browser", "description":"Hyprnav browser navigation", "path":launcher, "type":"stdio"
    });
    match browser {
        BrowserKind::Firefox => {
            registration["allowed_extensions"] = json!(["hyprnav@anoromi.local"])
        }
        BrowserKind::Chromium => {
            registration["allowed_origins"] = json!([format!(
                "chrome-extension://{}/",
                include_str!("../browser-extension/chromium-id").trim()
            )])
        }
    }
    // Atomically replace an existing manual file or Nix-managed symlink, never
    // follow a symlink into the immutable store when installing for development.
    let temporary = directory.join(format!(".hyprnav_browser.{}.json", std::process::id()));
    fs::write(&temporary, serde_json::to_vec_pretty(&registration)?)?;
    fs::rename(&temporary, &manifest)?;
    println!("Installed {}", manifest.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_targets_default_to_firefox_and_chromium_is_preserved() {
        let legacy: BrowserTarget =
            serde_json::from_value(json!({"name":"app","workspace":"work"})).unwrap();
        assert_eq!(legacy.browser, BrowserKind::Firefox);
        let chrome: BrowserTarget =
            serde_json::from_value(json!({"browser":"chromium","name":"app","workspace":"work"}))
                .unwrap();
        assert_eq!(serde_json::to_value(chrome).unwrap()["browser"], "chromium");
    }
    #[test]
    fn frames_round_trip_unicode_and_reject_oversized_input() {
        let value = json!({"workspace":"日本語 & ?=#"});
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &value).unwrap();
        assert_eq!(read_frame(&mut bytes.as_slice()).unwrap(), value);
        assert!(read_frame(&mut ((LIMIT + 1) as u32).to_ne_bytes().as_slice()).is_err());
        assert!(read_frame(&mut &bytes[..bytes.len() - 1]).is_err());
    }
    fn host(kind: BrowserKind, name: &str) -> Host {
        Host {
            kind,
            name: name.into(),
        }
    }
    #[test]
    fn primary_browser_wins_across_kinds_and_fallbacks_say_so() {
        let island = host(BrowserKind::Chromium, "Island");
        let chromium = host(BrowserKind::Chromium, "chromium");
        let zen = host(BrowserKind::Firefox, "zen");
        let all = [island.clone(), chromium.clone(), zen.clone()];
        let pick = |kind, hosts: &[Host], primary| choose(kind, hosts, primary).unwrap();

        assert_eq!(
            pick(BrowserKind::Chromium, &all, Some("Island")),
            (island.clone(), None)
        );
        assert_eq!(
            pick(BrowserKind::Chromium, &all, Some("zen")),
            (zen.clone(), None)
        );
        assert_eq!(
            pick(BrowserKind::Firefox, &all, Some("chromium")).0,
            chromium
        );

        // Primary not running: same kind first, and the note names the substitute.
        let (chosen, note) = pick(BrowserKind::Firefox, &all[..2], Some("zen"));
        assert_eq!(chosen, island);
        assert!(note.unwrap().contains("zen is not running; using Island"));
        let (chosen, note) = pick(BrowserKind::Firefox, &all[..], Some("gone"));
        assert_eq!(chosen, zen);
        assert!(note.is_some());

        // No primary: a lone browser serves silently; a choice among several is noted.
        assert_eq!(
            pick(BrowserKind::Chromium, &[zen.clone()], None),
            (zen.clone(), None)
        );
        assert!(pick(BrowserKind::Chromium, &all, None).1.is_some());
        assert!(choose(BrowserKind::Chromium, &[], Some("Island")).is_err());
    }
}
