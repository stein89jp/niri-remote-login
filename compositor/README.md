# Maintained headless Niri dependency

This directory pins upstream Niri `main` at
`fc9e8c425e2267d877a65918f030c86f3c698179` (v26.04 plus later upstream work,
Smithay `8a270232f1bfc416c781273c98a3f52a812a0bba`) and carries a feature
series of thirteen patches. Patch 0001 is the unmerged headless backend of
upstream PR #3953 (head `0be60b7`), rebased onto that `main`; patches 0002–0013
are the Niri-RDP features, one per patch, so a future rebase fails per feature.
See [CHECKPOINT.md](CHECKPOINT.md) for decisions, exact results and remaining
machine tests, and [pins.json](pins.json) for exact revisions and hashes. The
previous PR #3953 pin and its two patches are recorded under `previous` in
pins.json for rollback. The normal Niri executable is not replaced.

| Patch | Feature |
|---|---|
| `0001-headless-backend` | PR #3953 virtual headless outputs, rebased |
| `0002-fail-closed-startup` | Fail startup without renderer/default output; bind `wayland-0` |
| `0003-output-bounds` | Virtual outputs 1–8192 px, at most four, 1–240 Hz |
| `0004-output-mode-change` | `zwlr_output_manager_v1` custom modes applied in place |
| `0005-egl-device-selection` | `NIRI_RDP_RENDERER` auto/hardware/software EGL device choice, render node |
| `0006-dmabuf-feedback` | Hardware linux-dmabuf global with render-node feedback |
| `0007-private-startup-hook` | Private session activation before autostart |
| `0008-keyboard-layout-group-sync` | Virtual keyboard modifier sync keeps Niri's layout group |
| `0009-unicode-keymap-restore` | Restore the seat keymap after a committed Unicode tap |
| `0010-remote-input-regressions` | Tests for the bridge's keyboard/pointer contract |
| `0011-cursor-motion-echo` | Cursor sessions do not echo a client's own virtual-pointer motion |
| `0012-cursor-dnd-icon` | Cursor captures include the drag-and-drop icon |
| `0013-headless-frame-pacing` | Fixed-phase simulated vblank; frame callbacks every refresh during redraws |

For a fresh source tree, obtain the archive URL in pins.json, verify its SHA256,
extract it into a disposable build directory, and apply every file listed in
`selected.patches`, in order, with `patch -Np1`. The series does not change
`Cargo.lock`. Build with `cargo build --release --locked --no-default-features`.
Run the regressions with
`cargo test --locked --no-default-features --lib backend::headless` and
`cargo test --locked --no-default-features --lib -- tests::remote_input tests::cursor_capture utils::private_startup`.
The headless startup and software-renderer tests need Mesa's EGL vendor
(llvmpipe). An Arch clean build environment needs Rust/git/base-devel plus
seatd, libinput, libdisplay-info, mesa, libglvnd, wayland, libxkbcommon, pango,
cairo, pixman and systemd-libs. An extracted seatd package is used only for this
workspace's lab.

For a prepared workspace, `upstream/` is the patched lab tree.
`lab/build-niri.sh --offline` and `lab/build-app.sh` rebuild local artifacts.
`lab/run-isolated.sh lab/capture-proof.sh` runs only in private namespaces,
using the Arch userspace libraries read-only and software rendering.
`lab/run-isolated.sh python3 scripts/headless-integration.py` exercises
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
if no device can initialize instead of silently falling back. `software` makes
the worker restrict glvnd to Mesa's EGL vendor and select llvmpipe; without the
vendor restriction NVIDIA's EGL could serve the surfaceless platform. Driver
libraries and device access must be available to the logged-in account. Logs
identify the selected path. Hardware rendering advertises linux-dmabuf formats
and render-node feedback so accelerated applications can submit GPU buffers to
the compositor, and so ext-image-copy-capture can offer DMA-BUF capture buffers.
The software namespace lab does not validate actual NVIDIA rendering performance.

On a disposable NVIDIA-capable test machine,
`lab/run-isolated.sh --nvidia-renderer env NIRI_RDP_LAB_ENCODER=nvenc python3 scripts/headless-integration.py`
requires hardware EGL, verifies NVIDIA identification and DMA-BUF advertisement,
and tests capture/input with the NVENC RDP path. It exposes NVIDIA nodes and
DRM render nodes only, preserving isolation from physical modesetting devices.
The series creates private `wayland-0` and HEADLESS-1 initially at 1920×1080,
60 Hz; scale=1 and transform=normal are enforced by the supplied config. Capture
defaults to SHM screencopy with damage waits; an optional DMA-BUF import path
uses supported GPU buffers; input uses the standard virtual keyboard/pointer
protocols. The bridge changes the HEADLESS-1 mode through `zwlr_output_manager_v1`
custom-mode configuration when a client connects or requests a new size.
Changes preserve the output, workspaces and applications, update layout and
announced capture geometry, and keep a single current mode. Dimensions remain
bounded to 1–8192 and refresh to 1–240 Hz. No RDP protocol code lives in the
compositor.

Upstream Niri and Smithay now route virtual keyboards through Niri's XKB,
focus, grabs and keybindings (Mod+D works remotely), release held keys when a
virtual keyboard is destroyed, let input-method forwarded keys bypass the
input method's grab, and map wheel notches to v120; the former private
keyboard rewrite and these fixes were dropped. The bridge sends its keyboard
with `no_keymap`, so it follows Niri's configured keymap and reloads. Patch
0008 keeps Niri's current layout group when the bridge synchronizes modifiers,
and patch 0009 restores the seat keymap after the bridge's temporary
single-key Unicode keymap, including when Fcitx relays it. Patch 0010 holds
the client-server regressions for this contract, including Mod+D, Unicode
commit with and without an input method, held-key release and sealed/unsealed
keymaps.

Private remote startup publishes the compositor's application environment to its
private D-Bus bus synchronously before startup applications can activate services.
This hook runs only in a headless broker session and uses the fixed packaged
autostart helper. It serves Wayland events while waiting for input-method
readiness, so a daemon's startup roundtrips cannot block the compositor.

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

The cursor comes from upstream's standard ext-image-copy-capture cursor
sessions; the former private `niri-rdp-cursor-v1` protocol and its patch were
removed. The bridge receives the cursor image, hotspot, position and
enter/leave from a cursor session on its own connection and forwards them as
RDP pointer updates within negotiated limits, keeping the embedded-cursor
fallback. Patch 0011 stops the compositor from echoing the bridge's own
virtual-pointer motion back as cursor positions, which would otherwise be
forwarded as RDP warps and snap the client's cursor back; compositor warps and
other pointers are still reported. Patch 0012 adds the drag-and-drop icon to
cursor captures, as the old protocol did. The bridge requires this series:
stock Niri without 0011 shows cursor snap-back that the bridge cannot detect,
so the bridge and compositor are packaged and upgraded together. Explicit
output scaling separates physical capture pixels from logical application
coordinates. See [performance controls](../docs/performance.md) for
configuration and acceptance.
