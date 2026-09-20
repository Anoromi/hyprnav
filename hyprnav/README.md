# hyprnav

`hyprnav` is the local workspace environment server and overlay client
used with `hyprnav-plugin`.

The binary covers three surfaces:

- a headless daemon that owns state and command handling
- the MRU workspace switcher overlay
- the environment grid overlay

## Process Model

The main commands are:

- `hyprnav daemon`
- `hyprnav trigger`
- `hyprnav grid`

`daemon` is the long-lived headless server. It owns:

- the SQLite state database
- environment and slot bindings
- global lock state
- Hyprland state queries
- spawn operation tracking

`trigger` opens the MRU workspace switcher UI.

`grid` opens the environment grid UI.

In the local setup, Hyprland usually autostarts the daemon with:

```hyprlang
exec-once = hyprnav daemon
```

Most non-daemon commands will auto-start the daemon if it is not already
running.

## Environment Model

An environment maps virtual slot numbers such as `1`, `2`, `3` to physical
Hyprland workspaces such as `5`, `101`, or `103`.

Explicit named environment IDs can also form a hierarchy:

- `x`
- `x.y`
- `x.y.z`

Only explicit named env IDs participate in this tree. Path-derived env IDs stay
flat.

Three local binding kinds exist:

- `fixed`
  Maps a slot to an explicit physical workspace ID.
- `managed`
  Allocates a server-owned workspace from the managed range starting at `101`.
- `inherit`
  Keeps a local slot entry but resolves the workspace from the parent
  environment slot of the same number.

State is persisted in:

- `$XDG_STATE_HOME/hyprnav/state.sqlite3`
- fallback: `~/.local/state/hyprnav/state.sqlite3`

## Environment Resolution Rules

These rules matter because not every command resolves environments the same way.

### Commands that can infer an environment from the working directory

- `env ensure`
- `slot assign`

Resolution order:

1. explicit `--env`
2. canonicalized `--cwd`
3. canonicalized current working directory
4. for `slot assign` only: global lock fallback if cwd resolution fails

### Commands that require an explicit environment or a global lock

- `slot clear`
- `slot resolve`
- `goto`
- `run`

Resolution order:

1. explicit `--env`
2. global lock
3. otherwise fail

The global lock is set with:

```bash
hyprnav lock <env-id>
```

and cleared with:

```bash
hyprnav unlock
```

## Hierarchical Slot Resolution

For a named env like `x.y.z`, slot lookup walks the same slot number up the
tree:

1. `x.y.z`
2. `x.y`
3. `x`

Workspace resolution rules:

- a local `fixed` or `managed` row wins
- a local `inherit` row keeps walking upward
- a missing local row also keeps walking upward
- if no ancestor provides a concrete binding, resolution fails

Launch command resolution is separate:

- the nearest non-null command on the same slot wins
- a child env can override only the command by first creating
  `slot assign --inherit`

## Command Reference

### `daemon`

```bash
hyprnav daemon
```

Starts the headless server. If another daemon is already responding, the
command exits successfully without starting a second one.

### `trigger`

```bash
hyprnav trigger
hyprnav trigger --reverse
```

Opens the MRU workspace switcher overlay.

Behavior:

- the first invocation opens the overlay
- repeated trigger calls reuse the active switcher session when one is open
- `--reverse` steps backward through the same session

### `grid`

```bash
hyprnav grid
```

Opens the environment grid overlay.

The grid shows one row per environment and the currently mapped slots for each
environment.

### `status`

```bash
hyprnav status
hyprnav status --cwd /some/path
```

Prints JSON status with:

- `locked_environment_id`
- `current_environment_id`

`current_environment_id` is derived from the provided `--cwd` or omitted if no
cwd is supplied.

### `lock`

```bash
hyprnav lock <env-id>
```

Sets the persistent global lock.

Generic commands such as `goto --slot 2` and `run --slot 3 -- ...` resolve
against this lock when `--env` is omitted.

### `unlock`

```bash
hyprnav unlock
```

Clears the global lock.

### `env ensure`

```bash
hyprnav env ensure
hyprnav env ensure --env demo
hyprnav env ensure --cwd /path/to/project
hyprnav env ensure --env demo --client desktop
```

Creates or refreshes an environment record.

Behavior:

- if `--env` is provided, that string becomes the canonical environment ID
- otherwise the canonical environment ID is `realpath(cwd)`
- the display name is `--env` if provided, otherwise `basename(realpath(cwd))`
- if `--client` is provided, the client record is also ensured

### `env delete`

```bash
hyprnav env delete --env demo
```

