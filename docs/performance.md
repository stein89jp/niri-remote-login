# Performance controls and reproducible validation

The remote desktop keeps one display and bounded latest-frame delivery. The
accepted audio baseline remains 48 kHz stereo S16LE PCM in 20 ms blocks, with
both RDPSND timestamps zero. Audio and video measurements use internal monotonic
time; graphics acknowledgements do not establish physical presentation or an
audio/video clock.

## Configuration

Set frontend environment variables in `/etc/niri-rdp/frontend.conf`, then apply
them during your planned reinstall or service restart. Configuration is validated
before desktop capture starts.

| Variable | Values and precedence |
| --- | --- |
| `NIRI_RDP_ENCODER` | `bitmap` (default), `nvenc`; NVENC validates the hardware encoder at startup, and clients without a compatible graphics codec receive bitmaps |
| `NIRI_RDP_FPS` | 1–60; a ceiling, default 30 for bitmap and 60 for NVENC |
| `NIRI_RDP_BITRATE_MBPS` | 1–100; video ceiling, default 12 |
| `NIRI_RDP_LINK_MBPS` | Optional known total link budget, 3–100; reserves 1.7 Mbps for PCM plus framing and sets the video target below the remaining budget |
| `NIRI_RDP_VIDEO_PROFILE` | `speed` (existing p1 default), `balanced` (p4 with spatial AQ), `quality` (p5 with spatial AQ); B frames and lookahead remain disabled |
| `NIRI_RDP_VIDEO_CODEC` | `avc420` (default), `auto`, `avc444`; negotiated client support determines the usable codec, with AVC420 and bitmap fallbacks |
| `NIRI_RDP_GPU_CAPTURE` | `shm` (default) or `auto`; DMA-BUF import/conversion is selected only with a supported capture, GPU and encoder combination; SHM remains the fallback |
| `NIRI_RDP_GPU_DEVICE` | Optional NVIDIA DRM render node, for example `/dev/dri/renderD128`; otherwise readable render nodes are probed in order. Applies to `auto` GPU capture |
| `NIRI_RDP_SCALE` | `client` or explicit 1.0–5.0; an explicit value takes precedence over client Display Control scale metadata |
| `NIRI_RDP_ADAPTIVE_GRAPHICS` | `1` opts into coordinated FPS and a bounded graphics window; `0` (default) retains the established cadence/window while comparisons are collected |
| `NIRI_RDP_DIAGNOSTICS` | `1` enables bounded timing/counter reports; ordinary operation avoids diagnostic aggregation |

