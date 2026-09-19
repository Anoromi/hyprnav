# Hard sticking: plan

Date: 2026-09-19. Status: Phases 0 to 4 implemented and verified in the lab, see HARD-STICKING-TESTING.md. Phase 5 (live rollout) pending approval.

Scope: hyprnav + hyprnav-plugin in this repo. Nothing in
`/etc/nixos` changes until the last phase, and only with explicit approval.

## Problem

Background agents drive apps through the desktop MCP in their own workspaces.
Windows those apps open later (dialogs, pickers, second windows, portal file
choosers) appear on whatever workspace is focused and cover the user's work.

## Decisions

| Topic | Decision |
|---|---|
| Scope | Only process trees launched through `hyprnav spawn`. Other apps keep stock behaviour. |
| Duration | For the life of the spawned tree, and for any window later attributed to it. Not bound to the `hyprnav spawn` process staying alive. |
| Signal | Silent. No urgent flag, no notification, no focus change, no workspace switch. |
| Unattributable windows | 1. xdg parent toplevel's workspace. 2. Same PID or same app class as a window already in a stuck workspace. 3. Otherwise stock placement. |

## What exists today

- `hyprnav spawn <ws> -- cmd` asks the daemon to `SpawnPrepare`, forks
  `spawn-internal` which reports its PID (`SpawnStart`) and execs the command.
  The daemon tells the plugin to `watch {operation_id, workspace_id, root_pid,
  focus_policy}`. The plugin's `CSpawnManager` listens to `window.open` and,
  for windows whose PID descends from `root_pid`, moves them to the workspace
  and optionally restores focus (`preserve`).
- The watch ends when `hyprnav spawn` exits (`SpawnFinish` -> `unwatch`) or
  the cleanup thread notices `root_pid` is gone. Since `spawn-internal` execs
  the app, the root PID is the app itself; when it exits the watch ends. That
  part is right. But a daemon restart drops every watch, and nothing survives
  a plugin reload.
- The plugin is not loaded on the live compositor. `~/.local/state/hyprnav/plugin-load.log`
  shows 50 failed attempts on 2026-09-18 with
  `undefined symbol: _ZN8CMonitor17activeWorkspaceIDEv`: the dev-mode `.so`
  in the state dir was built against an older Hyprland. `hyprctl plugin list`
  confirms only hypr-agent-portal is loaded.
- The Portal plugin has a separate, session-scoped version of this
  (`WorkspaceSession`, `placeRelatedWindowOnRootWorkspaceEarly`) that uses
  `window.openEarly`, same-PID and X11 transient matching. It is the better
  hook and matching reference, but it is bound to paste sessions.

## Design

A new `CStickManager` in hyprnav-plugin, replacing the placement half of
`CSpawnManager` (the spawn socket protocol stays).

### Data

```
struct SStickRoot {
    std::string     stickId;         // stable, from the daemon
    int             workspaceId;     // target
    int             monitorId;       // fallback monitor for recreation
    pid_t           rootPid;         // spawned process
    std::set<pid_t> knownPids;       // rootPid plus every descendant seen so far
    std::set<std::string> classes;   // app classes seen in this tree
    std::set<uintptr_t> windows;     // windows currently attributed
    uint64_t        lastSeenMs;
};
```

Roots are keyed by `stickId`. A root is dropped only when `rootPid` is gone
**and** it has no live windows **and** no known descendant PID is alive.
This is the difference from today: a Firefox that forks and lets its
launcher exit keeps its stick.

### Attribution, in order, at `window.openEarly`

1. Window PID is `rootPid` or a descendant (`/proc/<pid>/status` PPid walk,
   cached in `knownPids`). Existing logic, kept.
2. `window->parent()` or `x11Parent()` is a stuck window: same root.
   Covers xdg-foreign portal dialogs and X11 transients.
3. Window PID equals the PID of any stuck window (thread or re-exec case).
4. Window class equals a class recorded in exactly one root, and that root
   has at least one mapped window. Ambiguous classes (two roots, or a class
   that also has windows outside any stick) do not match.
5. No match: leave the window alone.

Once attributed, the window is recorded and its class added to the root.
`window.open` (later) re-applies the move if Hyprland relocated the window
between early and mapped, which the Portal code shows can happen.

### Placement

- `window->m_noInitialFocus = true` and suppress `ACTIVATE` and
  `ACTIVATE_FOCUSONLY` before the window maps, so `focus_on_activate` in the
  user's config cannot pull the workspace.
- `moveToWorkspace(target)` with the target recreated on `monitorId` if it was
  destroyed. If the target is the focused workspace, do nothing beyond focus
  suppression.
- Silent: `m_isUrgent` is cleared if set; no `window.urgent` emission.
- If a stuck window becomes the focused workspace's active window because the
  user went there, nothing special happens. Sticking only governs placement.

### Persistence

The daemon owns the source of truth so a plugin reload or compositor restart
does not lose sticks:

- New table `sticks(stick_id, workspace_id, monitor_id, root_pid, created_at)`
  in `state.sqlite3`, written at `SpawnStart`, removed when the root is gone
  and the plugin reports zero windows for it.
- On plugin (re)connect, the daemon replays every stick as `stick {…}` lines
  on the spawn socket. The plugin then walks existing windows once and
  attributes any that match, moving them if they sit elsewhere.
