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
directory as `XDG_RUNTIME_DIR`, and `--config /etc/niri-rdp/headless.kdl`.
It must never inherit a local display socket or run Niri with `--session`.
The patch creates private `wayland-0` and HEADLESS-1 initially at 1920×1080, 60 Hz;
scale=1 and transform=normal are enforced by the supplied config. Capture uses
existing wlr SHM screencopy; input uses existing virtual keyboard/pointer
protocols. The bridge changes the HEADLESS-1 mode through existing
`zwlr_output_manager_v1` custom-mode configuration when a client connects or
requests a new size. Changes preserve the output, workspaces and applications,
update layout and announced capture geometry, and keep a single current mode.
Dimensions remain bounded to 1–8192 and refresh to 1–240 Hz. No RDP protocol code
lives in the compositor.
