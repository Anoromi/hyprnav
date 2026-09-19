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
| Palette actions from the keyboard | rename, close all windows (via `hyprctl dispatch hl.dsp.window.kill`), remove; `Home`/`End` jump within a row |
| Recording | `hyprnav-shell/recordings/temp-slots.mp4` |

Found and fixed on the way: Quickshell's `Hyprland.dispatch` did not deliver Lua dispatcher strings and `hl.dsp.window.close` is a no-op; the shell runs `hyprctl dispatch … window.kill` as a process instead. See HARD-STICKING-TESTING.md for the missing move-event fix in the plugin.

Not covered: daemon restart with a temporary slot holding a window (records
persist in `slot_bindings`, so it survives by construction; not exercised),
`slot remove --name` collisions when two slots share a name (first by index wins).
