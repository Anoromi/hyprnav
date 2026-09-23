# Temporary slots: test evidence

Date: 2026-09-20, lab compositor (hyprnav-shell `scripts/lab.py`), dev daemon
built with the system nixpkgs, hyprnav-plugin loaded.

| Check | Result |
|---|---|
| `hyprnav slot temp --env shell --name review` | slot 1000 on managed workspace 103, `temporary: true`, owner `cli` |
| `hyprnav slot temp --env agents -- kitty` | slot allocated, kitty spawned and stuck on the new workspace, focus unchanged |
| `hyprnav slot temps` | lists env, slot, workspace, owner, `empty_since` |
| Empty slot timer | `empty_since` set within 2 s of the workspace emptying; slot released at 30 s; workspace id returned to the managed pool (next temp reused 103) |
| Grid | temporary frames dashed, badge shows live window title (falls back to "Temp N"), caption "temporary, by cli|grid", hourglass with "empty N s" while the timer runs; digits skip them |
| Palette (Ctrl+P) | filter "temp" -> New temporary slot creates one in the selected roll with owner `grid`; "remove" on a temporary frame removes it |
| Switcher and bar | unnumbered slots show no digit; bar strip skips them |
| Row ownership (2026-09-23) | a temporary slot shows only at the end of its own environment's row; `shell.docs` inherits `shell`'s numbered slots 1 and 3 but not its temp on ws 103, and keeps its own temp on ws 104 last |
| MRU switcher (2026-09-23) | `ui_snapshot_switcher` returns numbered cards only; the two temp workspaces, both with a live kitty, are absent |
| Palette actions from the keyboard | rename, close all windows (via `hyprctl dispatch hl.dsp.window.kill`), remove; `Home`/`End` jump within a row |
| Recording | `hyprnav-shell/recordings/temp-slots.mp4` |

Found and fixed on the way: Quickshell's `Hyprland.dispatch` did not deliver Lua dispatcher strings and `hl.dsp.window.close` is a no-op; the shell runs `hyprctl dispatch … window.kill` as a process instead. See HARD-STICKING-TESTING.md for the missing move-event fix in the plugin.

Not covered: daemon restart with a temporary slot holding a window (records
persist in `slot_bindings`, so it survives by construction; not exercised),
`slot remove --name` collisions when two slots share a name (first by index wins).

## Where a temporary slot shows (2026-09-23)

A temporary slot belongs to the environment that created it and shows in one
place: after the numbered frames of that environment's own grid row. Numbered
slots keep inheriting down the environment chain; temporary ones do not.

Implemented in `hyprnav/src/server.rs`:

- `slot_indexes_for_environment` drops any binding whose `env_id` is not the
  row's own environment and whose `slot_index >= TEMP_SLOT_START`, so an
  ancestor's temp never enters a descendant row. Ascending sort already puts a
  row's own temps last.
- `resolve_slot_effective_from_bindings` stops walking the chain for a
  temporary index, so nothing can resolve an ancestor's temp under a child
  environment even if called directly.
- `build_switcher_snapshot` filters the live workspace cards to
  `slot_index < TEMP_SLOT_START` before the initial selection is computed, and
  skips temporary grid cells when it appends browser slots. That is the
  cheapest place: one pass over a list the daemon already has, and every
  switcher client gets it.
- `db.rs::resolve_slot_effective` is deliberately untouched. It answers
  explicit "resolve env X slot N" questions (`workspace goto --env … --slot …`,
  spawn and assignment paths) rather than building a row, so narrowing it would
  change navigation semantics for no visible gain.

Unit tests next to the existing inheritance ones:
`slot_indexes_for_environment_exclude_ancestor_temporary_slots`,
`resolve_slot_effective_from_bindings_does_not_inherit_temporary_slots`,
`grid_snapshot_keeps_own_temp_last_and_drops_it_from_the_child_row`.

`shell/switcher/Switcher.qml` in hyprnav-shell also drops cards whose grid cell
is `unnumbered`/`temporary`, from both the drawn row and the cycling order, so
an older daemon cannot put a temp under Alt-Tab.
