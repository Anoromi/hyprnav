use cxx_qt_lib::{QGuiApplication, QQmlApplicationEngine, QString};
use hyprnav::cli::{
    parse_args, BatchArgs, ClientCommand, Command, EnvCommand, EnvTitleCommand, LockArgs,
    ResolveArgs, RunArgs, SlotAssignArgs, SlotClearArgs, SlotCommand, SlotCommandClearArgs,
    SlotTempArgs,
    SlotCommandSetArgs, SlotLaunchCommand, SlotNameCommand, SpawnArgs, SpawnInternalArgs,
};
use hyprnav::controller::qobject::{
    hyprnav_configure_root_window, hyprnav_load_qml_from_module,
    hyprnav_set_quit_on_last_window_closed,
};
use hyprnav::protocol::{
    send_request, BatchMutationPayload, Request, SlotAssignmentMode, SpawnPrepared, SpawnStarted,
    StatusSnapshot,
};
use hyprnav::runtime_paths::{append_switch_log, resolve_runtime_paths};
use hyprnav::server::run_server;
use hyprnav::spawn::{current_pid, exec_command};
use hyprnav::ui_session::{
    send_grid_open_command, send_grid_ping_command, send_switcher_activate_command,
    send_switcher_cancel_command, send_switcher_ping_command, send_switcher_step_command,
    start_grid_session_listener, start_switcher_session_listener,
};
use anyhow::Context;
use serde_json::Value;
use std::fs;
use std::io::Read;
use std::io::{self, Write};
use std::process::Command as ProcessCommand;
use std::process::ExitStatus;
use std::thread;
use std::time::Duration;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<()> {
    cxx_qt::init_crate!(cxx_qt);
    cxx_qt::init_crate!(cxx_qt_lib);
    cxx_qt::init_crate!(hyprnav);
    cxx_qt::init_qml_module!("com.anoromi.hyprnav");

    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .without_time()
        .try_init();

    let cli = parse_args();
    match cli.command.unwrap_or(Command::Daemon) {
        Command::Tab(command) => {
            use hyprnav::browser;
            use hyprnav::cli::TabCommand;
            use serde_json::json;
            match command {
                TabCommand::Install { browser, host_dir } => browser::install(browser, host_dir),
                TabCommand::NativeHost { browser } => browser::native_host(browser),
                TabCommand::List { browser } => {
                    print_json(browser::request(browser, json!({"op":"list"})))
                }
                TabCommand::Open {
                    browser,
                    name,
                    url,
                    param,
                } => print_json(browser::request(
                    browser,
                    json!({"op":"open", "name":name, "url":url, "param":param}),
                )),
                TabCommand::Goto {
                    browser,
                    name,
                    workspace,
                } => print_json(browser::navigate(&browser::BrowserTarget {
                    browser,
                    name,
                    workspace,
                })),
                TabCommand::Assign {
                    browser,
                    env,
                    slot,
                    name,
                    workspace,
                } => {
                    ensure_server_running()?;
                    print_json(send::<Value>(Request::BrowserSlotSet {
                        env,
                        slot,
                        target: browser::BrowserTarget {
                            browser,
                            name,
                            workspace,
                        },
                    }))
                }
                TabCommand::Clear(args) => {
                    ensure_server_running()?;
                    print_json(send::<Value>(Request::BrowserSlotClear {
                        env: args.env,
                        slot: args.slot,
                    }))
                }
            }
        }
        Command::Daemon => {
            info!("hyprnav command entry: daemon");
            append_switch_log("cli.command", "name=daemon");
            if server_running() {
                return Ok(());
            }

            run_server()
        }
        Command::Trigger(args) => {
            info!(reverse = args.reverse, "hyprnav command entry: trigger");
            append_switch_log(
                "cli.command",
                format!("name=trigger reverse={}", args.reverse),
            );
            ensure_server_running()?;
            ensure_switcher_server_open(args.reverse)
        }
        Command::Switcher(command) => {
            ensure_server_running()?;
            match command {
                hyprnav::cli::SwitcherCommand::Activate => {
                    info!("hyprnav command entry: switcher activate");
                    append_switch_log("cli.command", "name=switcher.activate");
                    let _ =
                        send_switcher_command_with_startup_grace(send_switcher_activate_command)?;
                }
                hyprnav::cli::SwitcherCommand::Cancel => {
                    info!("hyprnav command entry: switcher cancel");
                    append_switch_log("cli.command", "name=switcher.cancel");
                    let _ = send_switcher_command_with_startup_grace(send_switcher_cancel_command)?;
                }
            }
            Ok(())
        }
        Command::Grid => {
            info!("hyprnav command entry: grid");
            append_switch_log("cli.command", "name=grid");
            ensure_server_running()?;
            ensure_grid_server_open()
        }
        Command::SwitcherServer => {
            info!("hyprnav command entry: switcher-server");
            append_switch_log("cli.command", "name=switcher-server");
            ensure_server_running()?;
            if send_switcher_ping_command()? {
                return Ok(());
            }
            run_ui("switcher", false, true)
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
            print_json(send::<Value>(Request::LockSet {
                env: env_id,
                origin: cli.origin,
            }))
        }
        Command::Unlock => {
            ensure_server_running()?;
            print_json(send::<Value>(Request::LockClear { origin: cli.origin }))
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
                SlotCommand::Temp(args) => handle_slot_temp(args),
                SlotCommand::Remove(args) => print_json(send::<Value>(Request::SlotRemove {
                    env: args.env,
                    slot: args.slot,
                    name: args.name,
                })),
                SlotCommand::Temps => print_json(send::<Value>(Request::SlotTempList)),
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
                origin: cli.origin,
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
        Command::Agents => {
            ensure_server_running()?;
            print_json(send::<Value>(Request::AgentsList))
        }
        Command::Events(args) => {
            ensure_server_running()?;
            stream_events(args.once)
        }
        Command::Frames(args) => {
            ensure_server_running()?;
            let paths = resolve_runtime_paths();
            let address = hyprnav::frames::normalize_address(&args.address)
                .ok_or_else(|| anyhow::anyhow!("address must look like 0x1234"))?;
            let codec = hyprnav::video::Codec::parse(&args.codec)
                .ok_or_else(|| anyhow::anyhow!("unknown codec {}", args.codec))?;
            let request = hyprnav::frames::ClientRequest {
                address,
                codecs: vec![codec],
                stream: hyprnav::frames::StreamRequest {
                    fps: args
                        .fps
                        .clamp(hyprnav::frames::MIN_FPS, hyprnav::frames::MAX_FPS),
                    quality: args
                        .quality
                        .clamp(hyprnav::frames::MIN_QUALITY, hyprnav::frames::MAX_QUALITY),
                    max_width: args
                        .max_width
                        .clamp(hyprnav::frames::MIN_WIDTH, hyprnav::frames::MAX_WIDTH),
                },
                follow_transient: args.follow.as_deref() == Some("transient"),
            };
            let result = if args.ivf {
                // IVF needs to seek back and patch its header, so it needs a file.
                let path = args.output.clone().ok_or_else(|| {
                    anyhow::anyhow!("--ivf writes a seekable file; pass -o PATH")
                })?;
                let mut file = std::fs::File::create(&path)
                    .with_context(|| format!("creating {}", path.display()))?;
                hyprnav::frames::stream_to_ivf(&paths.frames_socket_path, &request, &mut file)
            } else if let Some(path) = args.output.clone() {
                let mut file = std::fs::File::create(&path)
                    .with_context(|| format!("creating {}", path.display()))?;
                hyprnav::frames::stream_frames(&paths.frames_socket_path, &request, &mut file)
            } else {
                let mut stdout = io::stdout().lock();
                hyprnav::frames::stream_frames(&paths.frames_socket_path, &request, &mut stdout)
            };
            match result {
                // A closed pipe (`| head`) or a Ctrl-C is a normal way to stop.
                Err(error) => match error.downcast_ref::<std::io::Error>() {
                    Some(io_error) if io_error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
                    _ => Err(error),
                },
                ok => ok,
            }
        }
        Command::Agent(command) => {
            use hyprnav::cli::AgentCommand;
            ensure_server_running()?;
            match command {
                AgentCommand::Register(args) => print_json(send::<Value>(Request::AgentRegister {
                    agent_id: args.id,
                    label: args.label,
                    client: args.client,
                    pid: args.pid.unwrap_or_else(|| unsafe { libc::getppid() } as u32),
                    cwd: args.cwd.or_else(|| std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned())),
                    env: args.env,
                    thread_id: args.thread_id,
                    thread_environment_id: args.thread_environment_id,
                })),
                AgentCommand::Beat(args) => print_json(send::<Value>(Request::AgentBeat {
                    agent_id: args.id,
                    state: args.state,
                    target: args.target,
                    action: args.action,
                })),
                AgentCommand::Label(args) => print_json(send::<Value>(Request::AgentLabel {
                    agent_id: args.id,
                    label: args.label,
                })),
                AgentCommand::Finish(args) => print_json(send::<Value>(Request::AgentFinish {
                    agent_id: args.id,
                })),
            }
        }
        Command::Screencast(command) => {
            use hyprnav::cli::ScreencastCommand;
            use hyprnav::runtime_paths::{runtime_root, screencast_request_path};
            let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").unwrap_or_default();
            let path = screencast_request_path(&runtime_root(), &signature);
            match command {
                ScreencastCommand::Request(args) => {
                    let address = args.address.trim().trim_start_matches("address:").to_owned();
                    if !address.starts_with("0x") {
                        return Err(anyhow::anyhow!("address must look like 0x1234"));
                    }
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&path, format!("{address}\n"))?;
                    print_json(Ok(serde_json::json!({"path": path, "address": address})))
                }
                ScreencastCommand::Clear => {
                    let existed = std::fs::remove_file(&path).is_ok();
                    print_json(Ok(serde_json::json!({"path": path, "removed": existed})))
                }
            }
        }
        Command::Stick(command) => {
            use hyprnav::cli::StickCommand;
            ensure_server_running()?;
            match command {
                StickCommand::List => print_json(send::<Value>(Request::StickList)),
                StickCommand::Release(args) => print_json(send::<Value>(Request::StickRelease {
                    stick_id: args.stick_id,
                })),
                StickCommand::Add(args) => print_json(send::<Value>(Request::StickAdd {
                    workspace_id: args.workspace,
                    pid: args.pid,
                })),
                StickCommand::Move(args) => print_json(send::<Value>(Request::StickMove {
                    stick_id: args.stick_id,
                    workspace_id: args.workspace,
                })),
            }
        }
        Command::Batch(args) => {
            ensure_server_running()?;
            handle_batch(args, cli.origin)
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

fn handle_slot_temp(args: SlotTempArgs) -> anyhow::Result<()> {
    let created: Value = send(Request::SlotTempCreate {
        env: args.env,
        cwd: args.cwd,
        name: args.name,
        owner: Some(args.owner.unwrap_or_else(|| "cli".to_owned())),
        client: None,
        launch_argv: if args.command.is_empty() {
            None
        } else {
            Some(args.command.clone())
        },
    })?;
    if args.command.is_empty() {
        println!("{}", serde_json::to_string_pretty(&created)?);
        return Ok(());
    }
    let workspace = created
        .get("physical_workspace_id")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow::anyhow!("daemon did not return a workspace for the new slot"))?;
    eprintln!("{}", serde_json::to_string(&created)?);
    handle_spawn(SpawnArgs {
        no_focus: args.no_focus,
        no_stick: false,
        print_workspace_id: false,
        workspace: workspace.to_string(),
        command: args.command,
    })
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
        no_stick: args.no_stick,
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

fn handle_batch(args: BatchArgs, origin: Option<String>) -> anyhow::Result<()> {
    let payload = read_batch_payload(args)?;
    if payload.operations.is_empty() {
        return Err(anyhow::anyhow!("batch requires at least one operation"));
    }
    print_json(send::<Value>(Request::BatchMutate {
        atomic: payload.atomic,
        operations: payload.operations,
        origin,
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

/// Print the daemon's event stream, one JSON object per line.
///
/// `--once` stops after the connect burst (hello, agents, slots); otherwise it
/// runs until the daemon goes away or the terminal interrupts it.
fn stream_events(once: bool) -> anyhow::Result<()> {
    use std::io::BufRead;
    let paths = resolve_runtime_paths();
    let stream = std::os::unix::net::UnixStream::connect(&paths.events_socket_path)
        .map_err(|error| {
            anyhow::anyhow!(
                "connecting to {}: {error}",
                paths.events_socket_path.display()
            )
        })?;
    let mut reader = std::io::BufReader::new(stream);
    let mut stdout = io::stdout();
    let mut seen = 0usize;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            // `--once` past the third line: an older daemon sends no `locked`.
            Err(_) if once && seen >= 3 => break,
            Err(error) => return Err(error.into()),
        }
        if line.trim().is_empty() {
            continue;
        }
        write!(stdout, "{line}")?;
        stdout.flush()?;
        seen += 1;
        if once && seen >= 4 {
            break;
        }
        if once && seen == 3 {
            reader
                .get_ref()
                .set_read_timeout(Some(Duration::from_millis(300)))?;
        }
    }
    Ok(())
}

fn print_json(result: anyhow::Result<Value>) -> anyhow::Result<()> {
    let value = result?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn send<R: serde::de::DeserializeOwned>(request: Request) -> anyhow::Result<R> {
    let paths = resolve_runtime_paths();
    send_request(&paths.server_socket_path, &request)
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

fn ensure_switcher_server_open(reverse: bool) -> anyhow::Result<()> {
    let initial_sent = send_switcher_step_command(reverse)?;
    debug!(reverse, sent = initial_sent, "switcher initial socket step");
    append_switch_log(
        "cli.switcher.initial_step",
        format!("reverse={reverse} sent={initial_sent}"),
    );
    if initial_sent {
        return Ok(());
    }

    let current_exe = std::env::current_exe()?;
    info!(reverse, "spawning hyprnav switcher-server");
    append_switch_log("cli.switcher.spawn", format!("reverse={reverse}"));
    ProcessCommand::new(current_exe)
        .arg("switcher-server")
        .spawn()?;

    for attempt in 0..40 {
        thread::sleep(Duration::from_millis(25));
        if send_switcher_step_command(reverse)? {
            debug!(reverse, attempt = attempt + 1, "switcher became available");
            append_switch_log(
                "cli.switcher.retry_success",
                format!("reverse={reverse} attempt={}", attempt + 1),
            );
            return Ok(());
        }
    }

    warn!(reverse, "timed out waiting for hyprnav switcher-server");
    append_switch_log(
        "cli.switcher.timeout",
        format!("reverse={reverse} attempts=40"),
    );
    Err(anyhow::anyhow!(
        "timed out waiting for hyprnav switcher-server"
    ))
}

fn send_switcher_command_with_startup_grace(
    send_command: fn() -> anyhow::Result<bool>,
) -> anyhow::Result<bool> {
    for attempt in 0..10 {
        if send_command()? {
            debug!(attempt = attempt + 1, "switcher command delivered");
            append_switch_log(
                "cli.switcher.command_success",
                format!("attempt={}", attempt + 1),
            );
            return Ok(true);
        }

        if attempt < 9 {
            debug!(attempt = attempt + 1, "switcher command delivery retry");
            append_switch_log(
                "cli.switcher.command_retry",
                format!("attempt={}", attempt + 1),
            );
            thread::sleep(Duration::from_millis(25));
        }
    }

    warn!("switcher command was not delivered after startup grace");
    append_switch_log("cli.switcher.command_failed", "attempts=10");
    Ok(false)
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
        hyprnav_set_quit_on_last_window_closed(app, false);
    }

    if let Some(engine) = engine.as_mut() {
        if !hyprnav_load_qml_from_module(
            engine,
            &QString::from("com.anoromi.hyprnav"),
            &QString::from(qml_type),
        ) {
            return Err(anyhow::anyhow!("failed to load {qml_type} from QML module"));
        }
    }

    if let Some(engine) = engine.as_mut() {
        if !hyprnav_configure_root_window(engine) {
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
