# Live window video: efficient design

Status: Phase B built and verified in the lab, 2026-09-20. README "Window frames" now
describes what exists; this document is the design behind it and the record of what the
experiments settled. Phase C (T3 WebCodecs player) and Phase D (T2 zero-copy) are open.
The MJPEG path stays as the fallback format.

Two things §1 got wrong, found in the lab and fixed in the build:

* The damage hook has to be on `CWLSurfaceResource::commitState`, not on
  `CHyprRenderer::damageWindow`. The renderer's damage paths are gated on visibility --
  zero `damageSurface` calls in ten seconds for a window on a hidden workspace, ten in as
  many seconds once it is on screen -- so they never fire for the case the feature exists
  for. A client painting commits either way. (The class is `Render::IHyprRenderer` in
  0.56, not `CHyprRenderer`.)
* `render_unfocused` is not enough to bootstrap. A toolkit needs more than one frame
  callback to turn a changed label into a committed buffer, and on an idle headless output
  nothing renders at all, so `render_unfocused` produces no callbacks either. The daemon
  allows a bounded burst of at most three unprompted captures, reset by every damage
  report; a window that is really still settles to zero.

A third detail §2 did not anticipate: ffmpeg holds a picture until the next one arrives
(one raw frame in, zero bytes out; two in, both out), so the pump writes the last frame
again when the window goes quiet.

## Goal

A gesture-free live view of any Hyprland window (typically an agent's window on a hidden
workspace) for T3 Code's mini player, the shell, and remote dashboards, at the lowest
steady-state cost: zero when the window is static, GPU-bound when it moves, one pipeline per
window no matter how many watchers, and cheap enough on the wire for Tailscale.

Non-goals: replacing the click-to-watch PipeWire portal (that stays the full-size, lowest-latency
path), audio, recording to disk.

## Measured baseline (current MJPEG path, lab, 2026-09-20)

| case | cost |
|---|---|
| hidden countdown, 8 fps, 640 px wide, JPEG q60 | 0.7 % of a core, 60–300 KB/s |
| static hidden window | 0.2 % of a core (render + pixel compare, no encode), 0 B/s |
| GPU encoders available (AMD Strix Halo, VA-API) | AV1, HEVC, H.264 all verified working from this user |

## Pipeline

```
compositor ──toplevel-export (dmabuf, wait-for-damage)──► hyprnav-capture ──fd or raw──► encoder
   ▲ render_unfocused while watched                         │ VPP scale on GPU              │ AV1 (VA-API)
   └───────────────── daemon sets/unsets ──────────────────┘                                ▼
                                                     daemon: GOP cache + fan-out ──frames.sock──► clients
                                                                                         (T3 route pass-through)
                                                                                         browser: WebCodecs → canvas
```

### 1. Capture: change-driven via the plugin, shm first

Experiment E1 (2026-09-20) killed the original premise: `copy(buffer, ignore_damage = 0)` is
released by the next **monitor commit**, not by the window's damage (`ToplevelExport.cpp`
`onOutputCommit` has no per-window check). A hidden window therefore starves on a quiet screen
and gets captured on every unrelated repaint on a busy one. `render_unfocused` keeps the app
painting but does not influence delivery. So:

* Protocol stays `hyprland-toplevel-export-v1` with `ignore_damage = 1` (synchronous standalone
  render; proven for hidden windows).
* Change detection moves into the hyprnav **plugin**, which runs inside the compositor: hook
  `CHyprRenderer::damageWindow(PHLWINDOW, …)` (CFunctionHook, as the existing StickManager hooks
  do), and for windows the daemon has marked "watched" post a `window_damaged {address}` event to
  the daemon over the existing plugin↔daemon channel, coalesced to at most one per
  `1000/max_fps` ms per window. Only watched windows generate traffic; unwatched cost one map
  lookup per damage call.
* Daemon: on `window_damaged` for a watched address, tell the helper to capture that window once
  (`{"op":"capture","addr":…}`). No polling loop. A static window: zero renders, zero copies,
  zero encodes. The helper's identical-pixel dedupe stays as a safety net (damage without visual
  change, e.g. cursor blink in a terminal) but is no longer the primary mechanism.
* Keep the app painting while hidden: hidden clients only repaint on frame callbacks. The
  standalone render sends them (surface feedback unblocked when the window is not visible), so
  a moving window sustains itself once capture starts; to bootstrap and to survive quiet spells
  the daemon sets `render_unfocused = true` on watched windows (frame callbacks at
  `misc:render_unfocused_fps`, 15) and unsets it when the last watcher leaves. Verified harmless.
* Fallback when the plugin is not loaded (plain Hyprland): today's paced loop at `max_fps` with
  dedupe (0.6 % of a core per moving window, measured).
* Visible windows: `ignore_damage = 0` is strictly better there (11 renders for 11 changes vs
  79 in E1); optional optimisation, not required.
* Buffers: shm readback + integer box downscale as today (2 % of a core per window at 640 wide,
  E2). dmabuf capture is verified feasible (E4: `AR24`, linear modifier, GBM on renderD128, hidden
  windows render) and is the T2 upgrade when several windows are watched at once.
