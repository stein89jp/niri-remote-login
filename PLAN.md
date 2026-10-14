# Performance and quality implementation plan

Created 2026-10-14 from the repository performance/quality review.

## Request and scope

The user requested that **all improvements from the review be implemented and
validated**. This file is the handoff to the implementing Codex session. Proceed
through the work below; do not stop after producing another proposal or fixing
only the easiest items. The user authorized focused agent work where useful.

Deliver working changes, meaningful regressions, reproducible measurements,
updated documentation, and a buildable package. Larger features may use
capability-selected paths and dependable fallbacks. A prototype or investigation
alone does not finish a requested feature. Promote performance settings to
defaults only when comparative measurements justify them.

Implement in focused increments. Keep this checklist current, including commit
references, validation receipts, and any remaining machine acceptance. Complete
all independent work when one item needs unavailable hardware or client access.
Do not mark the overall request complete while required validation is pending.
Installing on the working host, interrupting its sessions, or publishing a
release requires separate deployment authorization; source work, disposable
testing, and package preparation can proceed.

## Read first and preserve

Read [AGENTS.md](AGENTS.md), [bridge/README.md](bridge/README.md),
[compositor/README.md](compositor/README.md),
[docs/validation.md](docs/validation.md), and
[docs/acceptance.md](docs/acceptance.md). Recheck source before editing: the
references below describe the review snapshot, not immutable line numbers.

- IronRDP is pinned to `bf019ca502d1e52616853a5ded68c885cec1e322` in
  `bridge/Cargo.toml` and `bridge/Cargo.lock`.
- Niri is pinned to `0be60b742762905b7bee1ea67bb5a2fb00859bf0` in
  `compositor/pins.json`; maintained modifications live under
  `compositor/patches/`. Preserve dependency pins and lockfiles. Add required
  support through maintained, reproducible changes rather than an upstream
  upgrade as a shortcut. Update patch hashes and package source inventories
  when their tracked inputs change.
- Build trees, dependency caches, `research/upstream/`, and generated
  `packaging/arch/*/src/` copies are reference material, not the authoritative
  place to implement a fix. The selected compositor archive may have different
  capabilities from a newer research snapshot.
- Preserve bounded queues, latest-frame delivery, cancellation-safe encoding,
  IDR recovery, output suppression/resume, client capability fallback, held-input
  release before broker detachment, private session isolation, and fresh PAM
  authorization on reconnect.
- Preserve full-range BT.709 video conversion, edge padding, coded dimensions
  aligned to 16, and cropping to the logical desktop. Test any replacement GPU
  conversion against the existing color and guarded-buffer regressions.
- Keep the initial scope to the existing single-display desktop. Multi-monitor,
  microphone redirection, and replacement authentication mechanisms are separate
  projects. Reuse IronRDP's protocol implementations where available.

### Accepted audio baseline

The release-19 combination is a hard constraint, as documented in AGENTS.md:

- Prefer **48 kHz stereo S16LE PCM**, paced in **20 ms blocks**, before AAC and
  MS ADPCM. Its approximately **1.54 Mbit/s** payload cost is intentional.
- Keep **both RDPSND wire timestamps zero**. Internal monotonic timestamps for
  telemetry and sample age do not change the RDP presentation-clock contract.
- Preserve four pending waves, three capture blocks, 60 ms capture expiry,
  silence suppression, finite playback recovery, and validated recent feedback.
- Optional second confirmations provide measurements; they must never become
  mandatory credits or indefinite playback gates.
- An ordinary credit wait preserves recent queued samples and AAC state. Reset
  AAC after actual sample loss, not merely because the credit window fills.
- Retain the PCM preference/exact wire-byte and credit-wait continuity tests.
  Preserve existing coupling that lowers video bitrate under audio congestion.

The broad request to improve the project should be implemented within this
baseline. A deliberate codec/control change still needs explicit approval and
actual Windows 10 mstsc clarity/latency validation. Decoder success alone is
insufficient. Release 20 addressed delayed playback feedback, but real Windows
30-minute Firefox/VLC sync, stalls, and repeated-reconnect acceptance remain open.

## Execution order and progress

Suggested order: establish measurement and reproduce the controller bug; improve
capture/input scheduling; remove redundant work; implement coordinated video
policies and desktop features; integrate GPU/text-quality paths; finish package
and actual-machine validation. Audio/reconnect resilience can proceed alongside
capture work. Delegate independent components with clear file ownership; avoid
concurrent edits to `wayland.rs`, `video.rs`, or shared compositor patches.

