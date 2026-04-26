use cxx_qt_lib::{QGuiApplication, QQmlApplicationEngine, QString, QUrl};
use hyprnav::cli::{
    parse_args, BatchArgs, ClientCommand, Command, EnvCommand, EnvTitleCommand, LockArgs,
    ResolveArgs, RunArgs, SlotAssignArgs, SlotClearArgs, SlotCommand, SlotCommandClearArgs,
    SlotCommandSetArgs, SlotLaunchCommand, SlotNameCommand, SpawnArgs, SpawnInternalArgs,
};
use hyprnav::controller::qobject::{
    hyprexpo_configure_root_window, hyprexpo_load_qml_from_module,
    hyprexpo_set_quit_on_last_window_closed,
};
use hyprnav::protocol::{
    send_request_with_fallbacks, BatchMutationPayload, Request, SlotAssignmentMode, SpawnPrepared,
    SpawnStarted, StatusSnapshot,
};
use hyprnav::runtime_paths::resolve_runtime_paths;
use hyprnav::server::run_server;
use hyprnav::spawn::{current_pid, exec_command};
use hyprnav::ui_session::{
    send_grid_open_command, send_grid_ping_command, send_switcher_step_command,
    start_grid_session_listener, start_switcher_session_listener,
};
use serde_json::Value;
use std::fs;
use std::io::Read;
use std::io::{self, Write};
use std::process::Command as ProcessCommand;
use std::process::ExitStatus;
use std::thread;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<()> {
    cxx_qt::init_crate!(cxx_qt);
    cxx_qt::init_crate!(cxx_qt_lib);
    cxx_qt::init_crate!(hyprnav);
    cxx_qt::init_qml_module!("com.anoromi.hyprnav");

    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .without_time()
        .try_init();

    let cli = parse_args();
    match cli.command.unwrap_or(Command::Daemon) {
        Command::Daemon => {
            if server_running() {
                return Ok(());
            }

            run_server()
        }
        Command::Trigger(args) => {
            ensure_server_running()?;
            if send_switcher_step_command(args.reverse)? {
                return Ok(());
            }
            run_ui("switcher", args.reverse, false)
        }
        Command::Grid => {
            ensure_server_running()?;
            ensure_grid_server_open()
        }
        Command::GridServer => {
            ensure_server_running()?;
            if send_grid_ping_command()? {
                return Ok(());
            }
            run_ui("grid", false, true)
        }
        Command::Status(args) => {
            ensure_server_running()?;
            let response: StatusSnapshot = send(Request::StatusGet { cwd: args.cwd })?;
            println!("{}", serde_json::to_string_pretty(&response)?);
            Ok(())
        }
        Command::Lock(LockArgs { env_id }) => {
            ensure_server_running()?;
            print_json(send::<Value>(Request::LockSet { env: env_id }))
        }
        Command::Unlock => {
            ensure_server_running()?;
            print_json(send::<Value>(Request::LockClear))
        }
        Command::Env(command) => {
            ensure_server_running()?;
            match command {
                EnvCommand::Ensure(args) => print_json(send::<Value>(Request::EnvEnsure {
                    env: args.env,
                    cwd: args.cwd,
                    client: args.client,
                    title: args.title,
                })),
                EnvCommand::Delete(args) => {
                    print_json(send::<Value>(Request::EnvDelete { env: args.env }))
                }
                EnvCommand::Title(command) => match command {
                    EnvTitleCommand::Set(args) => print_json(send::<Value>(Request::EnvTitleSet {
                        env: args.env,
                        title: args.title,
                    })),
                    EnvTitleCommand::Clear(args) => {
                        print_json(send::<Value>(Request::EnvTitleClear { env: args.env }))
                    }
                },
            }
        }
        Command::Client(command) => {
            ensure_server_running()?;
            match command {
                ClientCommand::Ensure(args) => print_json(send::<Value>(Request::ClientEnsure {
                    client: args.client,
                })),
            }
        }
        Command::Slot(command) => {
            ensure_server_running()?;
            match command {
                SlotCommand::Assign(args) => handle_slot_assign(args),
                SlotCommand::Clear(args) => handle_slot_clear(args),
                SlotCommand::Resolve(args) => handle_resolve(args),
                SlotCommand::Command(command) => match command {
                    SlotLaunchCommand::Set(args) => handle_slot_command_set(args),
                    SlotLaunchCommand::Clear(args) => handle_slot_command_clear(args),
                },
                SlotCommand::Name(command) => match command {
                    SlotNameCommand::Set(args) => print_json(send::<Value>(Request::SlotNameSet {
                        env: args.env,
                        slot: args.slot,
                        name: args.name,
                    })),
                    SlotNameCommand::Clear(args) => {
                        print_json(send::<Value>(Request::SlotNameClear {
                            env: args.env,
                            slot: args.slot,
                        }))
                    }
                },
            }
        }
        Command::Goto(args) => {
            ensure_server_running()?;
            print_json(send::<Value>(Request::WorkspaceGoto {
                env: args.env,
                slot: args.slot,
            }))
        }
        Command::Run(args) => {
            ensure_server_running()?;
            handle_run(args)
        }
        Command::Spawn(args) => {
            ensure_server_running()?;
            handle_spawn(args)
        }
        Command::Batch(args) => {
            ensure_server_running()?;
            handle_batch(args)
        }
        Command::SpawnInternal(args) => {
            ensure_server_running()?;
            handle_spawn_internal(args)
        }
    }
}