Deletes an environment and its slot bindings. If that environment is currently
locked, the lock is also cleared.

### `client ensure`

```bash
hyprnav client ensure --client desktop
```

Ensures a stable client record exists. This is mainly for attribution and future
extension; it does not affect environment resolution by itself.

### `slot assign`

```bash
hyprnav slot assign --slot 1 --workspace 5 --env demo
hyprnav slot assign --slot 2 --managed --env demo
hyprnav slot assign --slot 3 --managed --cwd /path/to/project
hyprnav slot assign --slot 2 --inherit --env demo.child
hyprnav slot assign --slot 4 --managed --env demo --launch -- ghostty --class work
```

Assigns a virtual slot to a physical workspace.

Rules:

- use exactly one of `--workspace <id>`, `--managed`, or `--inherit`
- `--managed` allocates from the managed pool starting at `101`
- `--inherit` is valid only for named dotted env IDs that have a parent
- reassigning an existing managed slot keeps its current managed workspace ID
- `--launch -- <argv...>` stores a launch command for future hyprnav navigation to that slot
- omitting `--launch` preserves any existing stored launch command

### `slot clear`

```bash
hyprnav slot clear --slot 2 --env demo
```

Removes a slot binding. Clearing a managed binding releases that managed
workspace ID back to the pool. The physical Hyprland workspace itself is not
deleted.

### `slot resolve`

```bash
hyprnav slot resolve --slot 2 --env demo
hyprnav slot resolve --slot 2
```

Prints JSON describing the resolved slot binding:

- `environment_id`
- `binding_environment_id`
- `command_environment_id`
- `slot_index`
- `physical_workspace_id`
- `binding_kind`
- `launch_argv`

Without `--env`, this requires a global lock.

### `slot command set`

```bash
hyprnav slot command set --slot 1 --env demo -- ghostty --class work
hyprnav slot command set --slot 2 -- bun run dev:desktop
```

Stores a launch command for an existing slot binding.

Notes:

- the command after `--` is stored as raw argv
- if you want a child env to override only the command, first create a local
  row with `slot assign --inherit`
- without `--env`, this requires a global lock

### `slot command clear`

```bash
hyprnav slot command clear --slot 1 --env demo
hyprnav slot command clear --slot 2
```

Clears a stored launch command from an existing slot binding.

Clearing a child command exposes the next command from the parent chain, if one
exists. Without `--env`, this requires a global lock.

### `goto`

```bash
hyprnav goto --slot 2 --env demo
hyprnav goto --slot 2
```

Resolves a slot and switches Hyprland to the resolved physical workspace.

If that slot has a stored launch command, hyprnav runs it only when the target
workspace is currently empty. Re-entering the same workspace while the app is
already present does not launch another copy.

Without `--env`, this requires a global lock.

### `run`

```bash
hyprnav run --slot 2 --env demo -- ghostty
hyprnav run --slot 3 -- bun run dev:desktop
```

Resolves a slot and launches a command into that physical workspace without
changing the user’s current visible workspace.

Notes:

- the command after `--` is passed as raw argv
- no shell concatenation is used
- without `--env`, this requires a global lock

### `spawn`

```bash
hyprnav spawn 105 -- ghostty
hyprnav spawn rand -- bun run dev:desktop
hyprnav spawn --no-focus rand -- ghostty
```

Spawns a foreground-attached process tree targeted at a raw physical workspace.

Behavior:

- `<workspace>` is either a positive integer or `rand`
- `rand` allocates a temporary high-ID workspace reservation
- the spawned command inherits normal terminal stdio
- `Ctrl+C` still kills the foreground app through the terminal path
- placement is PID-tree based and plugin-assisted
- matching windows are placed once on initial appearance

`--no-focus` is opt-out focus preservation:

- the new window is still placed on the target workspace
- Hyprland should not switch your current focus/workspace to follow it

`spawn` does not use environment slots in v1. It targets physical workspace IDs
directly.

## Typical Flows

### Create and use a named environment

```bash
hyprnav env ensure --env demo
hyprnav slot assign --env demo --slot 1 --workspace 1
hyprnav slot assign --env demo --slot 2 --managed
hyprnav slot assign --env demo --slot 3 --managed
hyprnav lock demo
hyprnav goto --slot 2
```

### Override a child command while inheriting the parent workspace

```bash
hyprnav env ensure --env x
hyprnav env ensure --env x.y.z
hyprnav slot assign --env x --slot 2 --managed --launch -- ghostty
hyprnav slot assign --env x.y.z --slot 2 --inherit
hyprnav slot command set --env x.y.z --slot 2 -- kitty
hyprnav slot resolve --env x.y.z --slot 2
```