| ID | Work | Depends on | Status |
| --- | --- | --- | --- |
| W01 | Complete-path measurements and benchmark workloads | None | [x] Seven workloads, repeated 1080p/4K matrices, decoded comparisons and final-package receipts; physical presentation pending |
| W02 | Relearn stale graphics ACK baseline | None; compare with W01 | [x] Bounded ACK relearning + transition/reset/audio regressions |
| W03 | Service input during capture waits | None | [x] Wakeable capture/import; real one-FPS app input 2.139 ms |
| W04 | Damage-aware capture, bitmap suppression, partial updates | W03 | [x] Damage/partial/full-snapshot regressions and actual refresh/suppression/resume wire proof |
| W05 | Coalesce motion and preserve critical input under overload | W03 | [x] Bounded motion coalescing, ordered barriers, drag/release regressions |
| W06 | Coalesce resize requests | W03; coordinate W05 | [x] Adjacent resize coalescing; native portrait/odd-size integration passed |
| W07 | Cache login rendering and keyboard translation | W04 for partial updates | [x] Cached framebuffer/partial form + thread-owned XKB; comparative receipt |
| W08 | Joint FPS/bitrate adaptation and measured quality profiles | W01, W02 | [x] Matched profile/controller comparisons; duplicate pacing regression fixed; optional policy and defaults documented |
| W09 | Bounded adaptive graphics window | W01, W02; coordinate W08 | [x] Bounded adaptive window; 16 final-package shaped comparisons at nominal RTT 20/50/100/200 ms passed |
| W10 | Bounded static-image refinement | W04, W08; coordinate W13 | [x] Bounded one-shot refinement, congestion/expiry tests and corrected 1080p/4K decoded comparisons |
| W11 | Separate RDP cursor updates | W03, W04 | [x] Maintained cursor protocol + negotiated fallback; cursor/DPI proof passed; Windows pending |
| W12 | GPU-resident capture/conversion/encoding | W01, W04; coordinate W13 | [x] Native analytic color/padding plus final-package GPU420/444 decode/resize/reconnect and SHM fallback passed |
| W13 | AVC444 text-quality path | W01, W08; coordinate W10, W12 | [x] Packing/reference/color tests and strict actual FreeRDP AVC444 input/resize/reconnect passed; mstsc pending |
| W14 | Audio source-age tracking across local queues | W01 | [x] NRA1 source/relay age and sequence; continuity/fragmentation tests passed; listening pending |
| W15 | Readiness-driven audio worker | W14 | [x] Readiness/deadline worker; audio regressions passed; listening pending |
| W16 | Audio-helper recovery and usable desktop without audio | None | [x] Bounded helpers + desktop-only reconnect; real disposable failure tests passed |
| W17 | Predictable silent-network-loss detection and reconnect | W01 | [x] TCP keepalive/user timeout + heartbeat; 20.4–22.8 s blackhole/reconnect proof |
| W18 | Bidirectional text clipboard | None; coordinate session lifecycle | [x] Native Unicode/isolation/reconnect; 9 lifecycle/text and 5 pre-dechunk wire tests passed; mstsc/toolkits pending |
| W19 | Explicit remote DPI scaling | W03, W06; coordinate W11 | [x] Real 100/150/200% geometry/click/cursor and config-reload/relative-motion proof passed |
| W20 | Documentation, package, and actual-machine acceptance | All above | [~] Software, checked package and reproducible handoff ready; user will perform Windows/installed acceptance after clean reinstall |

The user will perform actual Windows-to-host acceptance after their clean reinstall.
Source implementation, disposable validation and package preparation remain the
implementation session's responsibility. No commits, pushes, installations or host
service changes are authorized by this handoff; the user will commit/push.
Checklist checks above indicate completed software/focused evidence, not Windows
or installed-machine acceptance. Raw receipts live under ignored `artifacts/`.

