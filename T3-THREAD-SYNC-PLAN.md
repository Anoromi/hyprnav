# T3 Code follows hyprnav thread switches: plan

Status: approved 2026-09-27 with all recommendations (D1 event, D2 per device, D3 ignore, D4 ignore, D5 registry rule now, D6 Electron only, D7 stay put, D8 keep, D9 fix). The event is named `locked`, not `lock`. Implementation in progress. Written 2026-09-27 from hyprnav `main` @ 36efdc4,
hyprnav-shell @ e1c1bb7, T3 `modernize/upstream-main-20260924` @ 17e9fec8dc.

## 1. Current state (verified in code)

### 1.1 How T3 maps onto hyprnav environments
- Ids are built in the desktop main process, `apps/desktop/src/hyprnav/HyprnavEnvironment.ts:132-148`:
  `p.<sha256(realpath projectRoot)[:12]>` → `….w.<sha256(worktreePath ?? projectRoot)[:12]>` → `….t.<threadId>`.
  `lockEnvId = threadEnvId ?? worktreeEnvId`. Parent/child is pure dotted-prefix (`hyprnav/src/db.rs:1075-1086`).
- **Who creates them, and when:** `HyprnavRuntimeOrchestrator` (`apps/web/src/components/HyprnavRuntimeOrchestrator.tsx`),
  mounted per open thread by `ThreadRouteView.tsx:213`, calls `syncHyprnavEnvironment` with `lock: true`
  (`:93`). The desktop turns that into one atomic `hyprnav batch --stdin` (env_ensure for touched scopes, plus
  the thread scope whenever `lock && threadId`, `HyprnavEnvironment.ts:516`), then `hyprnav lock <lockEnvId>` (`:583-584`).
  So **a thread gets a hyprnav env only once it has been opened in T3**, and opening it in T3 already moves the lock:
  the T3→hyprnav direction exists.
- Only threads of the **primary** T3 environment are published (`apps/web/src/hyprnavRuntime.ts:289-312`).
- The user's bindings (`~/.t3/userdata/client-settings.json`): worktree slot 1 Terminal (managed), worktree slot 2
  "T3code", **absolute workspace 2**, action `nothing`; worktree slot 3 Editor; thread slot 5 "testing terminal";
  thread slot 8 Corkdiff (managed). So "T3 is on workspace 2" is a **worktree-scope frame, the same physical ws 2
  in every worktree env**. Each thread row in the grid shows it as a `shared` cell owned by its worktree
  (`protocol.rs:88-97`, test `server.rs:3352`). T3 is one window; switching rows never moves it.
- Bug found: when a Corkdiff binding exists, the orchestrator re-syncs every 4 min
  (`hyprnavRuntime.ts:25,341-343`, loop at `HyprnavRuntimeOrchestrator.tsx:126-128`) **with `lock: true` again**.
  A lock the user moved in hyprnav is silently taken back by T3 up to 4 min later.

### 1.2 What moves the lock in the daemon
The lock is one row in `global_state` (`db.rs:10,714-751`). It changes in six places:
| Site | Lock becomes |
|---|---|
| `lock_set` / `lock_clear`, direct or inside `batch_mutate` (`server.rs:779-782,1634-1670`) | the given env / none |
| `workspace_goto {env,slot}` (`server.rs:797-821`): `record_environment_focus(resolved_env)`, which **also sets the lock** (`db.rs:753-766`) | the requested env, i.e. the grid row's **leaf** (thread) |
| `workspace_goto_physical` (`server.rs:848-862`) | the **concrete owner** of that ws (the ancestor for shared frames); none if ambiguous |
| Hyprland `workspacev2`/`focusedmonv2` thread (`server.rs:339-403`), only when the ws id changes | concrete owner; none if ambiguous (`server.rs:2078-2119`) |
| `env_delete` of the locked env (`db.rs:~708`) | none |

