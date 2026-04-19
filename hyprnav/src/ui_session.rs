use crate::runtime_paths::{
    ensure_parent_dir, fallback_switcher_socket_paths, resolve_runtime_paths,
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

static SWITCHER_COMMAND_RX: OnceLock<Mutex<Receiver<UiSessionCommand>>> = OnceLock::new();

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

fn send_command_to_socket(path: &Path, command: UiSessionCommand) -> Result<()> {
    let mut stream =
        UnixStream::connect(path).with_context(|| format!("connecting to {}", path.display()))?;
    let line = match command {
        UiSessionCommand::StepForward => "STEP FORWARD\n",
        UiSessionCommand::StepReverse => "STEP REVERSE\n",
        UiSessionCommand::Activate => "ACTIVATE\n",
        UiSessionCommand::Cancel => "CANCEL\n",
    };
    stream.write_all(line.as_bytes())?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response)?;
    if response.trim() == "OK" {
        Ok(())
    } else {
        Err(anyhow::anyhow!("switcher session rejected command"))
    }
}