Final software receipts are indexed in [docs/validation.md](docs/validation.md).
The final runtime frontend SHA-256 is
`789a10ecae4654ed0dd67cac9f29658f0579207f405a20c296544c5cd48cb579`.
The checked `1.1.0-1` package passed 159 bridge library tests, wire/controller/
broker/compositor checks, final-package GPU login/clipboard/fallback integrations,
and 40 final-runtime workload comparisons. The root Python suite ran 82 tests;
optional skips and separately executed hardware checks are listed in validation.
Earlier candidate matrices and failed diagnostics retain their own hashes and
source snapshots; they are not relabeled as final-runtime measurements.
The final documentation archive is checked against the measured runtime hashes;
portable source receipts are `source-export-validation.json` and
`legacy-source-validation.json` under `artifacts/plan-upgrades/`.
W20 and overall installed-machine acceptance remain open for the user's final
reinstall/Windows tests; no listening or physical-presentation result is claimed.

## Work specifications

### W01 — Measure complete-path performance

**Why:** The recorded 4K NVENC result is approximately 11.45 ms mean / 12.33 ms
p95, excluding capture/readback, input, networking, client decoding, and display.
It does not establish end-to-end 4K60. Current real-app integration establishes
correctness, rather than sustained performance or visual quality.

**Work:** Extend `bridge/src/performance.rs`, capture/video/audio telemetry,
`scripts/encoder-bench.c`, and the disposable integration client as appropriate.
Record capture wait/readback, snapshot/conversion/encode time, time awaiting
credits, send/ACK delay, actual frame cadence, frame age, dropped/suppressed
frames, wire bytes, and queue pressure. Distinguish server timing, client decode,
and physical presentation; an ACK is not a shared audio/video clock. Use bounded
aggregation and low-overhead, optional diagnostic logging. Do not log credentials,
clipboard contents, typed text, or ordinary desktop/audio content.

**Acceptance:** Reproducible reports for idle, typing, scrolling, colored small
text, window movement, video, and resize at 1080p and 4K. Include p50/p95/max
latency, delivered FPS, CPU/GPU load, bandwidth, and decoded image comparisons.
Test bitmap, NVENC, and NVENC-configured bitmap fallback. Compare LAN and shaped
bandwidth/latency/stalls in disposable namespaces, reserving PCM bandwidth plus
framing overhead. Repeat measurements and record machine, revision, workload,
encoder, and client information. Establish budgets before promoting new defaults.

### W02 — Fix stale ACK baseline

**Why:** `AdaptiveBitrate::acknowledged()` retains a lifetime minimum ACK delay;
`reset_frames()` does not reset it. A standalone harness of the actual controller
reproduced one 20 ms ACK followed by 30 steady 250 ms ACKs with zero queue bytes:
the target fell from 12 Mbps to 1 Mbps. A connection starting at 250 ms retained
12 Mbps. This is reproduced controller behavior, not a measured Windows failure.

**Work:** Add a failing regression in `bridge/src/performance.rs`; implement
bounded baseline aging or careful relearning after a stable path change. Account
for generation/reset behavior. Distinguish sustained transit changes from client
decoding backlog and rising queue delay; raising the baseline must not conceal
actual congestion or remove audio-pressure safeguards.

**Acceptance:** Cover 20→250 ms and reverse transitions, stable long RTT, jitter,
queued bytes, missing ACKs, malformed/unknown ACKs, capability resets, and audio
congestion. An empty-queue stable path must recover useful quality; genuine
backlog must still reduce the target. Commit the regression; do not depend on an
ephemeral `/tmp` harness.

### W03 — Make capture waits interruptible by input

**Why:** `wayland.rs` drains input between captures. During screencopy negotiation
and readback, `pump_with_timeout()` polls only the Wayland FD. Commands cannot
wake those waits, so a slow capture delays keyboard and pointer delivery.

**Work:** Add a command wake source/event FD to the capture event loop. Service
keys, motion, buttons, release, and stop while capture is pending; keep resize at
a safe buffer/output boundary. Preserve Wayland prepare-read cancellation rules,
flush behavior, capture cancellation, deadlines, and worker teardown.

**Acceptance:** Artificially delay capture negotiation/readback and prove input
arrives before capture finishes. Test one-FPS capture, idle/damage waits, stop,
compositor exit, held-input release, and broker lease ordering. Avoid busy polling.

### W04 — Capture damage and avoid unchanged bitmap traffic

**Why:** The capture loop calls `capture_output()`/`copy()` every tick and creates
an owned full-frame snapshot in `lib.rs::capture_bitmap()`. NVENC deduplicates
after that copy; bitmap mode and pre-AVC fallback return without this suppression.
The pinned compositor has wlr `CopyWithDamage` support; verify its exact semantics.

