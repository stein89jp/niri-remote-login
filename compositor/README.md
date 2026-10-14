# Maintained headless Niri dependency

This directory pins upstream PR3953 and carries the small remote-session patch.
See [CHECKPOINT.md](CHECKPOINT.md) for decisions, exact results and remaining
machine tests, and [pins.json](pins.json) for exact dependency revisions/hashes.
The normal Niri executable is not replaced.

For a fresh source tree, obtain the archive URL in pins.json, verify its SHA256,
extract into a disposable build directory and apply
`patches/0001-remote-headless-bounds-and-startup.patch` with `patch -Np1`.
Build `cargo build --release --locked --no-default-features`. Run the lifecycle
regression with `cargo test --locked --no-default-features --lib backend::headless`.
An Arch clean build environment needs Rust/git/base-devel plus seatd, libinput,
libdisplay-info, mesa, libglvnd, wayland, libxkbcommon, pango, cairo, pixman and
systemd-libs. An extracted seatd package is used only for this workspace's lab.

For a prepared workspace, `lab/build-niri.sh --offline` and `lab/build-app.sh`
rebuild local artifacts. `lab/run-isolated.sh lab/capture-proof.sh` runs only in
private namespaces, using the Arch userspace libraries read-only and software
rendering. `lab/run-isolated.sh python3 scripts/headless-integration.py` exercises
library-managed RDP and the real compositor/app capture/input bridge.

The broker's worker launches with `NIRI_BACKEND=headless`, a private runtime
directory as `XDG_RUNTIME_DIR`, and a configuration selected by the worker.
It normally includes the user's existing Niri configuration, with an explicit
remote override and system fallback described in [installation](../docs/install.md).
It must never inherit a local display socket or run Niri with `--session`.
`/etc/niri-rdp/renderer.conf` selects `auto`, `hardware`, or `software`. `auto`
tries non-software EGL devices first, preferring NVIDIA devices that advertise
`EGL_NV_device_cuda`, then uses the surfaceless fallback. This uses Smithay's
`EGL_EXT_platform_device` support, which NVIDIA provides for headless rendering;
it needs no local display server, VT, seat, or DRM master. `hardware` fails startup
if no device can initialize instead of silently falling back. `software` retains
the original llvmpipe settings in the worker. Driver libraries and device access
must be available to the logged-in account. Logs identify the selected path.
Hardware rendering advertises linux-dmabuf formats and render-node feedback so
accelerated applications can submit GPU buffers to the compositor. The RDP bridge
still captures the final output through SHM and uploads it for NVENC.
The software namespace lab does not validate actual NVIDIA rendering performance.

On a disposable NVIDIA-capable test machine,
`lab/run-isolated.sh --nvidia-renderer env NIRI_RDP_LAB_ENCODER=nvenc python3 scripts/headless-integration.py`
requires hardware EGL, verifies NVIDIA identification and DMA-BUF advertisement,
and tests SHM capture/input with the NVENC RDP path. It exposes NVIDIA nodes and
DRM render nodes only, preserving isolation from physical modesetting devices.
The patch creates private `wayland-0` and HEADLESS-1 initially at 1920×1080, 60 Hz;
scale=1 and transform=normal are enforced by the supplied config. Capture defaults to existing wlr SHM screencopy with damage waits; an optional
DMA-BUF import path uses supported GPU buffers; input uses existing virtual keyboard/pointer
protocols. The bridge changes the HEADLESS-1 mode through existing
`zwlr_output_manager_v1` custom-mode configuration when a client connects or
requests a new size. Changes preserve the output, workspaces and applications,
update layout and announced capture geometry, and keep a single current mode.
Dimensions remain bounded to 1–8192 and refresh to 1–240 Hz. No RDP protocol code
lives in the compositor.

The patched virtual keyboard implementation routes remote key events through
Niri's normal XKB, focus, grab and keybinding handling. The old pinned Smithay
implementation sent events directly to `wl_keyboard` and maintained a separate
client keymap; it bypassed compositor shortcuts such as Mod+D. The compositor
now uses Niri's configured keymap, preserves the selected layout group during
remote modifier synchronization, and releases held keys on disconnect. Temporary
Unicode maps stay specific to the text device; the seat's map and modifiers
remain intact. Their events share the input-method queue with physical keys,
preserving order when a client sends a burst. The isolated integration fixture decodes the actual
Wayland XKB map and requires consecutive `abc123ABc`, Return and a Mod+D compositor
binding, in addition to capture, resize and disconnect checks.

Private remote startup publishes the compositor's application environment to its
private D-Bus bus synchronously before startup applications can activate services.
This hook runs only in a headless broker session and uses the fixed packaged
autostart helper. It serves Wayland events while waiting for input-method
readiness, so a daemon's startup roundtrips cannot block the compositor.

Input-method virtual keyboards are handled separately from remote physical
keyboards. Fcitx's forwarded keys reach applications without reentering its
own grab or applying XKB/keybindings twice. Both sealed, NUL-terminated bridge
keymaps and the pinned Smithay grab's unsealed text keymaps are accepted.

With Fcitx5 and Mozc installed, the opt-in disposable check validates inherited
profiles, forwarded typing, Ctrl+Space, Japanese and Katakana native Wayland composition,
and compositor startup readiness:

```sh
lab/build-app.sh
lab/run-isolated.sh --input-method-startup env NIRI_RDP_LAB_FCITX=1 \
  NIRI_RDP_LAB_NIRI=compositor/upstream/target/release/niri \
  NIRI_RDP_LAB_BRIDGE=bridge/target/release/niri-rdp-bridge \
  python3 scripts/headless-integration.py
```

The fixture's text-input protocol XML is copied from the pinned
`wayland-protocols` 0.32.13 crate with its upstream license intact; its hash is
recorded in `pins.json`. The test mounts checkout startup helpers only in the
private namespace and uses a disposable account database, bus and profile.

The supplemental `0002-remote-cursor-capture.patch` adds a private, headless-only
cursor protocol. It sends bounded RGBA shape FDs, hotspot, physical position and
visibility metadata; ordinary motion reuses the last shape. The frontend selects
it using negotiated RDP pointer limits and retains embedded-cursor fallback.
Apply both maintained patches in order. Explicit output scaling separates physical
capture pixels from logical application coordinates. See
[performance controls](../docs/performance.md) for configuration and acceptance.