### Create an environment from the current directory

```bash
cd ~/code/stolen/t3code
hyprnav env ensure
hyprnav slot assign --slot 1 --workspace 1
hyprnav slot assign --slot 2 --managed
```

### Launch an app into a managed slot without changing your current workspace

```bash
hyprnav lock demo
hyprnav run --slot 2 -- ghostty
```

### Launch an app tree into a temporary workspace

```bash
hyprnav spawn rand -- bun run dev:desktop
```

## Local Workflow Notes

For local development in this repo:

- build the switcher with `hyprnav-dev-build`
- rebuild/reload the plugin with `hyprnav-plugin-dev-reload`
- preserve the local wrapped command name `hyprnav`

The main external integration files are:

- `/etc/nixos/anoromi/hyprland.nix`
- `/etc/nixos/anoromi/config/hypr/hyprland.conf`

## Browser workspace navigation

`hyprnav tab` connects Firefox/Zen and Chromium tabs to environment slots. Different slots
can reuse one named tab and change only its `workspace` query parameter.
See [browser setup and demo](browser-extension/README.md).

## Temporary slots

`hyprnav slot temp [--env X] [--name N] [--owner who] [-- cmd]` creates an
unnumbered slot (index 1000 and up) on a fresh managed workspace. With a
command it spawns the process tree into that slot with a stick and keeps
focus where it is. Temporary slots show up in the grid by name, never by
digit. They are released once their workspace has been empty for 30 seconds,
or with `hyprnav slot remove --env X --slot S|--name N`. `hyprnav slot temps`
lists them with owner and empty timer. Grid cells carry `temporary`,
`unnumbered`, `owner` and `empty_for_ms`.

## Sticks

`hyprnav spawn` pins the spawned process tree to its workspace: later
windows from that tree (dialogs, pickers, second windows) open there,
silently, for as long as the tree or any of its windows lives. `spawn
--no-stick` restores the old behaviour. `hyprnav stick list|release|add|move`
inspects and changes sticks. Requires hyprnav-plugin.

## Events

The daemon pushes state changes instead of making clients poll. Beside the
request socket it opens a second Unix socket:

```
$XDG_RUNTIME_DIR/hx/<fnv1a64(HYPRLAND_INSTANCE_SIGNATURE)>/events.sock
```

Unlike the request socket it accepts many concurrent subscribers, and it is
write-only: the daemon never reads from it and ignores anything a client
sends. Every event is one JSON object per line terminated by `\n`, and every
event carries `"event": <string>` and `"ts_ms": <u64 unix ms>`.

On connect the daemon immediately sends the current state:

```json
{"event":"hello","ts_ms":1789874943432,"version":1}
{"event":"agents","ts_ms":1789874943432,"agents":[]}
{"event":"slots","ts_ms":1789874943432}
```

- `agents` carries the full current list, serialised exactly like the
  `agents_list` reply. It is sent whenever any agent changes: register, beat
  (including `current_target`, `last_action` and `action_count` changes),
  label, state transitions (`working`, `idle`, `waiting_for_user`,
  `finished`), finish, and when the registry prunes a dead agent.
- `slots` has no payload. It is sent whenever slot, environment, stick or
  temporary-slot state changes: temporary slot created or removed, the reaper
  releasing an empty one, environment switch, workspace goto, lock and pin
  changes, stick add, release or move — anything that would change
  `ui_snapshot_grid` output. Clients re-request the grid snapshot when they
  see it.

Bursts are coalesced: at most one `agents` and one `slots` event per ~50 ms,
last state wins, so a chatty MCP cannot flood subscribers. A subscriber that
stops draining its socket is dropped rather than allowed to stall the daemon
or the other subscribers.

### `events`

```bash
hyprnav events           # stream until interrupted
hyprnav events --once    # print hello + agents + slots, then exit
```

Prints the stream to stdout, one line per event.

## Agents

An MCP process announces itself with `agent_register` and gets an unnumbered
temporary slot of its own. Its snapshot carries, besides the slot and the
live state, two optional strings that the host app supplies:

- `thread_id` — the conversation the agent is acting for
- `thread_environment_id` — the host's own environment for that thread

Both are opaque to hyprnav: it stores, serialises and reports them, never
interprets them. They are `null` when the host did not supply them. They
appear in `agents_list`, in the `agents` event and in `hyprnav agents`, so a
dashboard can group frames by thread. The cua MCP bridge fills them from
`T3CODE_THREAD_ID` and `T3CODE_ENVIRONMENT_ID`.