**Work:** Use damage-aware screencopy with shared unchanged-frame suppression for
bitmap and video paths. Deliver partial bitmap updates for changed rectangles,
with bounded rectangle count and a full-frame fallback when damage is broad.
Accumulate damage across overwritten/skipped latest-frame notifications so pixels
are never lost. Retain or reconstruct a complete current desktop image for video
encoding: the current `Encoder::encode()` requires origin `(0,0)` and full desktop
dimensions. Emit partial rectangles only through a path that supports them;
never pass partial `BitmapUpdate`s directly into the existing video wrapper.
Apply accumulated damage to that complete image before encoding after skipped
notifications. Keep control/refresh heartbeats separate from expensive readback
and distinguish legitimate no-damage waiting from a stalled/failed compositor.

**Acceptance:** Static desktops stop repeated pixel transmission and substantially
reduce readback/copy work. Test sparse edits, skipped frames, overlapping damage,
padding/inversion/channel order, cursor changes, resize, late graphics negotiation,
refresh, minimize/resume, backpressure, and IDR recovery. A quiet healthy desktop
must not disconnect at the old three-second capture deadline.

### W05 — Coalesce input bursts without losing ordered events

**Why:** The input channel has capacity 256; failed `try_send()` currently ends
the session. Mouse floods combined with capture/resize stalls can exhaust it.
This is a confirmed overload policy, not an observed mstsc failure.

**Work:** Coalesce adjacent redundant absolute motion. Preserve relative movement
semantics, drag paths where required, wheel deltas, button coordinates, and
ordering barriers around keys/buttons/resizes. Provide bounded reliable handling
for release/stop; no unbounded queue or arbitrary loss of key-up/button-up events.
Define a deterministic failure policy if critical traffic alone exceeds capacity.

**Acceptance:** Motion floods during slow capture and resize remain connected and
reach the final position. Mixed clicks, drags, wheel events, held modifiers, and
disconnect releases retain order and leave no stuck input. Memory stays bounded.

### W06 — Coalesce adjacent resize requests

**Why:** Each different queued size invokes synchronous output reconfiguration
and can recreate the graphics surface/encoder. Window dragging needlessly visits
intermediate sizes; each output resize has a bounded but lengthy failure deadline.

**Work:** Apply only the latest adjacent size, preserving input events whose
coordinates belong to an earlier geometry. Commit desktop size, output mode,
pointer mapping, frame geometry, and encoder generation coherently. Avoid dropping
the final request or encoding frames at a superseded size.

**Acceptance:** Resize floods do fewer reconfigurations and converge promptly.
Retain the existing odd-height, portrait/landscape, client reactivation, center
click, capability recovery, and input-order regressions.

### W07 — Remove redundant login rendering and translation work

**Why:** `login.rs` redraws the full desktop on notifications and every second;
rendering allocates/fills a full BGRA image, then converts it. A 4096² BGRA buffer
alone is 64 MiB. Password characters after the first keep the same fixed-mask
pixels but still notify. `login_text()` also recompiles the saved XKB map per
character. NVENC suppresses identical pixels only after this CPU work.

**Work:** Cache the background/framebuffer and visible state; repaint only changed
regions, preserving cached full refresh for startup/late graphics negotiation.
Cache safe XKB translation state with appropriate thread ownership. Do not expose
password length or alter physical-layout/modifier/Unicode semantics.

**Acceptance:** Measure reduced allocation/CPU/traffic at 1080p and 4K. Preserve
fixed password masking, login rejection/authentication, Japanese/AltGr/keypad,
delayed graphics setup, output suppression, bitmap fallback, and desktop transition.

### W08 — Coordinate frame rate, bitrate, and quality profiles

**Why:** The same default 12 Mbps ceiling serves 1080p and 4K; adaptation currently
changes only bitrate. Backpressure/skipping changes actual cadence while the
encoder retains nominal FPS and its original one-frame VBV. The fastest NVENC
`p1` preset is used without comparative desktop-quality evidence.

**Work:** Add a bounded, stable policy that coordinates capture/send cadence and
bitrate, retains user ceilings, reserves audio capacity, and accounts for actual
cadence. Benchmark compatible presets and spatial AQ at matched bitrate before
choosing/documenting profiles. Keep B frames/lookahead disabled and preserve the
low-delay model. Do not adopt unconstrained bursts or increase queueing to hide
quality loss. Change encoder parameters only through supported operations with
correct reference/IDR recovery if recreation is necessary.

