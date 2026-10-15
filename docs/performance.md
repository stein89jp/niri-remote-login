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
| `NIRI_RDP_BITRATE_MBPS` | 1–100; video ceiling, default 12. Adaptation uses 6 quantized levels down to 1 Mbps; decreases at most once per second, increases after 10 s healthy, applied at the next due IDR or within 30 s |
| `NIRI_RDP_LINK_MBPS` | Optional known total link budget, 3–100; reserves 1.7 Mbps for PCM plus framing and sets the video target below the remaining budget |
| `NIRI_RDP_VIDEO_PROFILE` | `speed` (existing p1 default), `balanced` (p4 with spatial AQ), `quality` (p5 with spatial AQ); B frames and lookahead remain disabled |
| `NIRI_RDP_VIDEO_CODEC` | `avc420` (default), `auto`, `avc444`; negotiated client support determines the usable codec, with AVC420 and bitmap fallbacks |
| `NIRI_RDP_GPU_CAPTURE` | `auto` (default with NVENC) or `shm` to force CPU readback; DMA-BUF import/conversion is selected only with a supported capture, GPU and encoder combination; SHM remains the fallback, and bitmap mode always uses SHM |
| `NIRI_RDP_GOP_SECONDS` | 1–3600, default 30; periodic NVENC IDR safety net. Refresh, resize, resume, frame drops and other recovery points still force IDRs immediately |
| `NIRI_RDP_GPU_DEVICE` | Optional NVIDIA DRM render node, for example `/dev/dri/renderD128`; otherwise readable render nodes are probed in order. Applies to `auto` GPU capture |
| `NIRI_RDP_SCALE` | `client` or explicit 1.0–5.0; an explicit value takes precedence over client Display Control scale metadata |
| `NIRI_RDP_ADAPTIVE_GRAPHICS` | `1` opts into coordinated FPS and a bounded graphics window; `0` (default) retains the established cadence/window while comparisons are collected |
| `NIRI_RDP_MICROPHONE` | `auto` (default) offers mstsc microphone redirection (MS-RDPEAI `AUDIO_INPUT`, 48 kHz mono PCM preferred, about 0.8 Mbit/s upload while a remote application records); `off` offers no channel. Separate from playback; see [remote microphone](install.md#remote-microphone) |
| `NIRI_RDP_DIAGNOSTICS` | `1` enables bounded timing/counter reports, including `idr_frames`, `deferred_rate_increase_frames`, `bitrate_level_decreases` and `bitrate_level_increases`; ordinary operation avoids diagnostic aggregation |

GPU capture needs the packaged NVIDIA frontend drop-in described in
[the installation instructions](install.md#optional-nvidia-encoding). It sets
`PrivateDevices=no`, allows only the NVIDIA and DRM device groups under the base
unit's `DevicePolicy=closed`, and adds the `video` and `render` groups, making
DRM render nodes and NVIDIA driver nodes available to the service. Re-copy it
after upgrading; older copies lack the `DeviceAllow` lines. The allocator reads
`/sys/dev/char/MAJOR:MINOR/device/vendor` to identify NVIDIA nodes; the service's
read-only sysfs policy permits this lookup. Keep that metadata readable when
adding local sandbox overrides. On systems with several GPUs, choose the render
node explicitly and ensure its NVIDIA driver device nodes exist; the packaged
device helper prepares GPU minor 0, control, and UVM nodes. Unsupported or
inaccessible GPU capture falls back to SHM. Installed service-account access
remains a separate acceptance check.

SHM capture keeps a pool of up to three shared capture buffers per layout plus
one private overflow buffer, so a 3840×2160 desktop can hold four 33 MB buffers
(about 133 MB) in the worst case. Snapshots of a pooled buffer share its pixels
instead of copying them; a buffer still held as a diff base or encoder input is
not handed back to the compositor, and capture then uses the overflow buffer and
copies. Compositor damage limits pixel comparisons to the damaged 64-pixel
tiles; once per second a full comparison audits that damage, and a change found
outside it forces a full refresh.

With `NIRI_RDP_DIAGNOSTICS=1`, capture reports these counters:

| Counter | Meaning |
| --- | --- |
| `capture_damage_limited_compares` | Frames compared only within compositor-damaged tiles |
| `capture_full_compares` | SHM frames compared in full: no usable damage, or the once-per-second audit |
| `capture_changed_tiles` | Changed 64-pixel tiles found by those comparisons |
| `capture_damage_missed` | Audits that found changed pixels outside the reported damage; each forces a full refresh and should stay 0 |
| `capture_pool_exhausted` | SHM captures that used the overflow buffer and copied because every pooled buffer was held |

Each served connection, including failed TLS handshakes and abrupt TCP drops,
ends with one always-on `RDP connection summary` journal line, independent of
`NIRI_RDP_DIAGNOSTICS`:

```text
RDP connection summary peer=192.0.2.10:50123 duration_s=60.0 result=ok video_codec=h264 video_mode=avc420 frames=3600 idr_frames=4 video_avg_kbps=1000 audio_codec=pcm audio_waves=3000 audio_discarded_blocks=5 audio_credit_waits=6 audio_codec_recoveries=7 clipboard_to_client=8 clipboard_from_client=9 clipboard_timeouts=10 clipboard_oversize_drops=11 mic=pcm_48000_mono mic_opens=2 mic_packets=1306 mic_bytes=2507520 mic_dropped=0 mic_errors=0
```

It records the peer address, duration, `result=ok` or `error`, video codec and
mode, H.264 frames, IDR frames, average H.264 payload kbps, audio codec, waves,
discarded capture blocks, credit waits and codec recoveries, clipboard transfers
each way, clipboard timeouts and oversize drops. The microphone fields give the
recording format (`mic=none` when Windows never recorded), successful opens,
packets and bytes received, audio frames dropped by the bounded queues, and
failed or unanswered opens, invalid packets and helper connection errors. `video_codec=bitmap
video_mode=none` means no H.264 frames were sent; bitmap frames are not counted.
The line carries counts, durations and the peer address only: no usernames,
error text, clipboard content or credentials.

The frontend reads the separate cursor through standard ext-image-copy-capture
cursor sessions. It selects it using negotiated pointer support and falls back to
an embedded cursor when the client cannot represent the shape, including shapes
over 384 px (a drag-and-drop icon can make the shape large). Supported clients
move the cursor locally; ordinary remote input is not echoed back as an RDP
position update, which would force mstsc's newer local pointer backward. That
suppression needs the maintained compositor's no-echo patch (0011); with stock
Niri the cursor would snap back while moving, so the frontend and compositor are
packaged together. Patch 0012 includes the drag-and-drop icon in the cursor
image. Animated cursors re-render each animation step. Actual
compositor warps still send position updates, including repeated warps to the
same target. Shape, visibility and fallback changes do not replay old positions.
When the client negotiates a pointer cache, each activation keeps up to
min(cache size, 32) recent shapes and sends a repeated shape as a cached-pointer
reference instead of its full bitmap; a reconnect, resize or reactivation starts
with an empty cache, and a cache size of 0 sends full shapes as before.
The clipboard carries Unicode text, HTML and images in both directions, confined
to the authenticated compositor connection with finite transfer deadlines.
Windows text, `HTML Format` (CF_HTML, offered as `text/html`) and one image (PNG,
then DIBV5, then DIB, offered as `image/png`) reach Linux; an image is fetched
only when the copy has no text. Linux `image/png` reaches Windows as CF_DIB plus
a registered `PNG` format, and `text/html` as `HTML Format`. Limits are 1 MiB of
UTF-8 text, 4 MiB of HTML, and 32 MiB and 32768 pixels per side for images.
Enable clipboard in the RDP client's local-resource controls. Files and
host-local selection access are outside this feature: a Windows copy with no
usable format, such as files, leaves the Linux clipboard unchanged.
Native oversized or unusable offers are ignored so a later selection can recover.
Local text, HTML or image data that exceeds its limit while being read logs one
warning naming only the format and limit, is counted as an oversize drop, and is
not offered to the client.
A well-formed incoming RDP clipboard message larger than 32 MiB + 4104 bytes is
skipped without buffering and logged once with only its declared size; the
session continues, and a skipped paste response becomes a failed response, so
that paste simply fails. Malformed or compressed clipboard framing still closes
the connection before fragment assembly; reconnect restores the clipboard channel.

Private audio helper retries are finite. An unavailable audio descriptor permits
a usable desktop; after an active capture fails, reconnect obtains fresh format
and capture state. TCP keepalive and a 20-second outstanding-write timeout detect
silent loss independently of idle desktop pixels or suspended graphics ACKs.
The broker's retention (`grace_seconds`, default 45 seconds) still starts at
observed lease detachment.

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
latency and audio evidence is required before changing profile or codec
defaults. NVENC's GPU-capture default keeps automatic SHM fallback.

## Windows checks after reinstall

Use `docs/uninstall.md` and `docs/install.md` for the clean installation, then
record the remaining gates in `docs/acceptance.md`. Test Windows 100/150/200% DPI,
accurate corners/center/drag, cursor shape/hotspot/hide/animation, Japanese/emoji
text, HTML and image clipboard in both directions, portrait/landscape resize, minimize/resume,
blackholed network loss and retained-desktop reconnect. Compare colored one-pixel
edges and settled text with AVC444 and AVC420 fallback.

Keep the established PCM combination and listen to speech/music after reconnect.
Run at least 30 minutes each of Firefox/YouTube and VLC flash/tone content,
including pause/resume, limited bandwidth and short stalls. Measure initial/final
audio/video offset and recovery. Installed service-account GPU access, cold boot
without local login, monitor off/disconnected, helper recovery and session cleanup
remain actual-machine checks. Automated decoder or server timing results cannot
check these gates off.