Shell callers: grid `activate()` → `gotoSlot(cell.environment_id, slot)` (`hyprnav-shell/shell/grid/Grid.qml:114-120`;
`environment_id` is the row leaf); grid lock toggle (`Grid.qml:122-125`); bar right-click lock (`bar/Bar.qml:195-200`);
bar/switcher left-click `gotoSlot`/`gotoPhysical` (`Bar.qml:203-204`, `switcher/Switcher.qml:135-136`).
So "switch from thread A to thread B in hyprnav" today means **lock := `p.X.w.Y.t.<B>`**, from the grid (T3code cell
or any B frame), a lock toggle, or a plain Hyprland keybind to one of B's own frames (e.g. B's Corkdiff ws).
Workspace 2 is bound by every worktree env, so landing on ws 2 is ambiguous and **never** moves the lock by itself.
Going to a worktree's own terminal (unique managed ws) moves the lock to the **worktree env**, an ancestor of A.

### 1.3 What the event socket carries today
`hyprnav/src/events.rs:234-255`, connect burst `:279-283`. Actual lines:
```
{"event":"hello","ts_ms":1789874935366,"version":1}
{"agents":[{"agent_id":…,"label":…,"client":…,"pid":…,"environment_id":…,"slot_index":1000,"workspace_id":…,
  "state":"idle|working|waiting_for_user|finished","last_beat_ms":…,"action_count":…,"last_action":…,
  "current_target":"0x…","attached_windows":[…],"created_at_ms":…,"thread_id":…,"thread_environment_id":…}],
 "event":"agents","ts_ms":…}
{"event":"slots","ts_ms":…}
```
`slots` has **no payload**. Lock changes only fire it because they are classed as SLOTS (`server.rs:631-676`) or,
for the focus thread, via `slots_changed()` at `:382`. Nothing says "the lock is now X". A subscriber would have to
call `status_get` (`server.rs:681-689`, returns `locked_environment_id`) after every `slots`. That event also fires
for the 2 s temp reaper, titles and slot edits, at up to 20/s.

### 1.4 T3's side of the socket
- `apps/server/src/hyprnavEvents.ts`: one ref-counted upstream connection per server process. It forwards
  only `agents|slots` (`:94-96,203`) and replays the last line of each to late subscribers (`:116,128`).
- `apps/server/src/hyprnavRoutes.ts:242-260`: loopback-only SSE at `/api/hyprnav/events`. It sends
  `event: <name>` for anything the broker forwards.
- `apps/web/src/desktopAgentsStore.ts:~99-195`: the only consumer, one `EventSource` (`:140`), listens to `agents`
  and `status`. It opens only while an agents view is mounted. The Electron renderer uses it too (same URL through
  `resolvePrimaryEnvironmentHttpUrl`), so there is no separate IPC path for events.
- Agents: T3 exports `T3CODE_THREAD_ID` + `T3CODE_ENVIRONMENT_ID = mcpSession.environmentId`
  (`apps/server/src/provider/Layers/ClaudeAdapter.ts:4955-4956`; Codex via launch args). This is a T3
  `ScopedThreadRef`, not a hyprnav env. The cua bridge registers with `env: HYPRNAV_ENV || null`
  (`hypr-use/mcp/unified/hyprnav-bridge.mjs:61-64`). T3 does not set `HYPRNAV_ENV`, so the daemon puts agents
  in the **canonical cwd path env** (`server.rs:1068-1075`), not in `p.….t.<id>`. The MCP never locks or gotos.

### 1.5 Programmatic thread navigation in the web UI
TanStack Router route `/$environmentId/$threadId` (`apps/web/src/routes/_chat.$environmentId.$threadId.tsx`).
Canonical call: `navigate({ to: "/$environmentId/$threadId", params: buildThreadRouteParams(ref) })`
(`apps/web/src/threadRoutes.ts:42`, used in `AppSidebarLayout.tsx:306-313` `selectThread`, `:398-411` recent
switcher, `ThreadNotificationCoordinator.tsx:190,216`). Thread lookup: `readThreadShell(ref)` / `useThreadShell`
(`apps/web/src/state/entities.ts:99,182`). Shells carry `archivedAt`. Primary env: `usePrimaryEnvironmentId()`
(`state/environments.ts:58`). Precedent for main→renderer pushes: `THREAD_SWITCHER_ACTION_CHANNEL`
(`apps/desktop/src/window/DesktopWindow.ts:658-662`, `preload.ts:258-273`). Desktop has one main window
(`electronWindow.currentMainOrFirst`).