**Acceptance:** Compare text, scrolling, and video at 1080p/4K over several link
budgets. Report delivered FPS, image error/detail, per-frame bytes, encode p95,
frame age, and audio behavior. Verify convergence/recovery without oscillation,
config validation, resize/reconnect, and dynamically changed targets.

### W09 — Bound the graphics window by delivery conditions

**Why:** `video.rs::Handler::max_frames_in_flight()` returns two, and
`AdaptiveBitrate::sent()` also hardcodes two tracked frames. The pinned IronRDP
`should_backpressure()` enforces that limit in normal ACK mode. The theoretical
ceiling is approximately `window / ACK delay`: two frames / 100 ms ≈ 20 FPS.
ACK-suspended mode bypasses that limit; this estimate is not a measured FPS result.

**Work:** Implement a bounded adaptive window using delay, actual send cadence,
queue bytes, and frame age. Update controller tracking and library window together;
the pin exposes `set_max_frames_in_flight()`. Preserve a conservative ceiling and
latest-frame behavior. Handle the ACK-suspension sentinel explicitly, retaining
bounded application work without waiting for confirmations the client suspended.

**Acceptance:** Shape 20/50/100/200 ms paths and compare smoothness against p95
frame age and memory. Test stalls, resume, queue pressure, capability reset,
unknown/reordered ACKs, IDR recovery, and ACK suspension. A larger window must not
create a persistent stale-frame queue or growing audio offset.

### W10 — Refine settled images once, within a budget

**Why:** Source-pixel equality skips further encoding. A low-quality image sent
while scrolling or congested remains displayed after the source stops changing.
The missing refinement is confirmed; its visual benefit needs measurement.

**Work:** Schedule a bounded delayed refinement after motion settles and delivery
permits it. Integrate quality-profile changes and future AVC444 chroma refinement.
Keep source equality suppression for normal idle operation, stop refinement when
new content arrives, and preserve encoder reference continuity.

**Acceptance:** Decode before/after images after scrolling under congestion and
measure text/detail improvement. Repeated idle checks must not produce continuous
updates, encoder recreation, or repeated IDRs. Test minimize/resume, resize,
reconnect, ongoing congestion, and bitmap clients.

### W11 — Send cursor shape and position separately

**Why:** `capture_output(1, ...)` embeds the cursor in desktop pixels. Pointer
motion defeats deduplication and travels through capture, video encoding, and
decode. Separate RDP pointer updates should reduce desktop traffic and perceived
cursor delay.

**Work:** Verify cursor capture/metadata support in the selected Niri revision;
the reviewed archive appeared to lack newer ext-image-copy cursor support. Add
maintained compositor/protocol support if necessary. Use negotiated IronRDP
pointer updates for shape, hotspot, visibility, and position where supported;
retain embedded-cursor fallback. Coordinate client-local motion with compositor
warps, pointer grabs, animated cursors, and DPI. Avoid double cursors and avoid
requiring a full video encode for ordinary pointer movement.

**Acceptance:** Real mstsc and FreeRDP tests for click/drag, shape/hotspot changes,
hide/show, animations, warps, resize, reconnect, and DPI. Compare pointer-only
traffic and response latency; unsupported clients retain a usable cursor.

### W12 — Implement a GPU-resident video path

**Why:** NVENC still follows SHM readback, an owned CPU snapshot, BGR staging,
CPU swscale conversion, and upload. Each 4K BGRA payload pass is approximately
1.99 GB/s at 60 FPS. Existing encoding time leaves limited room in a 16.67 ms
frame budget, but only complete-path measurements can establish the benefit.

**Work:** Prototype, measure, then integrate bounded DMA-BUF capture/import,
explicit full-range BT.709 GPU conversion, and GPU encoder surfaces. Verify
formats, modifiers, selected GPU/device identity, synchronization/fences, buffer
ownership, and lifetime across cancellation/resize. Use a small reusable buffer
pool; release all resources on failure. Preserve the CPU/SHM path for unsupported
capture/import layouts and software-rendered sessions. Preserve explicit NVENC
startup failure when the encoder itself is unavailable; capture fallback does
not change that contract. Make any extra runtime dependencies reproducible and
provide service-device configuration appropriate to the supported path.