- The `watch`/`unwatch` protocol becomes `stick`/`unstick`/`list`. `unwatch`
  on `SpawnFinish` is removed: finishing the CLI no longer ends the stick.

### CLI

```
hyprnav spawn <ws> -- cmd            unchanged, now sticky by default
hyprnav spawn --no-stick <ws> -- cmd today's behaviour: watch ends with the CLI
hyprnav stick list                   roots, PIDs, classes, windows, workspace
hyprnav stick release <stick-id>     forget a root, windows stay where they are
hyprnav stick add <ws> <pid>         adopt an already running tree
hyprnav stick move <stick-id> <ws>   retarget, moves its windows
```

`hyprnav status` gains `sticks: n`. The hyprnav-shell grid marks stuck frames
with a small pin glyph next to the frame number; that is a separate change in
`~/code/experiments/hyprnav-shell` once the daemon exposes sticks in
`ui_snapshot_grid`.

### Out of scope

Global sticking for non-spawned apps, urgent or notification signalling,
changes to hypr-agent-portal, any edit to `/etc/nixos` before Phase 5.

## Phases

### Phase 0: make the plugin load again

1. Build hyprnav-plugin against the active `hyprland-0.56.2` with
   `hyprnav-plugin-dev-reload` (per AGENTS.md) or the repo's `default.nix`
   with the system nixpkgs. Fix whatever API drift caused the undefined symbol.
2. Load it into a disposable nested Hyprland first (the hyprnav-shell lab in
   `~/code/experiments/hyprnav-shell/scripts/lab.py` is the same 0.56.2 binary
   and can load the `.so`). Only then ask before loading live.
3. Confirm the existing `hyprnav spawn --no-focus rand -- ghostty` places
   the window and preserves focus. This alone removes the worst of the
   current annoyance.

Exit: `hyprctl plugin list` in the lab shows hyprnav-plugin; spawn placement
verified; a note of which symbols changed.

### Phase 1: plugin, `CStickManager`

1. Move the placement code out of `CSpawnManager` into `CStickManager`;
   spawn socket keeps accepting `watch` as an alias of `stick` during the
   transition.
2. Switch the hook to `window.openEarly` with a follow-up on `window.open`.
3. Implement attribution rules 1 to 5 and the root lifetime rule.
4. Unit-testable pieces (PID walk, class ambiguity) go into `common.cpp` with
   a small test binary under `hyprnav-plugin/tests/`.

### Phase 2: daemon persistence and protocol

1. `sticks` table and replay on plugin connect (the plugin sends `hello` when
   its socket comes up; the daemon already knows the socket path).
2. `SpawnStart` writes the stick; `SpawnFinish` no longer unwatches unless
   `--no-stick` was passed.
3. Reaper: every 5 s, for each stick with a dead root, ask the plugin
   `list`; drop roots with no windows and no live descendants.
4. `stick` CLI subcommands and `status` field.

### Phase 3: lab verification

Scenarios in the nested compositor, each recorded with the existing
`record.sh` to `~/code/experiments/hyprnav-shell/recordings/sticking-*.mp4`:

1. `hyprnav spawn --no-focus 105 -- zen-beta`, user stays on 1, agent opens a
   file picker (xdg-desktop-portal-gtk) via injected Ctrl+O: picker lands on 105.
2. Same with a GTK app that forks a helper (`nautilus`, which uses
   `nautilus --gapplication-service` and DBus activation): second window
   lands on 105 through rule 3 or 4.
3. Spawned app opens a second toplevel after the `hyprnav spawn` CLI exited
   (`ghostty -e sh -c 'sleep 2; ghostty &'` style): still sticks.
4. Kill the daemon, restart it, open a new window in the tree: still sticks.
5. Two spawned roots with the same class on 105 and 106: class rule refuses,
   PID rule still places correctly.
6. Plugin unload and reload: sticks replayed, windows re-attributed.

Exit: all six pass; `TESTING.md` in this repo lists commands and outcomes.

### Phase 4: hyprnav-shell integration

Pin glyph on stuck frames in the grid and switcher, from a new `stuck` field
per cell in `ui_snapshot_grid`. Small change, done in the hyprnav-shell
directory only.

### Phase 5: live rollout

With approval: `hyprnav-plugin-dev-reload` on the live compositor, then
update the packaged plugin path in `/etc/nixos/anoromi/hyprland.nix` so the
next `nixos-rebuild switch` picks up the fixed build.

## Risks

- API drift on 0.56.2: `openEarly` timing, `moveToWorkspace` before map, and
  the exact `Desktop::View` suppress flags need checking against the dev
  headers in `/nix/store/*-hyprland-0.56.2-dev/include`. The Portal plugin
  already compiles against them and is the reference.
- Class heuristic false positives: mitigated by the "exactly one root and no
  outside windows" rule; `hyprnav stick release` is the escape hatch.
- DBus-activated single-instance apps: if Nautilus is already running outside
  any stick, a spawned `nautilus` just asks the existing instance to open a
  window, which then lands on the user's workspace. Rule 4 refuses because
  the class has outside windows. Documented limitation; the fix is to spawn
  such apps with a separate `--new-instance` or DBus name, not compositor logic.
- Plugin ABI: the host hash check in `main.cpp` means every Hyprland bump
  needs a rebuild. Phase 0 documents the rebuild path so this stops rotting.
