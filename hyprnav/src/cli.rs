use clap::{Args, Parser, Subcommand};

const TOP_LEVEL_ABOUT: &str =
    "Workspace environment server, overlay switcher, and workspace-spawn helper for Hyprland.";

const TOP_LEVEL_LONG_ABOUT: &str = "\
hyprnav has three main roles:

  1. daemon
     Runs the persistent headless server. It stores environment mappings, lock
     state, preview metadata, and runtime spawn operations.

  2. UI clients
     - trigger: MRU workspace overlay
     - grid: environment/slot browser

  3. CLI control surface
     Environment and slot commands let you map virtual slots like 1, 2, 3 to
     physical Hyprland workspaces like 5, 101, 102.

Environment resolution rules:

  - Commands that create or assign state can infer an environment from --env,
    --cwd, or the current working directory.
  - Commands that operate on an existing mapping (slot clear, slot resolve,
    goto, run) require either:
      * --env <id>
      * or a global lock set with `hyprnav lock <env-id>`

`spawn` is separate from environment slots. It targets a raw physical workspace
ID or a temporary high-ID workspace allocated with `rand`.
";

const TOP_LEVEL_AFTER_HELP: &str = "\
Examples:
  hyprnav daemon
  hyprnav trigger
  hyprnav trigger --reverse
  hyprnav grid
  hyprnav env ensure --env demo
  hyprnav slot assign --env demo --slot 1 --workspace 5
  hyprnav slot assign --env demo --slot 2 --managed
  hyprnav slot assign --env demo.child --slot 2 --inherit
  hyprnav slot assign --env demo --slot 3 --managed --launch -- ghostty
  hyprnav slot command set --env demo --slot 1 -- ghostty --class work
  hyprnav lock demo
  hyprnav goto --slot 2
  hyprnav run --slot 2 -- ghostty
  hyprnav spawn rand -- ghostty
  hyprnav spawn --print-workspace-id rand -- ghostty
  hyprnav spawn --no-focus 105 -- bun run dev:desktop
";

#[derive(Debug, Parser)]
#[command(
    name = "hyprnav",
    about = TOP_LEVEL_ABOUT,
    long_about = TOP_LEVEL_LONG_ABOUT,
    after_long_help = TOP_LEVEL_AFTER_HELP
)]
pub struct Cli {
    /// Command to run. If omitted, `daemon` is assumed.
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    #[command(
        about = "Run the persistent headless server.",
        long_about = "Run the persistent headless server.\n\nThis process owns environment state, the control socket, preview metadata, and runtime spawn operations. In the local Hyprland workflow it is typically started once per session."
    )]
    Daemon,
    #[command(
        about = "Open or advance the MRU workspace switcher overlay.",
        long_about = "Open or advance the MRU workspace switcher overlay.\n\nIf a switcher session is already open, trigger reuses it and steps the current selection instead of spawning another overlay."
    )]
    Trigger(TriggerArgs),
    #[command(
        about = "Open the environment/slot grid overlay.",
        long_about = "Open the environment/slot grid overlay.\n\nThis command talks to a resident grid UI process when one is already running. If needed it starts that process first, then asks it to show the existing window."
    )]
    Grid,
    #[command(name = "grid-server", hide = true)]
    GridServer,
    #[command(
        about = "Show lock state and current environment derived from a path.",
        long_about = "Show lock state and, optionally, the environment that would be derived from a given working directory."
    )]
    Status(StatusArgs),
    #[command(
        about = "Persistently lock the default environment.",
        long_about = "Persistently lock the default environment.\n\nAfter locking, commands like `goto --slot`, `run --slot`, and `slot resolve` can omit --env and resolve against the locked environment."
    )]
    Lock(LockArgs),
    #[command(
        about = "Clear the persistent environment lock.",
        long_about = "Clear the persistent environment lock."
    )]
    Unlock,
    #[command(
        about = "Create or delete environment records.",
        long_about = "Create or delete environment records.\n\n`env ensure` creates an environment if it does not exist yet and refreshes its timestamps if it already exists."
    )]
    #[command(subcommand)]
    Env(EnvCommand),
    #[command(
        about = "Create or refresh client records.",
        long_about = "Create or refresh client records.\n\nClient IDs are stored for attribution and future extension. They do not change environment resolution by themselves."
    )]
    #[command(subcommand)]
    Client(ClientCommand),
    #[command(
        about = "Assign, clear, or resolve environment slot bindings.",
        long_about = "Assign, clear, or resolve environment slot bindings.\n\nA slot binding maps a virtual slot number inside an environment to a physical Hyprland workspace."
    )]
    #[command(subcommand)]
    Slot(SlotCommand),
    #[command(
        about = "Navigate to the physical workspace bound to a slot.",
        long_about = "Navigate to the physical workspace bound to a slot.\n\nRequires either --env or a global lock."
    )]
    Goto(ResolveArgs),
    #[command(
        about = "Run a command in the workspace resolved from an environment slot.",
        long_about = "Run a command in the workspace resolved from an environment slot.\n\nThe command is spawned by the daemon using Hyprland's silent workspace targeting, so your current visible workspace does not change. Requires either --env or a global lock."
    )]
    Run(RunArgs),
    #[command(
        about = "Spawn a process tree onto a raw physical workspace.",
        long_about = "Spawn a process tree onto a raw physical workspace.\n\nThis command is separate from environment slots. It accepts either an explicit physical workspace ID or `rand`, which allocates a temporary high-ID workspace. Window placement is handled through the plugin using PID-tree matching."
    )]
    Spawn(SpawnArgs),
    #[command(name = "spawn-internal", hide = true)]
    SpawnInternal(SpawnInternalArgs),
}

