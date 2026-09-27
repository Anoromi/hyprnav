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

The grid shows one row per leaf environment. Environment ids are prefix
chains (`p.x` project, `p.x.w` worktree, `p.x.w.a.t` thread) and a child
resolves numbered slots through its ancestors, so an ancestor's frames are
the same workspaces in every descendant. The grid does not repeat them as
rows of their own:

- A row candidate is an environment with a binding of its own, or the locked
  or current environment, that resolves at least one slot.
- A candidate is a row unless another candidate descends from it. An ancestor
  with no live descendant is its own row, and so is an ancestor that owns a
  temporary slot (a temp shows only in its owner's row).
- A row's cells are its numbered slots resolved through the chain plus its own
  temporaries, in slot order. Two threads under one worktree are two rows,
  each showing the worktree's frames.
- Row order is unchanged: the row whose chain holds the lock, then the row
  whose chain holds `--cwd`, then recent focus.

Snapshot fields (`ui_snapshot_grid`), per cell:

- `environment_id`: the row's leaf. `environment_title`: the deepest titled
  level of the chain, else the deepest cwd name, else the leaf id.
- `environment_chain`: `[{id, title, label, locked}]` root to leaf. `label` is
  the title, else the last component of the level's cwd, else empty; never a
  raw id.
- `environment_locked`: some level of the chain is locked;
  `locked_environment_id` names it.
- `owner_environment_id` (also `binding_environment_id`): the environment that
  binds the slot; slot mutations (remove, rename, command) go there.
  `owner_title` is its label. `shared` is true when the owner is an ancestor;
  `inherited` is kept as an alias for older clients.
- Going to a cell uses the leaf (`workspace_goto` with `environment_id`),
  which resolves the same workspace and honours a leaf launch command.

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

A temporary slot belongs to the environment that created it and shows in one
place only: after the numbered frames of that environment's own grid row.
Numbered slots are still resolved down the environment chain, so a child row
shows its ancestors' digits as shared frames; temporary ones are not, so a
child never displays its parent's scratch frame. An ancestor that owns a
temporary slot keeps a row of its own for it even when it has descendants. They are also left out of the MRU switcher
snapshot, so Alt-Tab never lands on one. The index threshold is the rule:
`slot_index >= 1000` means temporary, and `slot temp` is the only thing that
allocates there.

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
{"address":"0x55ea1ad9c6d0","codecs":["av1","h264","mjpeg"],"max_width":640,"max_fps":8,"follow":"transient"}
```

| field | meaning |
|---|---|
| `address` | `0x…` or `address:0x…`, as `hyprctl clients` prints it |
| `codecs` | what the client can decode, best first. `"format":"av1"` is shorthand for a one-element list. Absent means `mjpeg`, which is what every client written before this path sends |
| `max_width` | 64..3840, default 640. The capture is scaled down to it |
| `max_fps` | 1..15, default 8. Also the coalescing window the compositor applies to damage, `1000/max_fps` ms. `fps` is accepted as an alias |
| `quality` | 30..90, default 60. MJPEG only |
| `follow` | `transient` captures the target's dialog while one is mapped, `target` (the default) always captures the target |

The daemon picks the first codec the client listed that it is configured for
and this machine can actually encode. An address that does not name a live
window gets one line, `{"error":"unknown_window"}`, and nothing else; a
request no configured encoder can satisfy gets `{"error":"no_codec"}`, and
one that would exceed `max_pipelines` with no shareable pipeline gets
`{"error":"busy"}`.

### The record stream (`av1`, `h264`)

A byte stream of records, big-endian, which is what `DataView.getUint32(o)`
reads by default:

```
u32 magic 'HNVF' | u32 len | u32 flags | u64 pts_us | u16 width | u16 height | payload[len]
flags: 1 = KEYFRAME, 2 = CONFIG, 4 = KEEPALIVE
```

One record is one temporal unit (AV1) or one access unit (H.264). `width`
and `height` are the picture, not the padded buffer the encoder was given:
VAAPI wants the height a multiple of 16, so a 1902x1062 window at
`max_width` 640 is encoded 640x368 and reported 640x357. Clients should size
their canvas from the decoded frame and use the header only as a hint.

The **first record a client receives is always CONFIG**. Its payload is a
NUL-terminated codec string followed by the codec's out-of-band
configuration, if it has any:

```
av01.0.08M.08\0                       AV1: nothing more
avc1.640C16\0<SPS><PPS in Annex-B>    H.264: the parameter sets, no `description`
```

The H.264 string is read out of the SPS rather than assumed, because
`h264_vaapi` here emits High profile and a decoder told "constrained
baseline" may refuse the stream. H.264 is Annex-B and must be configured
without a `description`.

After CONFIG comes the **GOP cache**: the last keyframe and every record
since. A client joining a stream that has been running for a while decodes
its first picture immediately instead of waiting up to two seconds for the
next keyframe. Verified by writing a late joiner's stream to IVF: its first
frame is a keyframe.

Every record with KEYFRAME set is independently decodable. A record with
KEEPALIVE set has an empty payload and arrives after ten seconds of silence;
it is how a client tells "this window is static" from "the daemon died".
Static windows simply stop producing records — there is no other heartbeat.

Params that change the encoder (a different window size, because
`follow=transient` switched to a dialog) restart it and emit a fresh CONFIG
record. Clients reconfigure their decoder whenever they see one.

### The MJPEG stream (`mjpeg`)

Unchanged, and still the default: a `multipart/x-mixed-replace` byte stream
with the fixed boundary `frame`.

```
--frame\r\nContent-Type: image/jpeg\r\nContent-Length: <n>\r\n\r\n<n bytes>\r\n
```

Each MJPEG client holds a single latest-frame slot, so a slow reader drops
frames rather than queueing them, and a client joining a still window is
handed the most recent frame immediately.

### Fan-out

Pipelines are keyed by `(address, codec, width)`. Two clients that agree on
all three share one capture, one encoder and one GOP cache; two that
disagree get two pipelines. `max_pipelines` bounds the total — beyond it the
daemon offers an existing pipeline for the same window if the client listed
its codec, and answers `busy` otherwise.

Latest-wins is wrong for video, because P-frames need their predecessors. So
each client gets a bounded queue of whole records; when it overflows, the
backlog is thrown away and replaced with the GOP cache and the client
resyncs from the keyframe. A slow client never stalls the pipeline and never
sees a hole.

### `[frames]` configuration

`~/.config/hyprnav/config.toml`, all optional:

```toml
[frames]
encoder = "auto"                    # auto | vaapi | software
vaapi_device = "/dev/dri/renderD128"
codecs = ["av1", "h264", "mjpeg"]   # allow-list, in preference order
default_width = 640
max_pipelines = 4
force_fallback = false              # ignore the plugin and pace captures on a timer

[frames.codec.av1]
q = 30                              # -q:v; VAAPI ignores -qp
bitrate = 0                         # kbit/s; 0 keeps constant quality
gop = 16
```

`encoder = "auto"` does not trust `ffmpeg -encoders`: a name in that list
means the build has the encoder, not that this GPU will start it. Each
candidate gets a quarter-second null encode once at startup and only what
survives is offered to clients. On this machine that leaves
`av1/vaapi, h264/vaapi`, logged at `debug` level.

### `frames`

```bash
hyprnav frames 0x55ea1ad9c6d0 --codec av1 --ivf -o /tmp/w.ivf   # ffprobe/ffplay
hyprnav frames 0x55ea1ad9c6d0 --codec h264 -o /tmp/w.hnvf       # raw records
hyprnav frames 0x55ea1ad9c6d0 --codec av1 --follow transient -o /tmp/w.hnvf
hyprnav frames 0x55ea1ad9c6d0 --codec mjpeg --fps 12 | ffplay -f mpjpeg -
```

`--ivf` unwraps the records into an IVF file (CONFIG and KEEPALIVE records
dropped, geometry and frame count patched into the header at the end), which
needs a seekable output and therefore `-o`.

### How a frame happens

```
plugin: commitState hook ──window_damaged──► daemon ──{"op":"capture"}──► hyprnav-capture
                                               │                              │ toplevel-export, ignore_damage=1
                                               │                              │ box downscale to max_width
                                               │                              ▼ raw bgr24 on fd 3
                                               └── HNVF records ◄── ffmpeg ◄───┘
```

Nothing polls. The daemon tells the plugin which windows to watch
(`{"op":"frames_watch","addr":"0x…","on":true,"interval_ms":125}` on the
existing spawn socket) and the plugin pushes one `window_damaged` line per
window per `interval_ms` down the same connection. A window nobody watches
costs an empty-map test and at most one hash lookup per commit.

Three details are not obvious and each of them is load-bearing:

- **The hook is on `CWLSurfaceResource::commitState`, not on the renderer.**
  The renderer's damage paths are gated on visibility: measured in the lab,
  zero `damageSurface` calls in ten seconds for a window on a hidden
  workspace and ten in as many seconds once it is on screen. A client
  painting commits either way. The renderer hooks are kept for window-level
  changes like a move or a resize.
- **A hidden client only paints when it is handed a frame callback**, and the
  standalone capture render is what hands it one — but a toolkit needs more
  than one callback to turn a changed label into a committed buffer. The
  daemon therefore allows a burst of at most three unprompted captures, reset
  by every damage report. `render_unfocused`, which the daemon still sets on
  watched windows, covers this on a real session where the monitor repaints
  anyway; on an idle headless output nothing renders at all.
- **ffmpeg holds a picture until the next one arrives.** One raw frame in
  produces zero bytes out; two produce both. Without help a damage-driven
  stream would always be one change behind and a window that changed once
  would show nothing. When no new frame turns up within `1000/max_fps` ms
  (at least 150) the last one is written again to push it through: one
  encode, a handful of bytes, and only while the window is quiet.

`follow=transient` uses the same channel: the plugin reports
`transient_mapped` / `transient_unmapped` for a dialog whose parent is
watched, including one that was already open when the watch began, and the
pipeline retargets without restarting anything it does not have to.

When the plugin is not loaded — plain Hyprland, or `force_fallback = true` —
the helper falls back to its old paced loop at `max_fps` with the
identical-pixel dedupe. Everything above the helper is unchanged.

### `hyprnav-capture`

The pixels come from a C helper. One long-lived instance does window
identification and MJPEG for the whole daemon; each video pipeline gets its
own, because raw pixels need a private fd.

- follows `ext-foreign-toplevel-list-v1` for the per-window `identifier`
- asks `hyprland-toplevel-mapping-v1` for each toplevel's window address, so
  the two can be paired with what `hyprctl clients` prints
- renders each watched window on demand with `hyprland-toplevel-export-v1`
  into a reused `wl_shm` buffer, scaling with an integer box filter, then
  either encoding with libjpeg-turbo or writing packed 3-byte rows

Standalone modes:

```bash
hyprnav-capture --list             # one `add` line per window, then exit
hyprnav-capture --resolve 0x…      # print the bare identifier, exit 3 if unknown
hyprnav-toplevel-map               # the same binary under its identification name
hyprnav-capture --backend copy-capture   # the other backend, see below
hyprnav-capture --raw-fd 3         # raw pixels to fd 3, JSON headers on stdout
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
{"ev":"raw","addr":"0x…","len":706560,"w":640,"h":368,"real_h":357,"pix":"bgr24"}
```

Every existing window is announced with `add` before `ready`. A `frame` line
is followed immediately by exactly `len` raw JPEG bytes and then the next
line. A `raw` line has **no** payload on stdout: the pixels go to the fd
given by `--raw-fd`, which the daemon creates as a pipe per pipeline and
leaks into the helper at spawn time. That split is deliberate — the scratch
experiment wrote raw pixels to stdout between JSON lines, which
desynchronises any reader parsing both. `h` is the buffer height, padded up
to a multiple of 16 for the encoder; `real_h` is how many of those rows are
picture. Addresses are lowercase `0x` + hex without leading zeros. stdin:

```json
{"op":"start","addr":"0x…","max_width":640,"quality":60,"max_fps":8,"mode":"raw","paced":0}
{"op":"capture","addr":"0x…"}
{"op":"stop","addr":"0x…"}
```

`capture` renders exactly one frame. With `paced` set the helper re-arms
itself every `1000/max_fps` ms instead, which is the no-plugin fallback. A
second `start` for a running address only updates its parameters; the daemon
does the refcounting.

### Why toplevel-export and not ext-image-copy-capture

`--backend copy-capture` is the standard `ext-image-copy-capture-v1` path and
is the better protocol on paper: it is damage-driven, so an idle window
wakes nobody. It is unusable here. Hyprland only completes those frames
while the window is being rendered for a monitor, so a window on a workspace
that is not on screen — which is where an agent's windows live — freezes at
whatever it last showed. A ticking countdown on a hidden workspace yields 2
frames in 10 s; the window property `render_unfocused` does not change that.

`hyprland-toplevel-export-v1` renders the requested window standalone into
our buffer when we ask, whatever its visibility, so the same countdown
yields 12. That is why it is the default. `copy-capture` stays selectable
for the day the other implementation catches up.

### What frame streaming costs

Measured in the lab (Hyprland 0.56.2, headless 1920x1080, a 1902x1062 GTK4
window on a workspace that is not on screen, `max_width` 640, `max_fps` 8,
AV1 on VAAPI at `q 30`). CPU is helper plus ffmpeg, as a share of one core.

| case | captures | CPU | wire |
|---|---|---|---|
| countdown ticking once a second | 60 / 28 s | 3.7 % | 31 KB / 28 s ≈ 1.1 KB/s |
| the same, two clients | one pipeline, one ffmpeg | as above | as above |
| still window, after it settles | 0 / 20 s | 0.05 % | 0 B/s + a keepalive every 10 s |
| H.264 instead of AV1 | as above | as above | 6.4 KB / 12 s ≈ 530 B/s |
| MJPEG, `--fps 8`, `quality 60` | — | 0.7 % | 60–300 KB/s |

A ten-second AV1 capture of the ticking window is 13 frames and 9.2 KB, and
`ffprobe` decodes it. The still-window row is the whole point: two captures
when the client joins, then nothing at all until the window changes.

While an address has subscribers the daemon also sets `render_unfocused` on
that window and clears it when the last one leaves.