fn handle_slot_assign(args: SlotAssignArgs) -> anyhow::Result<()> {
    let assignment_mode_count = usize::from(args.workspace.is_some())
        + usize::from(args.managed)
        + usize::from(args.inherit);
    if assignment_mode_count != 1 {
        return Err(anyhow::anyhow!(
            "slot assign requires exactly one of --workspace, --managed, or --inherit"
        ));
    }

    if args.launch && args.command.is_empty() {
        return Err(anyhow::anyhow!(
            "slot assign --launch requires a command after --"
        ));
    }

    if !args.launch && !args.command.is_empty() {
        return Err(anyhow::anyhow!(
            "slot assign received trailing argv without --launch"
        ));
    }

    let assignment_mode = match (args.workspace, args.managed, args.inherit) {
        (Some(workspace_id), false, false) => SlotAssignmentMode::Fixed { workspace_id },
        (None, true, false) => SlotAssignmentMode::Managed,
        (None, false, true) => SlotAssignmentMode::Inherit,
        _ => unreachable!("validated assignment mode count"),
    };

    print_json(send::<Value>(Request::SlotAssign {
        env: args.env,
        slot: args.slot,
        assignment_mode,
        client: args.client,
        cwd: args.cwd,
        launch_argv: args.launch.then_some(args.command),
        display_name: args.name,
    }))
}

fn handle_slot_clear(args: SlotClearArgs) -> anyhow::Result<()> {
    print_json(send::<Value>(Request::SlotClear {
        env: args.env,
        slot: args.slot,
        client: args.client,
    }))
}

fn handle_resolve(args: ResolveArgs) -> anyhow::Result<()> {
    print_json(send::<Value>(Request::SlotResolve {
        env: args.env,
        slot: args.slot,
    }))
}

fn handle_slot_command_set(args: SlotCommandSetArgs) -> anyhow::Result<()> {
    print_json(send::<Value>(Request::SlotCommandSet {
        env: args.env,
        slot: args.slot,
        argv: args.command,
        display_name: args.name,
    }))
}

fn handle_slot_command_clear(args: SlotCommandClearArgs) -> anyhow::Result<()> {
    print_json(send::<Value>(Request::SlotCommandClear {
        env: args.env,
        slot: args.slot,
    }))
}