**Acceptance:** Matched before/after complete-path CPU/GPU timing and visual output
at 1080p/4K. Test color/range, odd logical dimensions, padding/crop, fences,
permission denial, unsupported modifiers, compositor/encoder failure, resize,
reconnect, and software/SHM fallback. Do not claim zero-copy while CPU readback or
unaccounted host/device transfers remain.

### W13 — Add negotiated AVC444 for desktop text quality

**Why:** Current AVC420/NV12 subsamples chroma, limiting colored text and thin
edges even at higher bitrate. AVC444 can preserve more chroma detail, but it is
an RDP capability/stream feature, not a `yuv444p` encoder toggle.

**Work:** Implement the appropriate negotiated AVC444/AVC444v2 path using the
pinned protocol APIs or maintained additions. Follow the RDP-defined main and
auxiliary AVC420 stream packing, ordering, region masks, decoder/reference rules,
and mode-specific encoder requirements. Integrate conversion, static refinement,
flow control, and measured bandwidth cost. Keep AVC420 and bitmap fallbacks.
Verify existing fixed region-QP metadata against the specification and actual
encoder behavior; correct it if required rather than assuming a visual defect.

**Acceptance:** Actual mstsc capability negotiation and decoded screenshots of
colored small text, one-pixel edges, scrolling, and mixed video/text. Test main
and auxiliary updates, fallback, reset/reconnect, resize, suppression, and decoder
continuity. Quantify quality, bandwidth, and latency before choosing defaults.

### W14 — Track sample age across the entire audio pipeline

**Why:** `audio.rs` timestamps blocks on frontend receipt, so its 60 ms expiry
does not measure earlier capture/relay/socket residence. The relay has its own
three-block FIFO and socket buffers; the existing stalled-relay regression allows
a larger whole-pipeline buffer budget. These facts do not prove current audible
delay, but obscure its origin.

**Work:** Carry internal monotonic source age/sequence metadata from the earliest
observable capture point, and report bounded relay/socket/frontend age, discarded
samples, credit waits, and recovery. Clearly distinguish capture age from browser
buffering and client playback. If adding framing or a side channel, version and
validate the private transport/readiness contract; metadata must never be decoded
as PCM. Keep RDP wave timestamps zero. Use measurements to remove avoidable
stale buffering while preserving ordinary PipeWire batching and accepted controls.

**Acceptance:** Fragmented reads/writes, sample alignment, sequence wrap/reset,
source stalls, socket pressure, reconnect, and known sample identifiers. Verify
source-age visibility after a frontend stall, exact healthy PCM continuity, and
all existing playback-feedback/AAC-fallback regressions. Actual audible delay
and Firefox/VLC sync still require W20.

### W15 — Replace fixed audio polling with readiness/deadline wakeups

**Why:** The audio worker sleeps two milliseconds per loop, nominally up to 500
wakeups/second, including sustained digital silence. Suppression saves wire
traffic but does not remove this polling cost.

**Work:** Wait on capture readiness, the next exact sample-duration send deadline,
credit/feedback changes, and stop. Keep continuous capture drain through socket
stalls, bounded buffers, and finite recovery. Preserve clock arithmetic and AAC
state across ordinary credit waits.

**Acceptance:** Compare idle wakeups/CPU and pacing jitter. Retain batching,
60 ms expiry, silence/resume, exact sample continuity, credit-wait continuity,
disconnect responsiveness, and loss-triggered AAC reset tests. Validate actual
mstsc listening before marking the audio work accepted.

### W16 — Recover audio helpers without blocking desktop resume

**Why:** `session-worker` checks audio only during initial startup and then execs
Niri. `session-audio` has no bounded daemon restart loop. `broker.c::respond_session()`
fails if the audio socket is unavailable, so ancillary audio failure can block
reconnecting to an otherwise running desktop. The Rust receiver already accepts
one or two descriptors; verify and align the complete broker contract.

**Work:** Supervise/restart private audio components with bounded retries and
backoff, correct descendant cleanup, and no use of shared user audio services.
Permit a clearly reported audio-unavailable desktop response when appropriate.
Define recovery for both an active connection and a reconnect, including fresh
format/capture state if a new audio descriptor is needed. Avoid infinite restarts,
orphan daemons, duplicate private graphs, and accidental silent codec changes.

**Acceptance:** Kill only disposable helper/PipeWire/Pulse/WirePlumber/monitor
processes. The desktop remains usable; audio either recovers through the defined
bounded path or reports degraded status; reconnect succeeds. Verify readiness
failures, fresh audio state, grace expiry, logout, broker death, and cleanup.

