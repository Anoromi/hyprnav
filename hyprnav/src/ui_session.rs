use crate::runtime_paths::{
    ensure_parent_dir, fallback_grid_socket_paths, fallback_switcher_socket_paths,
    resolve_runtime_paths,
};
use anyhow::{Context, Result};
use crossbeam_channel::{unbounded, Receiver};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UiSessionCommand {
    StepForward,
    StepReverse,
    Activate,
    Cancel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GridUiCommand {
    Open,
    Close,
    Refresh,
}

static SWITCHER_COMMAND_RX: OnceLock<Mutex<Receiver<UiSessionCommand>>> = OnceLock::new();
static GRID_COMMAND_RX: OnceLock<Mutex<Receiver<GridUiCommand>>> = OnceLock::new();

pub struct UiSessionHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    socket_path: PathBuf,
}

impl Drop for UiSessionHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = UnixStream::connect(&self.socket_path);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = fs::remove_file(&self.socket_path);
    }
}

pub fn start_switcher_session_listener() -> Result<UiSessionHandle> {
    let paths = resolve_runtime_paths();
    start_switcher_session_listener_at(&paths.switcher_socket_path)
}

pub fn drain_switcher_session_commands() -> Vec<UiSessionCommand> {
    let Some(receiver) = SWITCHER_COMMAND_RX.get() else {
        return Vec::new();
    };

    let Ok(receiver) = receiver.lock() else {
        return Vec::new();
    };

    let mut commands = Vec::new();
    while let Ok(command) = receiver.try_recv() {
        commands.push(command);
    }

    commands
}

pub fn start_grid_session_listener() -> Result<UiSessionHandle> {
    let paths = resolve_runtime_paths();
    start_grid_session_listener_at(&paths.grid_socket_path)
}

pub fn drain_grid_session_commands() -> Vec<GridUiCommand> {
    let Some(receiver) = GRID_COMMAND_RX.get() else {
        return Vec::new();
    };

    let Ok(receiver) = receiver.lock() else {
        return Vec::new();
    };

    let mut commands = Vec::new();
    while let Ok(command) = receiver.try_recv() {
        commands.push(command);
    }

    commands
}

pub fn send_switcher_step_command(reverse: bool) -> Result<bool> {
    let paths = resolve_runtime_paths();
    let mut candidates = vec![paths.switcher_socket_path];
    for candidate in fallback_switcher_socket_paths(&paths.runtime_root) {
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }

    for candidate in candidates {
        match send_command_to_socket(
            &candidate,
            if reverse {
                UiSessionCommand::StepReverse
            } else {
                UiSessionCommand::StepForward
            },
        ) {
            Ok(()) => return Ok(true),
            Err(_) => continue,
        }
    }

    Ok(false)
}

pub fn send_grid_open_command() -> Result<bool> {
    send_grid_command(GridUiCommand::Open)
}

pub fn send_grid_close_command() -> Result<bool> {
    send_grid_command(GridUiCommand::Close)
}

pub fn send_grid_refresh_command() -> Result<bool> {
    send_grid_command(GridUiCommand::Refresh)
}

pub fn send_grid_ping_command() -> Result<bool> {
    let paths = resolve_runtime_paths();
    let mut candidates = vec![paths.grid_socket_path];
    for candidate in fallback_grid_socket_paths(&paths.runtime_root) {
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }

    for candidate in candidates {
        match send_command_line_to_socket(&candidate, "PING\n") {
            Ok(()) => return Ok(true),
            Err(_) => continue,
        }
    }

    Ok(false)
}

fn start_switcher_session_listener_at(socket_path: &Path) -> Result<UiSessionHandle> {
    ensure_parent_dir(socket_path)?;
    let _ = fs::remove_file(socket_path);

    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("binding {}", socket_path.display()))?;
    listener.set_nonblocking(true)?;

    let (sender, receiver) = unbounded();
    let _ = SWITCHER_COMMAND_RX.set(Mutex::new(receiver));

    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let socket_path = socket_path.to_path_buf();
    let thread = thread::spawn(move || {
        while !stop_thread.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = handle_session_client(stream, &sender);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(25));
                }
                Err(_) => {
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
    });

    Ok(UiSessionHandle {
        stop,
        thread: Some(thread),
        socket_path,
    })
}