fn handle_run(args: RunArgs) -> anyhow::Result<()> {
    print_json(send::<Value>(Request::WorkspaceRun {
        env: args.env,
        slot: args.slot,
        argv: args.command,
    }))
}

fn handle_spawn(args: SpawnArgs) -> anyhow::Result<()> {
    if args.command.is_empty() {
        return Err(anyhow::anyhow!("spawn requires a command"));
    }

    let prepared: SpawnPrepared = send(Request::SpawnPrepare {
        target: args.workspace,
        focus_policy: if args.no_focus {
            "preserve".to_owned()
        } else {
            "follow".to_owned()
        },
    })?;

    if args.print_workspace_id {
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "{}", prepared.workspace_id)?;
        stdout.flush()?;
    }

    let current_exe = std::env::current_exe()?;
    let mut child = ProcessCommand::new(current_exe)
        .arg("spawn-internal")
        .arg("--operation-id")
        .arg(&prepared.operation_id)
        .arg("--")
        .args(&args.command)
        .spawn()?;

    let status = child.wait()?;
    let _ = send::<Value>(Request::SpawnFinish {
        operation_id: prepared.operation_id,
    });
    std::process::exit(exit_status_code(status));
}

fn handle_batch(args: BatchArgs) -> anyhow::Result<()> {
    let payload = read_batch_payload(args)?;
    if payload.operations.is_empty() {
        return Err(anyhow::anyhow!("batch requires at least one operation"));
    }
    print_json(send::<Value>(Request::BatchMutate {
        atomic: payload.atomic,
        operations: payload.operations,
    }))
}

fn handle_spawn_internal(args: SpawnInternalArgs) -> anyhow::Result<()> {
    if args.command.is_empty() {
        return Err(anyhow::anyhow!("spawn-internal requires a command"));
    }

    let _: SpawnStarted = send(Request::SpawnStart {
        operation_id: args.operation_id,
        root_pid: current_pid(),
    })?;
    exec_command(&args.command)?;
    Ok(())
}

fn read_batch_payload(args: BatchArgs) -> anyhow::Result<BatchMutationPayload> {
    match (args.file, args.stdin) {
        (Some(_), true) => Err(anyhow::anyhow!(
            "batch requires exactly one of --file or --stdin"
        )),
        (None, false) => Err(anyhow::anyhow!(
            "batch requires exactly one of --file or --stdin"
        )),
        (Some(path), false) => {
            let content = fs::read_to_string(&path)
                .map_err(|error| anyhow::anyhow!("reading batch payload from {path}: {error}"))?;
            serde_json::from_str(&content)
                .map_err(|error| anyhow::anyhow!("decoding batch payload from {path}: {error}"))
        }
        (None, true) => {
            let mut content = String::new();
            io::stdin().read_to_string(&mut content)?;
            serde_json::from_str(&content)
                .map_err(|error| anyhow::anyhow!("decoding batch payload from stdin: {error}"))
        }
    }
}

