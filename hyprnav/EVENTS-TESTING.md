# Event socket testing

Evidence for the push-event socket (`events.sock`, protocol version 1) added
on 2026-09-20. Everything ran in the hyprnav-shell lab compositor
(`scripts/lab.py up`, Hyprland 0.56.2, Quickshell 0.3.1), never against the
live session. The daemon under test was the nix dev build
(`nix build --file /tmp/hns-hyprnav-build.nix --impure`).

## Unit tests

`cargo test --lib` inside `nix develop --file /tmp/hns-hyprnav-build.nix`:
62 passed, 0 failed. Five are new:

| Test | What it pins down |
|---|---|
| `runtime_paths::events_socket_sits_beside_the_request_socket` | `events.sock` lives in the same directory as `hyprnav.sock` |
| `events::subscriber_receives_hello_agents_slots_on_connect` | connect burst is `hello`, `agents`, `slots`, each with a `ts_ms` |
| `events::bursts_are_coalesced_into_one_event` | 200 `agents_changed()` calls yield one event, and no second one within 300 ms |
| `events::wedged_subscriber_is_dropped_and_healthy_one_survives` | a subscriber that never drains is dropped, fan-out never blocks, the healthy one keeps every line |
| `events::marking_without_subscribers_does_not_queue_work` | with nobody listening the dirty flags are not kept, so no wakeups accumulate |

## Connect burst

```
$ hyprnav events --once
{"event":"hello","ts_ms":1789874935366,"version":1}
{"agents":[],"event":"agents","ts_ms":1789874935366}
{"event":"slots","ts_ms":1789874935366}
```

## Agent and slot events

Two `hyprnav events` subscribers running at once, driven by the CLI
(`hyprnav agent register|beat|finish`, `hyprnav slot temp|remove`). Both
received the identical 34-line stream.

| Action | Event seen |
|---|---|
| `agent register --id watch-test` | `agents` with the new agent (`state":"idle"`), then `slots` (it carved out temporary slot 1000) |
| `agent beat --state working --action typing` | `agents` with `state":"working"`, `action_count":1`, `last_action":"typing"` |
| 100 `agent beat` calls back to back (~1.3 s) | 25 `agents` events, one per ~50 ms, each carrying the latest `action_count` (4, 8, 12, … 101) — the coalescing window in action |
| `agent finish` | `agents` with `state":"finished"` |
| `slot temp --env agents --name evgrid` | `slots` |
| `slot remove --env agents --name evgrid` | `slots` |

## Slow subscriber

Two subscribers, eight registered agents to make each `agents` event about
2 KB, then `kill -STOP` on the second one and 816 beats spaced 55 ms apart
over 45 s on a held request connection.

| | before | after |
|---|---|---|
| healthy subscriber lines | 11 | 830 |
| stopped subscriber lines | 11 | 11 |

Every beat was answered during the whole window, so the stopped client stalled
neither the daemon nor the other subscriber. After `kill -CONT` the stopped
client was already gone: the daemon had closed its side and the CLI exited on
EOF.

## Idle cost

hyprnav-shell with the grid closed, measured over 20 s windows with
`/proc/<pid>/stat` CPU jiffies and a `/proc` scan for `hyprctl` execs.
"old" is the shell at `HEAD` (1 s `agents_list` poll plus a `hyprctl -j
clients` run per tick, 2 s grid poll); "new" is the event-driven shell.

| Scenario | Shell CPU / 20 s | `hyprctl` spawned by the shell / 20 s |
|---|---|---|
| old shell, no agents | 0.170 s | 13 |
| old shell, one live agent | 0.230 s | 19 |
| new shell, no agents | 0.010 s | 0 |
| new shell, one live agent | 0.000 s | 0 |

The daemon itself went from 0.020 s to 0.010 s idle. With a temporary slot
alive it still uses ~0.10 s per 20 s and spawns ten `hyprctl` runs: that is
the pre-existing 2 s temporary-slot reaper, untouched by this work.

## Shell behaviour

- Registering an agent and beating it with `--target <window address>` put the
  pencil badge and the outline on that window within one frame of the beat,
  with no timer involved. `agent finish` removed both.
- With the grid open, `slot temp --name evgrid` added the frame and
  `slot remove` took it away, both from `slots` events after the 2 s poll was
  deleted.
- Switcher, bar and `run.sh ipc call nav ping` still work; the request socket
  keeps its one-connection-at-a-time behaviour.
- The shell reconnects: it was started while `events.sock` was missing and
  picked the socket up on its own once the daemon was restarted.