### W17 — Detect silent network loss predictably

**Why:** The single-connection reject policy can reject a reconnect until the old
TCP connection is detected dead. The broker's 45-second retention window starts
after observed lease detachment, not when packets stop arriving.

**Work:** Inspect the pinned library's available heartbeat/liveness mechanisms
and configure or extend bounded dead-peer detection. Combine suitable transport
timeouts with negotiated liveness; do not equate a quiet desktop with a dead
client or require graphics ACKs in ACK-suspended mode. Preserve reject policy,
fresh PAM authentication, held-input release, and desktop-retaining disconnect.

**Acceptance:** Blackhole traffic in a disposable network namespace, rather than
merely closing sockets. Record detection/reconnect time and verify the same
compositor PID/application state on resume. Test healthy idle clients, stalls,
large RTT, frontend restart, grace expiry, and failed resume authorization.

### W18 — Implement bidirectional text clipboard

**Why:** Clipboard support is missing and is a substantial everyday usability
improvement for a remote desktop, including Japanese and emoji text.

**Work:** Use IronRDP CLIPRDR and the supported Wayland selection/data-control
mechanism in the authenticated private session. Implement negotiated Unicode
text copy/paste both ways with bounded payload size/time, selection ownership,
format conversion, cancellation, and reconnect cleanup. Restrict the feature to
the authenticated desktop; do not connect it to the host's local display or bus.
Avoid polling, clipboard feedback loops, secret logging, and stale cross-session
content. Document text-only scope and any client controls.

**Acceptance:** mstsc/FreeRDP ↔ native Wayland, GTK/Qt, and XWayland where supported.
Test multiline/empty text, Japanese, emoji, ownership changes, rapid copy/paste,
oversize/malformed offers, disconnect, and two different session identities.

### W19 — Support deliberate DPI scaling

**Why:** Display Control currently extracts only pixel dimensions; output setup
forces scale 1 and validation rejects other scales. High-resolution remote UI
can therefore be uncomfortable even when the pixels are sharp.

**Work:** Add an explicit remote scale policy/user override with appropriate
client-derived behavior. Keep physical pixel capture dimensions separate from
logical compositor/app coordinates; update pointer mapping, cursor size, output
validation, resize, and inherited-config handling consistently. Respect protocol
scale limits and unsupported-client fallback. Preserve ordinary user Niri config
and input-method state. Document configuration and precedence.

**Acceptance:** Windows 100/150/200% settings plus explicit overrides. Verify
sharp, comfortably sized UI; accurate corners/center/drag clicks; cursor hotspots;
portrait/landscape resize; reconnect; native Wayland and XWayland apps. Retain
normal transform and single-display behavior.

## W20 — Validation, documentation, and package handoff

Run targeted regressions after each meaningful behavioral change, then the full
checks at integration milestones. Do not keep repeating broad tests without a
new change or unresolved concern. Add tests for failure/cleanup and observable
contracts, rather than tests that merely mirror implementation details.

```sh
cargo fmt --manifest-path bridge/Cargo.toml --check
python3 -m unittest discover -s tests -v
make check
cargo clippy --manifest-path bridge/Cargo.toml --locked --all-targets
cargo build --manifest-path bridge/Cargo.toml --release --locked
make -C broker
git diff --check
```

`make check` uses locked offline dependencies. Locate/populate compatible caches
before running it; use `BRIDGE_CARGO_HOME`/`POC_CARGO_HOME` overrides where needed.
At review time an ignored packaging cache existed under
`packaging/arch/niri-rdp/src/cargo-home/`, but its presence is not guaranteed in a
new checkout. Missing fixtures/caches are preparation work, not successful checks.
Socket-restricted sandboxes can deny the tests' private D-Bus sockets; use the
environment's normal escalation process when that happens and explain the
disposable test scope. Do not misclassify sandbox denial as a product defect.

Rebuild the maintained compositor and native clients as documented, then run
real-app integration in private namespaces:

```sh
lab/run-isolated.sh python3 scripts/headless-integration.py
lab/run-isolated.sh --nvenc env NIRI_RDP_LAB_ENCODER=nvenc \
  python3 scripts/headless-integration.py
lab/run-isolated.sh --nvidia-renderer env NIRI_RDP_LAB_ENCODER=nvenc \
  python3 scripts/headless-integration.py
NIRI_RDP_TEST_NVENC=1 python3 -m unittest tests.test_native_video -v
```