Re-registering the same `agent_id` keeps the slot and refreshes whichever of
the two the caller passed.

```bash
hyprnav agent register --id a1 --label "planner" \
  --thread-id T1 --thread-environment-id E1
hyprnav agents
```

## Window frames

Beside the request socket the daemon opens a third Unix socket:

```
$XDG_RUNTIME_DIR/hx/<fnv1a64(HYPRLAND_INSTANCE_SIGNATURE)>/frames.sock
```

A client sends exactly one JSON line and then only reads:

```json
{"address":"0x55ea1ad9c6d0","fps":8,"quality":60,"max_width":640}
```

`fps` is clamped to 1..15 (default 8), `quality` to 30..90 (default 60) and
`max_width` to 64..3840 (default 640); all three may be omitted. The reply is
a `multipart/x-mixed-replace` byte stream with the fixed boundary `frame`:

```
--frame\r\nContent-Type: image/jpeg\r\nContent-Length: <n>\r\n\r\n<n bytes>\r\n
```

The daemon closes the connection when the window closes or unmaps. An
address that does not name a live window gets one line,
`{"error":"unknown_window"}`, and nothing else.

Every client watching the same address shares one capture, running at the
loosest settings any of them asked for. Each client holds a single
latest-frame slot: a reader that falls behind drops frames rather than
queueing them or delaying anyone else. A client joining a window that is
already being captured is handed the most recent frame immediately, so it
sees something even if the window never changes again.

### `frames`

```bash
hyprnav frames 0x55ea1ad9c6d0 > out.mjpeg
hyprnav frames 0x55ea1ad9c6d0 --fps 12 --quality 70 --max-width 960 | ffplay -f mpjpeg -
```

### `hyprnav-capture`

The pixels come from a C helper the daemon spawns once and restarts with
backoff. It holds a single Wayland connection and does three things the
daemon would otherwise need Wayland and JPEG crates for:

- follows `ext-foreign-toplevel-list-v1` for the per-window `identifier`
- asks `hyprland-toplevel-mapping-v1` for each toplevel's window address, so
  the two can be paired with what `hyprctl clients` prints
- runs an `ext-image-copy-capture-v1` session per watched window, over an
  `ext_foreign_toplevel_image_capture_source_manager_v1` source, into a
  reused `wl_shm` buffer; scales with an integer box filter and encodes with
  libjpeg-turbo

Standalone modes:

```bash
hyprnav-capture --list             # one `add` line per window, then exit
hyprnav-capture --resolve 0x…      # print the bare identifier, exit 3 if unknown
hyprnav-toplevel-map               # the same binary under its identification name
```

With no arguments it speaks NDJSON on stdout and takes NDJSON commands on
stdin. stdout:

```json
{"ev":"add","addr":"0x55ea1ad9c6d0","id":"18000004","app":"org.telegram.desktop","title":"…"}
{"ev":"title","addr":"0x…","title":"…"}
{"ev":"close","addr":"0x…"}
{"ev":"ready"}
{"ev":"capture_failed","addr":"0x…","reason":"stopped"}
{"ev":"frame","addr":"0x…","len":7897,"w":640,"h":365,"enc_ms":2.34}
```

Every existing window is announced with `add` before `ready`. A `frame` line
is followed immediately by exactly `len` raw JPEG bytes and then the next
line; nothing is interleaved. Addresses are lowercase `0x` + hex without
leading zeros. stdin:

```json
{"op":"start","addr":"0x…","max_width":640,"quality":60,"max_fps":8}
{"op":"stop","addr":"0x…"}
```

A second `start` for a running address only updates its parameters; the
daemon does the refcounting.

### What frame streaming costs

`ext-image-copy-capture` completes a frame only when the compositor repaints
the window, and the helper compares the raw buffer before encoding and skips
identical pixels. So a window that is not changing — including any window on
a workspace that is not on screen — produces its first frame and then
nothing at all: no encode, no wakeup, no traffic.

Measured in the lab (Hyprland 0.56.2, 1920x1080, `max_width` 640):

| Watched window | Frames | Helper CPU |
|---|---|---|
| terminal scrolling on the visible workspace, two clients | 15 / 10 s | 40 ms / 10 s |
| GTK window on a workspace that is not on screen | 1 / 20 s | 10 ms / 20 s |

Downscale plus JPEG encode of a 1920x1080 window to 640 px wide costs 2.3 to
5 ms, logged per frame at `debug` level (`encode_ms`). A start that produces
no frame within two seconds is reported at `warn` level.
