# Hard sticking: test evidence

Date: 2026-09-19. Hyprland 0.56.2, nested in Cage via the hyprnav-shell lab
(`~/code/experiments/hyprnav-shell/scripts/lab.py`). Dev builds:

- plugin: `nix build --file <expr> --impure` of `hyprnav-plugin/default.nix`
  against the system nixpkgs (the same expression `hyprnav-plugin-dev-reload`
  uses, without the live reload step)
- daemon: same for `hyprnav/default.nix`

Driver: `hyprnav-shell/scripts/sticking-test.sh`, keys injected with wtype
through a virtual seat. Recordings: `hyprnav-shell/recordings/sticking.mp4`
(the scenario run) and `hyprnav-shell/recordings/sticking-demo.mp4` (a story
told with a purpose-built GTK4 "agent worker", `hyprnav-shell/scripts/agent-demo`:
it counts down ten seconds and opens an approval dialog. Launched plainly the
dialog lands on the user's notes on workspace 1 and swallows their typing;
launched through `hyprnav spawn --no-focus 4` it stays on workspace 4, the bar
shows a pin, the user visits it via the grid, approves with Enter and returns).

Stock Hyprland 0.56.2 places that dialog on the focused workspace whether or
not it is transient for the agent window (both variants tested).

Observed along the way: a Ghostty "Configuration Errors" GTK dialog, spawned
by the agent tree, was stuck to 101 as well. That is the exact class of window
this feature exists for.

| # | Scenario | Result |
|---|---|---|
| 1 | `hyprnav spawn --no-focus 105 -- ghostty` while the user is on 1 | ghostty on 105, focus stays on 1 |
| 2 | Ctrl+Shift+N in that ghostty after the spawn CLI exited | second window on 105, focus stays on 1 |
| 3 | `kitty &` typed in the stuck ghostty (child process) | kitty on 105, focus stays on 1 |
| 4 | `nautilus --new-window` from the stuck shell | Nautilus on 105 |
| 5 | kill and restart the hyprnav daemon, open another kitty from the tree | still on 105 (replay from SQLite) |
| 6 | `hyprctl plugin unload` then `load`, open another kitty from the tree | still on 105 (plugin re-attributes on replay) |
| 7 | `spawn --no-stick` | first window placed on 106, no persisted stick |
| 8 | `hyprnav stick list`, `hyprnav status` | persisted row and plugin view agree; `sticks: 1` |

10 checks, 10 pass.

## Not covered

- xdg-desktop-portal file choosers: the lab session has no portal daemon, so
  rule 2 (xdg parent) was exercised only through Hyprland's own parent
  tracking, not a real portal dialog. Rule 4 (class heuristic) was not
  triggered by any scenario; it is a fallback and should be watched via
  `hyprnav stick list` classes.
- X11 windows: XWayland is disabled in the lab.
- Rust unit tests: not run, the local cargo has no working linker and the Nix
  derivation has `doCheck = false`.
- Live compositor: not loaded. The live state dir still holds the stale `.so`
  that fails with `undefined symbol: _ZN8CMonitor17activeWorkspaceIDEv`.

## Live rollout, when approved

```sh
# builds against the active Hyprland and swaps the dev plugin in the live compositor
hyprnav-plugin-dev-reload
# do NOT run hyprnav-dev-build blindly: it kills the running daemon and grid server;
# run it when a restart of the switcher is acceptable
hyprnav-dev-build
hyprctl plugin list      # expect hyprnav-plugin
hyprnav status           # expect a "sticks" field
```

## Follow-up fix (2026-09-20)

Windows placed at `window.openEarly` reached IPC clients with the
pre-placement workspace in `openwindow`, and no move event followed, so
Quickshell (and anything else reading `.socket2.sock`) kept them on the wrong
workspace: wrong thumbnails, wrong window lists. The plugin now posts
`movewindow` and `movewindowv2` after placement at `window.open`. Verified in
the lab: Quickshell reports the spawned kitty on workspace 103 and the grid
thumbnail lands on the right frame.
