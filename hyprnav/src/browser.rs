//! Local Firefox/Chromium native-messaging transport. No listening TCP port.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
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

fn socket_path(browser: BrowserKind) -> PathBuf {
    // Keep the existing Firefox path for running hosts and saved slots.
    let name = match browser {
        BrowserKind::Firefox => "control.sock",
        BrowserKind::Chromium => "chromium.sock",
    };
    crate::runtime_paths::runtime_root()
        .join("hyprnav-browser")
        .join(name)
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

pub fn request(browser: BrowserKind, value: Value) -> Result<Value> {
    let mut stream = UnixStream::connect(socket_path(browser)).context(
        "browser bridge unavailable; run `hyprnav tab install` and load the matching browser extension",
    )?;
    stream.set_read_timeout(Some(TIMEOUT + Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    write_frame(&mut stream, &value)?;
    let response = read_frame(&mut stream)?;
    if let Some(error) = response.get("error").and_then(Value::as_str) {
        bail!("{error}");
    }
    Ok(response["result"].clone())
}

pub fn navigate(target: &BrowserTarget) -> Result<Value> {
    request(
        target.browser,
        json!({"op":"goto", "name":target.name, "workspace":target.workspace}),
    )
}

pub fn native_host(browser: BrowserKind) -> Result<()> {
    let path = socket_path(browser);
    let parent = path.parent().unwrap();
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(parent.join(match browser {
            BrowserKind::Firefox => "lock",
            BrowserKind::Chromium => "chromium.lock",
        }))?;
    // Hold an exclusive lock before removing a stale socket. A second browser
    // profile must never steal the active profile's connection.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("another profile of this browser family owns the hyprnav bridge");
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
                        if reply["id"] == serial {
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
        assert_ne!(
            socket_path(BrowserKind::Firefox),
            socket_path(BrowserKind::Chromium)
        );
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
}