### 1.6 Branch situation
The hyprnav work on the current branch comes from squashed re-applies on 2026-09-24: d71f33c62e (contracts), 9971f0320b
(desktop + Corkdiff), bcb6fdac4e (orchestration), a9b8f60da0 (web settings/sync), e5957c94d0 (agents, SSE,
frames, i.e. `apps/server/src/hyprnav*.ts`). **8fea826889 "Publish hyprnav titles for project and worktree scopes"
is only on `origin/main`, not on the current branch.** On this branch only the thread env gets a title
(`HyprnavEnvironment.ts:530`). It has to be ported first (cherry-pick, touches `HyprnavEnvironment.ts`, `ipc.ts`,
`hyprnavRuntime.ts`, the settings panel). The lab's default T3 tree `~/code/stolen/t3code-lab` is on `hns-lab` @
2f61603e30, which is older still.

## 2. Design options

**(A) Recommended: the daemon emits a `locked` event, T3 server forwards it on the existing SSE, and the web renderer navigates.**
Daemon: new `locked` event, sent only when the value changes and included in the connect burst, carrying
`cause` + `origin`. T3 server: add `"locked"` to the forwarded set (a one-line change plus tests). Web: one shared
EventSource module, with a `HyprnavLockFollower` mounted in `AppSidebarLayout` that maps env id → thread ref and calls
`navigate`. Pros: reuses the broker and SSE, works the same in Electron and in the loopback web build (t3-web-lab),
navigation stays in the router, which is the one place that knows routes and drafts. No new IPC. Cons: needs a daemon
change, and a renderer with no connection to its server misses switches (it resyncs on reconnect from the replayed
baseline).

(A0) Stopgap with no daemon change: on each `slots` event the server runs `hyprnav status` (debounced 150 ms) and
synthesizes the lock line. It spawns a CLI on every slot event, cannot tell who moved the lock (no loop guard beyond
value comparison), and is racy with the focus thread. Only worth it if the daemon change is blocked.

(B) Desktop main process subscribes to `events.sock` itself and pushes `desktop:hyprnav-lock` over IPC (like the
thread switcher channel). This is Electron-only, duplicates the broker, and the browser lab would not follow. Its one
real advantage, raising the window, is not wanted (see below).

(C) Daemon webhook or exec on lock change, or (D) hyprnav-shell calling T3 via `qs ipc`. Rejected. T3 has no
thread deep link or CLI to call. (D) would miss CLI locks, keybinds to a thread's frame and the focus thread, and it
couples the shell to T3.

### Semantics under (A)
- **Key on the lock, not on the active workspace.** Every way the user "picks thread B" in §1.2 ends with lock = B's
  env. The active workspace is ambiguous for ws 2, the frame the user actually watches T3 on. The focus thread is
  already folded in, because it writes the lock.