Prepare prerequisites and current artifact paths from component documentation;
these commands alone do not create a complete lab. Also run explicit
NVENC-configured bitmap fallback, login, resize, held-input release, reconnect,
compositor-crash, input-method, and new feature/failure cases. Use extracted final
package binaries for the final integration rerun, not only development binaries.

Build the combined package with unprivileged `makepkg` from
`packaging/arch/niri-rdp/`, then inspect/stage it with
`scripts/validate-packages.py`. Update source snapshots, dependency/license notices,
configuration examples, and required package checks for new features. Never
install a package merely to validate its structure. Preserve stock Niri and local
desktop services. Keep commits focused; do not publish unless requested.

### Required actual-machine evidence

Update `docs/validation.md` with executed results and `docs/acceptance.md` with
honest per-gate status. Put raw logs, benchmark JSON, generated test clips,
screenshots of test content, and package reports in ignored `artifacts/`. Record
revisions, package hashes, kernel/toolchain, GPU/driver, compositor rendering,
client build/codec/decoding mode, workload, and link conditions.

The final Windows 10 mstsc validation must include:

- Text/colored-edge clarity and motion quality, including AVC444/fallback and
  bounded refinement; sustained 1080p/4K workloads with measured FPS/latency.
- Keyboard, pointer/cursor, clipboard, DPI, live resize, minimize/resume, and
  reconnect, including silent loss and held-input teardown.
- Speech/music clarity with the accepted PCM combination, at least 30 minutes
  of Firefox/YouTube and local VLC flash/tone content, pause/resume stop-tail,
  constrained bandwidth, short stalls, and repeated reconnects. Measure initial
  and final A/V offset and recovery; source clip timestamps or decoder success
  do not establish Windows output timing.
- Installed service-account GPU access, cold boot without local graphical login,
  physical monitor off/disconnected, audio-helper recovery, and session cleanup.

If an actual client/machine is unavailable, finish independent implementation,
tests, packaging, and a concrete acceptance procedure. State exactly which gates
remain pending and what equipment/action is needed. Do not invent listening
results, promote synthetic FPS into presentation FPS, or check off W20/overall
completion until the required evidence exists.

### Definition of done

- Every W01–W19 item has working implementation and focused evidence, or a
  precisely documented external validation dependency with software work ready.
- Required regression, compositor, native encoder, and package checks pass;
  skipped opt-ins are identified and required hardware cases are executed before
  final acceptance. No known relevant regression is left unexplained.
- New paths have bounded state, tested teardown/failure, correct capability
  selection, and documented fallbacks. Default policy changes have comparative
  performance/quality evidence.
- Accepted audio behavior and authentication/session isolation are preserved.
- Final package and documentation match the implemented behavior. Actual-machine
  pending results remain pending; overall completion requires their execution.

## Review evidence and primary references

The review was read-only. The Python suite passed outside the socket-restricted
sandbox: **72 tests run, three optional skips**. Native guarded video conversion
was included. The ACK-baseline behavior was reproduced with a temporary harness
of the actual controller. No fresh end-to-end Windows or GPU performance gain
was measured during the review. The existing recorded encoder results live in
[docs/validation.md](docs/validation.md#nvidia-performance-2026-10-10).

- [Microsoft AVC420 stream requirements](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/5f12c20e-2ea1-4ad1-a2a0-019ee3893731)
- [Microsoft AVC444 stream structure](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/844018a5-d717-4bc9-bddb-8b4d6be5dd3f)
- [Microsoft AVC444v2 stream combination](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/781406c3-5e24-4f2b-b6ff-42b76bf64f6d)
- [Microsoft full-range BT.709 conversion](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/954d7546-6873-4466-95c8-20a7569c43e5)
- [NVIDIA FFmpeg hardware acceleration guide](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.1/ffmpeg-with-nvidia-gpu/index.html)
- [NVIDIA encoder API guide](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.1/nvenc-video-encoder-api-prog-guide/index.html)

Reverify exact protocol/API semantics against the selected versions during
implementation. GPU-resident transcoding examples demonstrate the architectural
benefit of avoiding transfers; they do not establish direct compatibility with
this compositor's DMA-BUF layouts or the required RGB→BT.709 conversion.
