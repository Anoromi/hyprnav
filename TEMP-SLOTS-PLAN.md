# Temporary slots and the grid palette: plan

Date: 2026-09-20. Implemented the same day; lab-verified (see TEMP-SLOTS-TESTING.md).

## Decisions

| Topic | Decision |
|---|---|
| What | A slot inside an environment's row, allocated on demand on a managed workspace, marked temporary. |
| Numbering | None. Internally allocated from index 1000 upward; snapshot flags `unnumbered: true`; the shell shows a name, never a digit; digit shortcuts skip them. |
| Naming | Default: first window title, else launch command, else "Temp N". Renamable. The name is the handle. |
| Lifetime | Released when its workspace has been empty for 30 s, and on explicit remove. Never while it has windows. Survives daemon restart; empties are dropped on startup reconcile. |
| Ownership | Record who created it: `cli`, `grid`, or an agent client id. Used by the agent integration. |
| Creation | Only through the grid palette (Ctrl+P) or the CLI. No single-key chord. |
| Grid palette | Ctrl+P opens a filterable list of actions for the selected frame, its row, and global actions. Replaces single-key actions except digits, arrows, Enter, Esc. |

## hyprnav daemon and CLI

1. `slot_bindings` gains `temporary INTEGER DEFAULT 0`, `owner TEXT NULL`,
   `empty_since INTEGER NULL`. Migration adds columns.
2. Requests: `slot_temp_create {env, name?, launch_argv?, owner?}` returns the
   resolution; `slot_remove {env, slot}` for any slot (closes nothing, just
   unbinds; the workspace stays if it has windows and the row shows it as
   orphaned until empty); `slot_rename` exists as `slot_name_set`.
3. Reaper (existing 250 ms loop): for temporary slots, track when the
   workspace became empty (from `hyprctl workspaces`); after 30 s unbind and
   delete the record. Managed workspace ids return to the pool.
4. `ui_snapshot_grid` cells gain `temporary`, `unnumbered`, `owner`,
   `empty_for_ms`. Temporary cells sort after numbered ones in the row.
5. CLI: `hyprnav slot temp --env X [--name N] [-- cmd]` (spawns with a stick
   when a command is given, `--no-focus` by default), `hyprnav slot remove
   --env X --slot S|--name N`.
6. Environment title and lock already have requests; add `env_title_set` to
   the CLI as `hyprnav env title`. Exists.

## hyprnav-shell grid

1. Temporary cells: dashed frame, no number badge, name in the number's
   place in Casual, a small hourglass glyph with "empty 12 s" once the reaper
   clock is running.
2. Palette (`grid/Palette.qml`): Ctrl+P toggles. Text field on top, list
   below, Up/Down to pick, Enter runs, Esc closes. Actions carry a scope
   label (frame, roll, everywhere). Fuzzy filter on action title.
3. Actions:
   - frame: Open, Remove slot, Rename slot, Set launch command, Clear launch
     command, Release stick, Close all windows here, Move windows to…
   - roll: New temporary slot, New temporary slot and run…, Rename
     environment, Lock / Unlock environment, Delete environment (only when
     all frames are empty)
   - everywhere: Go to locked environment, Refresh
4. Actions that need text (rename, run, move to) reuse the same text field
   as a second step with a prompt label.
5. Digit shortcuts unchanged for numbered slots; `Shift+L` and the hint line
   at the bottom go away in favour of the palette (the hint becomes "Ctrl+P
   for actions").

## Verification (lab)

1. Create a temp slot from the palette, run kitty into it, see it in the row
   with the window title as its name, digit keys ignore it.
2. Close kitty; after 30 s the frame disappears; `hyprnav status` slot count
   drops; the workspace id is reused by the next temp slot.
3. Restart the daemon with a temp slot holding a window: it is still there.
4. Rename and remove through the palette.
5. Recording: `recordings/temp-slots.mp4`.

## Order

Before the cua integration: the agent slot in
`~/code/experiments/hypr-use/design/HYPRNAV-INTEGRATION-PLAN.md` becomes a
temporary slot with `owner = <agent env or client id>`, which removes the
special environment-per-agent cleanup rules from that plan.