fn print_json(result: anyhow::Result<Value>) -> anyhow::Result<()> {
    let value = result?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn send<R: serde::de::DeserializeOwned>(request: Request) -> anyhow::Result<R> {
    let paths = resolve_runtime_paths();
    send_request_with_fallbacks(&paths.server_socket_path, &request)
}

fn server_running() -> bool {
    send::<Value>(Request::Ping).is_ok()
}

fn ensure_server_running() -> anyhow::Result<()> {
    if server_running() {
        return Ok(());
    }

    let current_exe = std::env::current_exe()?;
    ProcessCommand::new(current_exe).arg("daemon").spawn()?;

    for _ in 0..24 {
        thread::sleep(Duration::from_millis(150));
        if server_running() {
            return Ok(());
        }
    }

    Err(anyhow::anyhow!("timed out waiting for hyprnav daemon"))
}

fn ensure_grid_server_open() -> anyhow::Result<()> {
    if send_grid_open_command()? {
        return Ok(());
    }

    let current_exe = std::env::current_exe()?;
    ProcessCommand::new(current_exe)
        .arg("grid-server")
        .spawn()?;

    for _ in 0..40 {
        thread::sleep(Duration::from_millis(100));
        if send_grid_open_command()? {
            return Ok(());
        }
    }

    Err(anyhow::anyhow!("timed out waiting for hyprnav grid-server"))
}

fn run_ui(mode: &str, reverse: bool, resident: bool) -> anyhow::Result<()> {
    std::env::set_var("HYPREXPO_SWITCHER_UI_MODE", mode);
    std::env::set_var(
        "HYPREXPO_SWITCHER_UI_REVERSE",
        if reverse { "1" } else { "0" },
    );
    std::env::set_var(
        "HYPREXPO_SWITCHER_UI_RESIDENT",
        if resident { "1" } else { "0" },
    );

    let qml_type = if mode == "grid" {
        "EnvironmentGrid"
    } else {
        "Main"
    };
    let _switcher_session = if mode == "switcher" {
        Some(start_switcher_session_listener()?)
    } else {
        None
    };
    let _grid_session = if mode == "grid" && resident {
        Some(start_grid_session_listener()?)
    } else {
        None
    };

    let mut app = QGuiApplication::new();
    let mut engine = QQmlApplicationEngine::new();

    if let Some(app) = app.as_mut() {
        QGuiApplication::set_desktop_file_name(&QString::from("hyprnav"));
        hyprexpo_set_quit_on_last_window_closed(app, false);
    }

    if let Some(engine) = engine.as_mut() {
        if !hyprexpo_load_qml_from_module(
            engine,
            &QString::from("com.anoromi.hyprnav"),
            &QString::from(qml_type),
        ) {
            return Err(anyhow::anyhow!("failed to load {qml_type} from QML module"));
        }
    }

    if let Some(engine) = engine.as_mut() {
        if !hyprexpo_configure_root_window(engine) {
            return Err(anyhow::anyhow!("failed to configure switcher root window"));
        }
    }

    if let Some(app) = app.as_mut() {
        app.exec();
    }

    Ok(())
}

fn exit_status_code(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(code) = status.code() {
            return code;
        }
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
        1
    }

    #[cfg(not(unix))]
    {
        status.code().unwrap_or(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyprnav::protocol::BatchMutationRequest;
    use std::env;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_file(label: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!("hyprnav-main-{label}-{unique}.json"))
    }

    #[test]
    fn read_batch_payload_rejects_missing_source() {
        let error = read_batch_payload(BatchArgs {
            file: None,
            stdin: false,
        })
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("exactly one of --file or --stdin"));
    }

    #[test]
    fn read_batch_payload_rejects_multiple_sources() {
        let error = read_batch_payload(BatchArgs {
            file: Some("/tmp/payload.json".to_owned()),
            stdin: true,
        })
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("exactly one of --file or --stdin"));
    }

    #[test]
    fn read_batch_payload_reads_from_file() {
        let path = unique_temp_file("batch-file");
        fs::write(
            &path,
            r#"{"atomic":true,"operations":[{"op":"lock_clear"}]}"#,
        )
        .unwrap();

        let payload = read_batch_payload(BatchArgs {
            file: Some(path.to_string_lossy().into_owned()),
            stdin: false,
        })
        .unwrap();

        assert!(payload.atomic);
        assert_eq!(payload.operations.len(), 1);
        assert!(matches!(
            payload.operations[0],
            BatchMutationRequest::LockClear
        ));

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn read_batch_payload_reports_malformed_json() {
        let path = unique_temp_file("batch-bad-json");
        fs::write(&path, "{not-json").unwrap();

        let error = read_batch_payload(BatchArgs {
            file: Some(path.to_string_lossy().into_owned()),
            stdin: false,
        })
        .unwrap_err();

        assert!(error.to_string().contains("decoding batch payload"));
        fs::remove_file(path).unwrap();
    }
}