#[derive(Debug, Args, Clone, Copy)]
pub struct TriggerArgs {
    /// Step backward instead of forward.
    #[arg(long)]
    pub reverse: bool,
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Path used to compute the current environment ID for display.
    #[arg(long)]
    pub cwd: Option<String>,
}

#[derive(Debug, Args)]
pub struct LockArgs {
    /// Environment ID to lock globally.
    pub env_id: String,
}

#[derive(Debug, Subcommand)]
pub enum EnvCommand {
    #[command(
        about = "Create or refresh an environment.",
        long_about = "Create or refresh an environment.\n\nResolution order:\n  1. --env <id>\n  2. --cwd <path>\n  3. current working directory\n\nIf no explicit --env is provided, the canonical environment ID is the realpath of the chosen directory. The display ID becomes the explicit --env string or the basename of that path."
    )]
    Ensure(EnvEnsureArgs),
    #[command(
        about = "Delete an environment and its slot bindings.",
        long_about = "Delete an environment and its slot bindings.\n\nIf the deleted environment is currently locked, the lock is cleared."
    )]
    Delete(EnvDeleteArgs),
}

#[derive(Debug, Subcommand)]
pub enum ClientCommand {
    #[command(
        about = "Create or refresh a client record.",
        long_about = "Create or refresh a client record."
    )]
    Ensure(ClientEnsureArgs),
}

#[derive(Debug, Subcommand)]
pub enum SlotCommand {
    #[command(
        about = "Bind a virtual slot to a physical workspace.",
        long_about = "Bind a virtual slot to a physical workspace.\n\nUse exactly one of --workspace, --managed, or --inherit. --managed allocates a server-owned workspace from the managed pool. --inherit keeps a local slot entry while delegating workspace resolution to the parent environment slot of the same number. Pass --launch followed by `-- <argv...>` to store a launch command for the slot. Environment resolution order:\n  1. --env <id>\n  2. --cwd <path>\n  3. current working directory\n  4. locked environment"
    )]
    Assign(SlotAssignArgs),
    #[command(
        about = "Remove a slot binding.",
        long_about = "Remove a slot binding.\n\nRequires either --env or a global lock."
    )]
    Clear(SlotClearArgs),
    #[command(
        about = "Resolve a slot to its physical workspace.",
        long_about = "Resolve a slot to its physical workspace.\n\nNamed dotted environments resolve the same slot number through their parent chain. The returned JSON includes the requested environment, the environment that supplied the concrete workspace binding, and the environment that supplied the launch command. Requires either --env or a global lock."
    )]
    Resolve(ResolveArgs),
    #[command(
        about = "Set or clear the stored launch command for a slot.",
        long_about = "Set or clear the stored launch command for a slot.\n\nThe launch command is stored as argv and can be triggered automatically when hyprnav navigates to that slot. For child-only command overrides, create a local slot row first with `slot assign --inherit`."
    )]
    #[command(subcommand)]
    Command(SlotLaunchCommand),
}

#[derive(Debug, Subcommand)]
pub enum SlotLaunchCommand {
    #[command(
        about = "Store a launch command for a slot.",
        long_about = "Store a launch command for a slot.\n\nRequires either --env or a global lock. The command after `--` is stored as raw argv and reused on future hyprnav navigation to that slot."
    )]
    Set(SlotCommandSetArgs),
    #[command(
        about = "Clear the stored launch command for a slot.",
        long_about = "Clear the stored launch command for a slot.\n\nRequires either --env or a global lock."
    )]
    Clear(SlotCommandClearArgs),
}