GPU capture needs the packaged NVIDIA frontend drop-in described in
[the installation instructions](install.md#optional-nvidia-encoding). It sets
`PrivateDevices=no` and adds the `video` and `render` groups, making DRM render
nodes and NVIDIA driver nodes available to the service. The allocator reads
`/sys/dev/char/MAJOR:MINOR/device/vendor` to identify NVIDIA nodes; the service's
read-only sysfs policy permits this lookup. Keep that metadata readable when
adding local sandbox overrides. On systems with several GPUs, choose the render
node explicitly and ensure its NVIDIA driver device nodes exist; the packaged
device helper prepares GPU minor 0, control, and UVM nodes. Unsupported or
inaccessible GPU capture falls back to SHM. Installed service-account access
remains a separate acceptance check.

Separate cursor metadata is available from the maintained headless compositor
only. The frontend selects it using negotiated pointer support and falls back to
an embedded cursor when the client cannot represent the shape. Clipboard is
Unicode text only, confined to the authenticated compositor connection, limited
to 1 MiB of UTF-8 text and finite transfer deadlines. Enable clipboard in the RDP
client's local-resource controls. Files, images and host-local selection access
are outside this feature.
Native oversized offers are ignored so a later text selection can recover.
Malformed or oversized incoming RDP clipboard framing closes that connection
before fragment assembly; reconnect restores the text channel.

Private audio helper retries are finite. An unavailable audio descriptor permits
a usable desktop; after an active capture fails, reconnect obtains fresh format
and capture state. TCP keepalive and a 20-second outstanding-write timeout detect
silent loss independently of idle desktop pixels or suspended graphics ACKs.
The broker's 45-second retention still starts at observed lease detachment.

## Measurements

Run `scripts/headless-performance.py` through `lab/run-isolated.sh`. It launches
only a private compositor, synthetic application and loopback client, records
separate frontend/compositor/application CPU, capture/video timing, frame cadence,
client decoded updates and a synthetic PPM screenshot. Its `--link-mbps` and
`--delay-ms` options use a bounded duplex relay on that private loopback.
The relay records wire bytes and keeps two queued 16 KiB chunks per direction;
it is a userspace link model, not a physical LAN or kernel-qdisc measurement. An idle desktop
should send initial pixels and recovery/control traffic; repeating identical
desktop frames is unnecessary.
These graphics workloads do not play audio; the configured PCM reserve is a
numeric bitrate budget. Simultaneous audio/video listening and synchronization
remain the post-reinstall checks below.

Prepare the patched compositor, release frontend, `lab/dynamic-app`, and
`artifacts/rdp-desktop-client` as described in the component READMEs. FreeRDP and
its dependencies may be extracted into `lab/sysroot`; they need not be installed
on the host. The desktop client fixture uses FreeRDP 3.27+ codec-statistics APIs;
these measurements use FreeRDP 3.32.2 with its displayless GDI frontend.

```sh
lab/run-isolated.sh python3 scripts/headless-performance.py \
  --width 1920 --workload colored-text --encoder bitmap \
  --report artifacts/performance/1080p-text-bitmap.json
lab/run-isolated.sh --nvenc python3 scripts/headless-performance.py \
  --width 3840 --workload scroll --encoder nvenc \
  --link-mbps 10 --delay-ms 50 \
  --report artifacts/performance/4k-scroll-shaped.json
lab/run-isolated.sh --nvidia-renderer python3 scripts/headless-performance.py \
  --width 3840 --workload colored-text --encoder nvenc \
  --codec avc444 --capture auto \
  --report artifacts/performance/4k-text-avc444-gpu.json
```

Repeat idle, typing, scrolling, colored small text, window movement, video and
resize at 1920×1080 and 3840×2160, using bitmap, NVENC and NVENC-configured bitmap
fallback. Compare unshaped and constrained links at matched targets. NVENC rate control
is a target rather than a physical wire cap. Native benchmark payload rates
project bytes per frame at the requested FPS; they are not measured wire rates,
and an encoder that misses its frame budget cannot sustain that projection.
Reserve additional headroom on constrained links and
check actual payload and wire-byte receipts alongside audio behavior. Use the
native `scripts/encoder-bench.c` matrix for conversion/encode and decoded image
error; it excludes capture, networking and client decoding. Keep those scopes
separate. EndPaint/update rates count decoded updates, which can include partial
rectangles; they are not monitor refresh or physical presentation FPS.
The server's historical `delivered_frames` counter counts successful GFX
submission to its event queue, before TCP write or client acknowledgement.
Use the client codec counts and decoded images to establish receipt/decoding.
Each short run includes connection startup and initial image delivery. A shaped
link may fail to deliver the requested codec within that window; its receipt sets
`decoded_path_within_window=false`, which is a delivery stall rather than a
successful codec performance result. Unshaped runs require decoded pixels and,
for NVENC, actual AVC commands. Increase `--seconds` for sustained measurements.
Use `--adaptive-graphics` for matched opt-in controller comparisons and use twice
`--delay-ms` as the relay's added nominal round-trip delay.
The Mbps setting is a ceiling. At larger delays the finite relay queue can
limit bulk throughput below that ceiling; use measured TLS payload bytes and
allow initial bitmap delivery to complete before assessing the codec policy.

Compare saved colored-text screenshots with
`scripts/compare-desktop-quality.py` (requires Python NumPy), selecting `--bitmap-dir`, `--shm-dir`,
`--gpu-dir` and optional `--avc444-dir` from matched workload directories.
The helper first proves the lossless reference crop and repeat consistency,
then reports RGB/luma error and separate black/colored glyph and surrounding
pixel errors. It rejects missing, blank, mirrored or incomplete references;
it does not align or blur away codec errors. These are decoded synthetic pixels,
not a substitute for viewing text through mstsc.

Record machine/kernel, Rust/FFmpeg/FreeRDP versions, revisions, binary/package
hashes, renderer, GPU/driver, settings and link conditions beside every receipt.
The initial budgets are bounded state and stale-frame prevention: latest capture,
finite graphics window, bounded frame age, no persistent backlog, no lost input
releases, and the accepted audio FIFO/expiry/credit controls. Comparative quality,
latency and audio evidence is required before changing profile/codec/capture
defaults.

## Windows checks after reinstall

Use `docs/uninstall.md` and `docs/install.md` for the clean installation, then
record the remaining gates in `docs/acceptance.md`. Test Windows 100/150/200% DPI,
accurate corners/center/drag, cursor shape/hotspot/hide/animation, Japanese/emoji
clipboard in both directions, portrait/landscape resize, minimize/resume,
blackholed network loss and retained-desktop reconnect. Compare colored one-pixel
edges and settled text with AVC444 and AVC420 fallback.

Keep the established PCM combination and listen to speech/music after reconnect.
Run at least 30 minutes each of Firefox/YouTube and VLC flash/tone content,
including pause/resume, limited bandwidth and short stalls. Measure initial/final
audio/video offset and recovery. Installed service-account GPU access, cold boot
without local login, monitor off/disconnected, helper recovery and session cleanup
remain actual-machine checks. Automated decoder or server timing results cannot
check these gates off.