- **Mapping rule (follower):**
  1. Ignore `cause:"snapshot"` (the connect burst or server replay is a baseline, not a request) and `origin:"t3code"`.
  2. `null` → ignore (unlock never navigates).
  3. `p.<h>.w.<h>.t.<threadId>` → ref `{environmentId: primary, threadId}`. No-op if it is the current route.
     Ignore if `readThreadShell` is null (deleted thread, stale env; log once) or `archivedAt` is set (decision D4).
  4. Worktree/project env (`p.<h>[.w.<h>]`): no-op if it is an **ancestor of the thread on screen** (the user went to
     the worktree terminal). Otherwise decision D3: ignore, or open the most recently active non-archived thread
     whose `worktreePath` (or project root) equals the event's `environment.cwd`.
  5. Any other env (agent cwd-path env, `agents`, the user's own envs): if a live agent in the agents snapshot has
     `environment_id == locked` and `thread_id` + `thread_environment_id`, follow that thread (decision D5). Else ignore.
- **Temp slots.** A grid temp slot created on a thread row belongs to that thread env, so rule 3 covers it. Agent temp
  slots live in the agent's cwd env, so rule 5 covers them. Temp slots inherit nothing from ancestors (`server.rs:3182`).
- **Shared frames.** Clicking B's T3code cell sends `workspace_goto{env:B,slot:2}`, which locks B (leaf, not owner),
  so it works. `workspace_goto_physical(2)` from the bar is ambiguous, leaves the lock alone, and T3 stays put. That is
  correct.
- **Debounce:** 150 ms trailing on the follower, with the latest `seq` winning, so a fast A→B→C lands on C with one navigation.
- **Focus:** navigate silently, never raise or focus the T3 window. hyprnav already chose the workspace. If the user
  locked B from their terminal ws, T3 is simply on B the next time they look at ws 2.
- **Multi-window:** Electron has one main window, so there is no issue. Several browser tabs on the loopback web build would all follow.
  Default the follower on for Electron and off for plain web (decision D6).
- **Startup:** the first lock seen after (re)connect is a baseline and never navigates, so T3 does not jump on launch (D7).
- **Reverse direction (T3 → hyprnav) and loop prevention.** It already exists (`lock:true`). Keep it and fix it:
  (i) T3's `lock`/`batch` calls pass `--origin t3code`, and the follower drops events with that origin.
  (ii) A navigation *caused by the follower* publishes with `lock:false`. The follower records `followedThreadKey`
  before `navigate`, and the orchestrator reads and clears it. Otherwise, during A→B→C, T3's late `lock B` could
  land after the user's `lock C` and undo it.
  (iii) The 4-min credential refresh uses `lock:false`. Only the first publish per `requestKey` may lock.
  (iv) The daemon emits `lock` only on change, so a redundant `lock B` while B is locked is silent. With these,
  hyprnav→T3→hyprnav stops after one hop.
  If both sides race, the last writer at the daemon wins and both sides converge on it: T3 ignores its own echo,
  and the shell redraws from `slots`.

## 3. Concrete changes

### 3.1 hyprnav daemon (`hyprnav/`)
- `src/events.rs`: add `locked_line(seq, locked, previous, cause, origin, environment)`. Add
  `EventBus::lock_changed(...)`, which fans out **immediately** (lock changes are rare and must not be merged away)
  under a `seq: u64` kept in `BusState`. `start_event_server` takes a second snapshot closure for the burst:
  `hello, agents, slots, locked(cause:"snapshot")`. `hello.version` stays 1 (additive; the shell `Hyprnav.qml:124-134`
  and T3 `hyprnavEvents.ts:203` already ignore unknown events).
- `src/server.rs`: a lock watch. `handle_request` (`:599-617`) reads `locked_environment()` before and after any
  SLOTS-class request. If the value differs, it calls `lock_changed(cause = request op name, origin = request.origin)`.
  The hypr event thread (`:370-383`) does the same with `cause:"focus"`, `origin:null`. Environment details
  come from `store` (title, cwd/source path, `environment_chain`).
- `src/protocol.rs`: `#[serde(default)] origin: Option<String>` on `LockSet`, `LockClear`, `WorkspaceGoto`,
  `WorkspaceGotoPhysical`, `BatchMutate`. `src/cli.rs`: global `--origin <tag>` for `lock`, `unlock`, `goto`, `batch`.
- Tests: burst now has 4 lines (`events.rs:325`); lock event only on change; `workspace_goto` into a thread row emits
  `cause:"workspace_goto"`; focus into an ambiguous ws emits nothing; `origin` round-trips.
  Update `EVENTS-TESTING.md` and the module doc (`events.rs:1-21`).
- Event schema:
```
{"event":"locked","ts_ms":…,"seq":42,
 "locked_environment_id":"p.3f…a1.w.9c…07.t.thr_B" | null,
 "previous_environment_id":"p.3f…a1.w.9c…07.t.thr_A" | null,
 "cause":"snapshot|lock_set|lock_clear|workspace_goto|workspace_goto_physical|focus|env_delete|batch_mutate",
 "origin":"t3code" | "hyprnav-shell" | null,
 "environment":{"title":"Other","cwd":"/home/…/worktree","chain":["p.3f…a1","p.3f…a1.w.9c…07","p.3f…a1.w.9c…07.t.thr_B"]} | null}
```

### 3.2 hyprnav-shell (optional, cosmetic)
`shell/services/Hyprnav.qml:182-185`: send `origin:"hyprnav-shell"` on `workspace_goto`, `lock_set`, `lock_clear`.
This is for logs only; T3 does not need it.

### 3.3 T3 server
`apps/server/src/hyprnavEvents.ts:40,94-96`: forward `"locked"`. The `latest` replay (`:116,128`) then hands every new SSE client the baseline.
Tests in `hyprnavRoutes.test.ts` / a broker test with a fake socket (`T3CODE_HYPRNAV_EVENTS_SOCKET`).

### 3.4 T3 desktop
`apps/desktop/src/hyprnav/HyprnavEnvironment.ts`: `run(["--origin","t3code","lock",id])` at `:338,584`, and the same for
`batch` (`:578`). `DesktopHyprnavSyncInput.lock` keeps its meaning. Tests in `HyprnavEnvironment.test.ts`.
Also port 8fea826889 first.

### 3.5 T3 contracts / web
- `packages/contracts/src/hyprnav.ts`: `parseHyprnavEnvironmentId(id) → {scope:"thread",threadId} | {scope:"worktree"|"project"} | null`
  (regex `^p\.[0-9a-f]{12}(\.w\.[0-9a-f]{12}(\.t\.(.+))?)?$`). Add a round-trip test against
  `buildHyprnavEnvironmentIds` in the desktop tests. Add `HyprnavLockEvent` type (the schema above).
- New `apps/web/src/hyprnavEventStream.ts`: the one shared EventSource, extracted from `desktopAgentsStore.ts:~99-195`
  (status/agents/lock listeners, ref-counted). `desktopAgentsStore` becomes a consumer.
- New `apps/web/src/hyprnavLockFollower.ts`: pure `decideHyprnavFollow({event, baselineSeq, currentRouteRef,
  primaryEnvironmentId, readThreadShell, agents, currentThreadEnvChain}) → {kind:"navigate",ref}|{kind:"ignore",reason}`,
  unit-tested for every rule in §2. Also exports `markFollowedThread` / `consumeFollowedThread(ref)`.
- New `apps/web/src/components/HyprnavLockFollower.tsx`: mount next to `RecentThreadSwitcherControl` in
  `AppSidebarLayout.tsx:620`. It debounces for 150 ms, calls `markFollowedThread(ref)`, then
  `navigate({to:"/$environmentId/$threadId", params: buildThreadRouteParams(ref), replace:false})`.
- `HyprnavRuntimeOrchestrator.tsx:85-94`: `lock: !consumeFollowedThread(threadRef) && setting.publishLock`, and
  force `lock:false` on the refresh iterations (`:126-128`).
- Settings (`packages/contracts/src/settings.ts` near `:410`, UI `apps/web/src/routes/settings.hyprnav.tsx`):
  client (per-device) settings `hyprnavFollowLock: boolean` (default true in Electron) and
  `hyprnavPublishLock: boolean` (default true, which is today's behaviour). Per-project alternative: decision D2.

### 3.6 hypr-use MCP
No change needed. Agents never lock (verified). Optional: export `HYPRNAV_ENV=<thread env id>` from T3 to the
MCP so agent temp slots land on the thread's row. Then rule 5 is unnecessary, and grid "go to agent" locks the thread
directly. This needs the desktop-computed id on the server side, so it is a follow-up (D5).

## 4. Verification
Lab only, never the live session. Point the lab at the implementing tree:
`T3CODE_REPO=~/code/stolen/t3code scripts/t3-lab.sh up` (Electron) and `scripts/t3-web-lab.sh up` (web). Use the dev
hyprnav build (`HNS_HYPRNAV_BIN`), or `hypr-lab up --profile electron --rev hyprnav=<sha> t3code=<sha>` / `--profile full`.
1. Daemon: `hyprnav events` in one terminal. Then `hyprnav lock p.x.w.b.t`, `hyprnav goto --env p.x.w.a.t --slot 2`,
   a keybind to ws 8 (thread-owned), `hyprctl dispatch workspace 2` (ambiguous: no event), `lock` of the same env
   twice (one event). Check `cause`, `origin` and `seq` on each.
2. T3: create project → worktree → threads A and B in the lab T3 and open each once, so their envs exist (compare
   `hyprnav grid` rows with `grid-merged-demo.sh seed`). Watch `curl -N localhost:<port>/api/hyprnav/events` for
   `event: locked`.
3. Behaviour matrix, each checked with the T3 route (`preview_evaluate location.pathname` in web lab, window title in Electron):
   grid T3code cell on B's row → T3 on B; grid lock toggle on A → A; bar right-click; B's Corkdiff keybind → B;
   worktree terminal of the current worktree → no navigation; archived thread's row → no navigation; rapid
   A→B→A→B in the grid within 300 ms → one final navigation, lock ends on B, no ping-pong (look at the `hyprnav events`
   log for extra `origin:"t3code"` lock lines); select C in T3's sidebar → lock C with `origin:"t3code"`, T3 does not
   re-navigate; wait out a refresh cycle (temporarily set `HYPRNAV_CREDENTIAL_REFRESH_DELAY_MS` low) → lock not stolen.
4. Recording: `scripts/record.sh start t3-thread-follow`. Show the grid open, B's T3code cell picked, T3 switching
   to B, then A via lock toggle, then a T3 sidebar pick with the grid's lock icon following. Add captions via
   `run.sh ipc call caption display`. Stop, and share the mp4 from `hyprnav-shell/recordings/`.
5. Live session, read-only, before rollout: `hyprnav status`, `hyprnav grid | jq` (confirm thread rows exist, that ws 2
   is bound by several worktree envs so it is ambiguous, and which envs are stale), and `hyprnav events --once` to
   confirm the daemon version. No `lock`, `goto` or restarts.

## 5. Effort
Daemon lock event, origin plumbing and tests: 0.5–1 day. Port 8fea826889 to the current branch: 1 h. T3 server forward: 1 h.
Desktop origin: 1 h. Web shared stream, follower logic, component, orchestrator fixes, settings and tests: 1–1.5 days.
Lab verification and recording: 0.5 day. **Total about 3 days**. A0 instead of the daemon change saves about half a day.

## 6. Open decisions for the user
- D1. Accept a daemon protocol addition (`locked` event + `origin`), or ship the A0 stopgap first?
- D2. Follow toggle per device (client setting, recommended; the lock is global) or per project (new
  `ProjectHyprnavSettings` field + persistence migration)?
- D3. Lock on a worktree/project env that is not an ancestor of the current thread: ignore (recommended for v1), or open that
  worktree's most recent thread?
- D4. Archived or deleted thread locked in hyprnav: ignore silently (recommended), toast, or open the archived thread?
  Should T3 also `env_delete` hyprnav envs of deleted threads?
- D5. Agent-driven: follow a lock on an agent's env to the agent's thread (rule 5), or export `HYPRNAV_ENV` so agents live
  on the thread row (cleaner, bigger)?
- D6. Loopback web build (browser tabs): follow too, or Electron only by default?
- D7. On T3 launch with a thread already locked: stay put (recommended) or open the locked thread?
- D8. Keep T3 → hyprnav locking on thread selection (today's behaviour, now loop-safe), or make it opt-in?
- D9. Is the 4-min refresh re-locking a bug to fix now regardless of this feature? (Recommended: yes.)