#[derive(Debug, Args)]
pub struct EnvEnsureArgs {
    /// Explicit environment ID. If omitted, the environment is derived from a path.
    #[arg(long)]
    pub env: Option<String>,
    /// Path used for environment derivation instead of the current working directory.
    #[arg(long)]
    pub cwd: Option<String>,
    /// Optional client ID to ensure/update alongside the environment.
    #[arg(long)]
    pub client: Option<String>,
}

#[derive(Debug, Args)]
pub struct EnvDeleteArgs {
    /// Environment ID to delete.
    #[arg(long)]
    pub env: String,
}

#[derive(Debug, Args)]
pub struct ClientEnsureArgs {
    /// Stable client identifier to create or refresh.
    #[arg(long)]
    pub client: String,
}

#[derive(Debug, Args)]
#[command(trailing_var_arg = true)]
pub struct SlotAssignArgs {
    /// Virtual slot number inside the environment. Must be positive.
    #[arg(long)]
    pub slot: i32,
    /// Fixed physical Hyprland workspace ID for this slot.
    #[arg(long)]
    pub workspace: Option<i32>,
    /// Allocate or reuse a server-managed workspace for this slot.
    #[arg(long)]
    pub managed: bool,
    /// Resolve this slot from the parent environment slot of the same number.
    #[arg(long)]
    pub inherit: bool,
    /// Explicit environment ID.
    #[arg(long)]
    pub env: Option<String>,
    /// Optional client ID stored as the updater of this binding.
    #[arg(long)]
    pub client: Option<String>,
    /// Path used for environment derivation when --env is omitted.
    #[arg(long)]
    pub cwd: Option<String>,
    /// Store the trailing argv as the slot launch command.
    #[arg(long)]
    pub launch: bool,
    /// Launch command and arguments to store after `--` when --launch is used.
    #[arg(last = true)]
    pub command: Vec<String>,
}

#[derive(Debug, Args)]
pub struct SlotClearArgs {
    /// Virtual slot number to remove. Must be positive.
    #[arg(long)]
    pub slot: i32,
    /// Explicit environment ID. Otherwise the locked environment is used.
    #[arg(long)]
    pub env: Option<String>,
    /// Optional client ID accepted for parity with the server API.
    #[arg(long)]
    pub client: Option<String>,
}

#[derive(Debug, Args)]
#[command(trailing_var_arg = true)]
pub struct SlotCommandSetArgs {
    /// Virtual slot number to update. Must be positive.
    #[arg(long)]
    pub slot: i32,
    /// Explicit environment ID. Otherwise the locked environment is used.
    #[arg(long)]
    pub env: Option<String>,
    /// Command and arguments to store after `--`.
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

#[derive(Debug, Args)]
pub struct SlotCommandClearArgs {
    /// Virtual slot number to update. Must be positive.
    #[arg(long)]
    pub slot: i32,
    /// Explicit environment ID. Otherwise the locked environment is used.
    #[arg(long)]
    pub env: Option<String>,
}

#[derive(Debug, Args)]
pub struct ResolveArgs {
    /// Virtual slot number to resolve. Must be positive.
    #[arg(long)]
    pub slot: i32,
    /// Explicit environment ID. Otherwise the locked environment is used.
    #[arg(long)]
    pub env: Option<String>,
}

#[derive(Debug, Args)]
#[command(trailing_var_arg = true)]
pub struct RunArgs {
    /// Virtual slot number to run inside. Must be positive.
    #[arg(long)]
    pub slot: i32,
    /// Explicit environment ID. Otherwise the locked environment is used.
    #[arg(long)]
    pub env: Option<String>,
    /// Command and arguments to run after `--`.
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

#[derive(Debug, Args)]
#[command(
    trailing_var_arg = true,
    after_long_help = "Examples:\n  hyprnav spawn 105 -- ghostty\n  hyprnav spawn rand -- bun run dev:desktop\n  hyprnav spawn --no-focus rand -- ghostty\n  hyprnav spawn --print-workspace-id rand -- ghostty"
)]
pub struct SpawnArgs {
    /// Keep the current workspace/focus instead of following the spawned window.
    #[arg(long)]
    pub no_focus: bool,
    /// Print the resolved physical workspace ID before the child process starts.
    #[arg(long)]
    pub print_workspace_id: bool,
    /// Physical workspace ID or `rand` for a temporary high-ID workspace.
    pub workspace: String,
    /// Command and arguments to run after `--`.
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

#[derive(Debug, Args)]
#[command(trailing_var_arg = true)]
pub struct SpawnInternalArgs {
    #[arg(long)]
    pub operation_id: String,
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

pub fn parse_args() -> Cli {
    let cli = Cli::parse();
    if cli.command.is_none() {
        Cli {
            command: Some(Command::Daemon),
        }
    } else {
        cli
    }
}