fn start_grid_session_listener_at(socket_path: &Path) -> Result<UiSessionHandle> {
    ensure_parent_dir(socket_path)?;
    let _ = fs::remove_file(socket_path);

    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("binding {}", socket_path.display()))?;
    listener.set_nonblocking(true)?;

    let (sender, receiver) = unbounded();
    let _ = GRID_COMMAND_RX.set(Mutex::new(receiver));

    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let socket_path = socket_path.to_path_buf();
    let thread = thread::spawn(move || {
        while !stop_thread.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = handle_grid_session_client(stream, &sender);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(25));
                }
                Err(_) => {
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
    });

    Ok(UiSessionHandle {
        stop,
        thread: Some(thread),
        socket_path,
    })
}

fn handle_session_client(
    mut stream: UnixStream,
    sender: &crossbeam_channel::Sender<UiSessionCommand>,
) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let command = match line.trim() {
        "STEP FORWARD" => Some(UiSessionCommand::StepForward),
        "STEP REVERSE" => Some(UiSessionCommand::StepReverse),
        "ACTIVATE" => Some(UiSessionCommand::Activate),
        "CANCEL" => Some(UiSessionCommand::Cancel),
        "PING" => None,
        other => {
            stream.write_all(format!("ERROR unknown command: {other}\n").as_bytes())?;
            stream.flush()?;
            return Ok(());
        }
    };

    if let Some(command) = command {
        let _ = sender.send(command);
    }

    stream.write_all(b"OK\n")?;
    stream.flush()?;
    Ok(())
}

fn handle_grid_session_client(
    mut stream: UnixStream,
    sender: &crossbeam_channel::Sender<GridUiCommand>,
) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let command = match line.trim() {
        "OPEN" => Some(GridUiCommand::Open),
        "CLOSE" => Some(GridUiCommand::Close),
        "REFRESH" => Some(GridUiCommand::Refresh),
        "PING" => None,
        other => {
            stream.write_all(format!("ERROR unknown command: {other}\n").as_bytes())?;
            stream.flush()?;
            return Ok(());
        }
    };

    if let Some(command) = command {
        let _ = sender.send(command);
    }

    stream.write_all(b"OK\n")?;
    stream.flush()?;
    Ok(())
}

fn send_command_to_socket(path: &Path, command: UiSessionCommand) -> Result<()> {
    let line = match command {
        UiSessionCommand::StepForward => "STEP FORWARD\n",
        UiSessionCommand::StepReverse => "STEP REVERSE\n",
        UiSessionCommand::Activate => "ACTIVATE\n",
        UiSessionCommand::Cancel => "CANCEL\n",
    };
    send_command_line_to_socket(path, line)
}

fn send_grid_command(command: GridUiCommand) -> Result<bool> {
    let paths = resolve_runtime_paths();
    let mut candidates = vec![paths.grid_socket_path];
    for candidate in fallback_grid_socket_paths(&paths.runtime_root) {
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }

    let line = match command {
        GridUiCommand::Open => "OPEN\n",
        GridUiCommand::Close => "CLOSE\n",
        GridUiCommand::Refresh => "REFRESH\n",
    };

    for candidate in candidates {
        match send_command_line_to_socket(&candidate, line) {
            Ok(()) => return Ok(true),
            Err(_) => continue,
        }
    }

    Ok(false)
}

fn send_command_line_to_socket(path: &Path, line: &str) -> Result<()> {
    let mut stream =
        UnixStream::connect(path).with_context(|| format!("connecting to {}", path.display()))?;
    stream.write_all(line.as_bytes())?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response)?;
    if response.trim() == "OK" {
        Ok(())
    } else {
        Err(anyhow::anyhow!("session rejected command"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_socket_path(label: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "hyprnav-ui-session-{label}-{}.sock",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn grid_session_listener_accepts_open_refresh_and_close_commands() {
        let path = test_socket_path("grid");
        let listener = start_grid_session_listener_at(&path).expect("listener");

        send_command_line_to_socket(&path, "OPEN\n").expect("open");
        send_command_line_to_socket(&path, "REFRESH\n").expect("refresh");
        send_command_line_to_socket(&path, "CLOSE\n").expect("close");
        send_command_line_to_socket(&path, "PING\n").expect("ping");

        thread::sleep(Duration::from_millis(75));

        assert_eq!(
            drain_grid_session_commands(),
            vec![
                GridUiCommand::Open,
                GridUiCommand::Refresh,
                GridUiCommand::Close
            ]
        );

        drop(listener);
        let _ = fs::remove_file(path);
    }
}
