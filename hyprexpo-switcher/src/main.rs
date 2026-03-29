use hyprexpo_switcher::cli::{parse_args, trigger_command, Mode};
use hyprexpo_switcher::controller::qobject::{
    hyprexpo_configure_root_window, hyprexpo_set_quit_on_last_window_closed,
};
use hyprexpo_switcher::runtime_paths::{fallback_switcher_socket_paths, resolve_runtime_paths, runtime_root};
use cxx_qt_lib::{QGuiApplication, QQmlApplicationEngine, QString, QUrl};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::thread;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<()> {
    cxx_qt::init_crate!(cxx_qt);
    cxx_qt::init_crate!(cxx_qt_lib);
    cxx_qt::init_crate!(hyprexpo_switcher);
    cxx_qt::init_qml_module!("com.anoromi.hyprexpo.switcher");

    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .without_time()
        .try_init();

    match parse_args(std::env::args()) {
        Mode::Trigger { reverse } => std::process::exit(run_trigger(reverse)),
        Mode::Daemon => {}
    }

    if daemon_already_running() {
        return Ok(());
    }

    std::env::set_var("QML_DISABLE_DISK_CACHE", "1");

    let mut app = QGuiApplication::new();
    let mut engine = QQmlApplicationEngine::new();

    if let Some(app) = app.as_mut() {
        QGuiApplication::set_desktop_file_name(&QString::from("hyprexpo-switcher"));
        hyprexpo_set_quit_on_last_window_closed(app, false);
    }

    if let Some(engine) = engine.as_mut() {
        engine.load(&QUrl::from(
            "qrc:/qt/qml/com/anoromi/hyprexpo/switcher/qml/Main.qml",
        ));
    }

    if let Some(engine) = engine.as_mut() {
        let _ = hyprexpo_configure_root_window(engine);
    }

    if let Some(app) = app.as_mut() {
        app.exec();
    }

    Ok(())
}

fn daemon_already_running() -> bool {
    send_command_with_fallbacks("PING\n", true)
}

fn run_trigger(reverse: bool) -> i32 {
    let command = trigger_command(reverse);
    if send_command_with_fallbacks(command, false) {
        return 0;
    }

    let current_exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(_) => return 1,
    };

    if Command::new(current_exe).arg("daemon").spawn().is_err() {
        return 1;
    }

    for _ in 0..12 {
        thread::sleep(Duration::from_millis(150));
        if send_command_with_fallbacks(command, false) {
            return 0;
        }
    }

    1
}

fn send_command_with_fallbacks(command: &str, wait_for_response: bool) -> bool {
    let paths = resolve_runtime_paths();
    if send_command_to_socket(&paths.switcher_socket_path, command, wait_for_response) {
        return true;
    }

    for path in fallback_switcher_socket_paths(&runtime_root()) {
        if path == paths.switcher_socket_path {
            continue;
        }

        if send_command_to_socket(&path, command, wait_for_response) {
            return true;
        }
    }

    false
}

fn send_command_to_socket(path: &std::path::Path, command: &str, wait_for_response: bool) -> bool {
    if path.as_os_str().is_empty() {
        return false;
    }

    let mut stream = match UnixStream::connect(path) {
        Ok(stream) => stream,
        Err(_) => return false,
    };

    if stream.write_all(command.as_bytes()).is_err() || stream.flush().is_err() {
        return false;
    }

    if !wait_for_response {
        return true;
    }

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    if reader.read_line(&mut response).is_err() {
        return false;
    }

    !response.starts_with("ERROR")
}