* Scaled height aligned to 16 (VAAPI encoders pad `640x365 → 640x368`); clients size the canvas
  from the decoded frame, not the record header.

### 2. Scale and encode: GPU, zero-copy

* Import the dmabuf into VA-API (`vaCreateSurfaces` with `VASurfaceAttribExternalBuffers` /
  DRM PRIME 2), scale with the VA-API video-processing pipeline to the requested width (default
  640, tiers 320/640/960/native), output NV12, encode with `av1_vaapi`-class encoder
  (VAProfileAV1Profile0, low-delay, CQP ~30 or CBR 400–800 kbps at 640 wide).
* Codec is configurable, never hard-wired:
  * Daemon config (`~/.config/hyprnav/config.toml`, section `[frames]`): `encoder = "vaapi" |
    "software" | "nvenc" | "auto"`, `vaapi_device`, `codecs = ["av1","h264","hevc","vp9"]` (the
    allow-list in preference order), `default_width`, `max_pipelines`, and per-codec overrides
    (`[frames.codec.av1] qp = 30`, `bitrate`, `gop`, `low_power`). `auto` probes the ffmpeg
    encoder list once at startup and keeps what actually initialises on this GPU.
  * Client negotiation: the request line carries `"codecs": ["av1","h264","mjpeg"]` in the
    client's preference order (the browser fills it from `VideoDecoder.isConfigSupported`); the
    daemon picks the first one it is configured and able to encode and reports it in the first
    record (`flags = CONFIG`, codec string in the payload header). `format` stays as a single-value
    shorthand for CLI use.
  * Pipelines are keyed by `(address, codec, width)`; two clients that agree share one encoder,
    two that disagree get two. `max_pipelines` bounds the total; beyond it the daemon answers
    with the nearest existing pipeline's codec if the client accepts it, else `busy`.
  * Defaults on this machine: AV1 first (Firefox and Chromium both decode it, VCN encodes it
    fastest of the three), H.264 second (universal decode), HEVC only when a client asks (Firefox
    cannot decode it), MJPEG last as the no-WebCodecs fallback.
* GOP: `-g 16 -bf 0 -async_depth 1` are mandatory (E2: `-async_depth 1` cuts encoder latency
  from 136 ms to 2 ms; `-bf 0` alone does not). Quality via `-q:v` (`-qp` is ignored by the VAAPI
  encoders). `-low_power` has no AV1 entrypoint on this GPU. IDR on demand when a watcher joins
  is not available through ffmpeg; the GOP cache covers joins instead.
* Implementation tiers, cheapest first, chosen by measurement (E2):
  * T1 `ffmpeg` child per watched window: `-f rawvideo` on stdin (helper does shm readback and
    integer downscale as today), `-vf format=nv12,hwupload -c:v av1_vaapi`, raw OBU/IVF on
    stdout. No dmabuf, one CPU copy + upload per frame, keyframe on demand via `-force_key_frames`
    expression is not possible: rely on 2 s GOP + GOP cache (below). ~1 day total.
  * T2 GStreamer in the helper: `appsrc` (dmabuf) → `vapostproc` → `vaav1enc` → `appsink`,
    force-key-unit events on join. Zero-copy, keyframe on demand. +1–2 days, adds gst dependency.
  * T3 libva directly in the helper. Zero-copy, smallest footprint, most code (AV1 encode
    parameter structures). Only if T2's dependency is unacceptable.
  Recommendation: build T1 first behind the same socket contract, measure, then decide T2.

### 3. Daemon: one pipeline per window, GOP cache, fan-out

* `FrameHub` (exists) gains per-address `Pipeline { capture, encoder, gop_cache, subscribers }`.
* GOP cache: sequence header + last IDR + following frames. A joining client receives the cache
  as a burst first, then live frames; it is decodable immediately without waiting for the next IDR.
  Bounded by GOP length (2 s at ≤15 fps = ≤30 small frames).
* Fan-out: latest-wins is wrong for video (P-frames depend on predecessors). Per client: bounded
  queue of whole frames; on overflow drop the client's queue back to the next IDR and mark
  "resync" (send the cached IDR). Slow clients never stall the pipeline.
* Static window: no packets flow. Clients keep their last decoded picture; a 10 s keepalive
  record (empty, `flags = KEEPALIVE`) lets clients tell "static" from "dead".
* Params change (width tier, fps): restart the encoder for that address; emit a new sequence
  header; clients reconfigure on `flags = CONFIG`.

### 4. Wire format (frames.sock, unchanged socket, new `format`)

Request line: `{"address":"0x…","codecs":["av1","h264","mjpeg"],"max_width":640,"max_fps":8}`
(`"format":"av1"` accepted as shorthand for a one-element list).
`mjpeg` (default for compatibility) keeps today's multipart body.
`av1`/`h264`: a byte stream of records:

```
u32 magic 'HNVF' | u32 len | u32 flags | u64 pts_us | u16 width | u16 height | payload[len]
flags: 1=KEYFRAME 2=CONFIG(sequence header / SPS+PPS) 4=KEEPALIVE
```
One record = one temporal unit (AV1) / one access unit (H.264/HEVC) / one frame (VP9). The
CONFIG record's payload starts with a NUL-terminated codec string (`av01.0.08M.08`,
`avc1.42E01E`, …) so the client can construct its decoder without guessing, followed by the
codec's out-of-band config if any (SPS/PPS for H.264). T3's route passes bytes through
with `Content-Type: application/vnd.hyprnav.frames`. `hyprnav frames --format av1 -o out.ivf`
writes IVF for `ffplay`.

### 5. Client: WebCodecs, MJPEG fallback

* Browser: probe `VideoDecoder.isConfigSupported` for the codecs it knows, send that list as
  `codecs`, `fetch()` the route, parse records from a `ReadableStream`, configure `VideoDecoder`
  from the CONFIG record's codec string, paint `VideoFrame`s to a canvas
  sized to the player. Firefox ≥130 and Chromium support WebCodecs video; hardware decode where
  available, dav1d software otherwise (sub-millisecond at 640 wide).
* Why not fMP4 + Media Source Extensions: it needs a muxer, init segments, timestamp
  continuity, and MSE buffers stall on gaps, which damage-driven streams have constantly.
  WebCodecs is a decoder and a canvas; static windows simply stop producing frames.
* Fallback chain: WebCodecs unavailable → `format=mjpeg` `<img>` as today.
* T3 mini player: the same `DesktopAgentMiniPlayer`, content swaps from `<img>` to canvas.

### 6. Target the right window

Today the stream follows `current_target`, which the MCP moves only when it acts. Two changes,
both cheap:
* MCP: beat with the dialog as target when `getDialog` binds one (it is what the agent is
  looking at).
* Daemon option `"follow":"target"|"transient"`: with `transient`, if the target has a mapped
  transient child (dialog) the pipeline captures the child instead, switching on map/unmap.

### 7. Security

frames.sock is a user-private socket. T3's route stays loopback-gated plus the agent-window
allowlist; for any non-loopback exposure (Tailscale) use the signed-URL pattern from
`AssetAccess.ts` before opening the route. Nothing here changes the picker/portal path.

## Cost model (expected, 640 wide, to be measured)

| case | capture | scale+encode | daemon | wire |
|---|---|---|---|---|
| static hidden window | 0 (no damage) | 0 | 0 | 0 (keepalive 10 s) |
| ticking countdown, 8 fps | one standalone render per change | 2 % helper + 2 % ffmpeg (E2, T1) | ~150 records/20 s | **~3 KB/s AV1, ~2 KB/s H.264** (E2) |
| 4 windows watched | linear in moving windows only | VCN has headroom for dozens of 640p streams | negligible | linear |

## Experiment results (2026-09-20)

* E1 no-go for `ignore_damage=0` on hidden windows (monitor-commit driven); §1 redesigned around
  a plugin damage hook. E2 go: av1_vaapi/h264_vaapi with `-async_depth 1`, 2 ms, ~3 KB/s.
  E3 go: WebCodecs in Firefox 155 and Chromium 152 decode the record stream, 1–4 ms to paint,
  10 s gaps fine; HEVC unsupported in both; H.264 must be Annex-B without `description`.
  E4 go: dmabuf capture works for hidden windows (deferred to T2).

## Experiments before building (each ≤ 1 h, lab only)

* E1 `ignore_damage=0` + `render_unfocused=true` on a hidden countdown: frames per 10 s ≥ 8,
  0 frames while static, helper CPU ≈ 0. Also without `render_unfocused`, to document the
  deadlock claim.
* E2 `ffmpeg -f rawvideo … -c:v av1_vaapi` fed from the helper at 640x360, 8 fps: CPU %, latency
  glass-to-glass (timestamp overlay in the demo app), bytes/s. Same with `h264_vaapi`.
* E3 WebCodecs AV1 decode in Zen and in Electron: a static HTML page fed from `hyprnav frames
  --format av1` via the T3 route; confirm playback, latency, canvas paint cost.
* E4 dmabuf capture: `linux_dmabuf` event honoured by Hyprland for a hidden window; GBM
  allocation with the advertised format/modifier works from the helper.

## Phases

* A. Experiments E1–E4 (½ day). Go/no-go on damage-driven mode and on T1 vs T2.
* B. Daemon + helper: damage-driven capture, encoder T1, GOP cache, record format, CLI
  (1 day).
* C. T3: route content-type passthrough, WebCodecs player with MJPEG fallback (½ day).
* D. Optional: T2 zero-copy (GStreamer) if E2 shows the upload copy matters; `follow=transient`
  and the MCP dialog beat (½ day).

## Decisions (user, 2026-09-20)

1. Encoder for the first build: T1, ffmpeg child with the upload path. Zero-copy (T2) later,
   behind the same socket contract, only if E2 shows the copy matters.
2. Client decode: WebCodecs to canvas. MJPEG `<img>` stays as the fallback.
3. Width tiers: 640 default; 320/960/native on request.
4. Dialog follow is in scope now: MCP beats with the bound dialog, daemon `follow=transient`.
