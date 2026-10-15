# Compositor checkpoint — 2026-10-15 (rebase onto upstream main)

Ownership: `compositor/` and `lab/`. No host package, PAM, service, GNOME, GDM,
power policy or physical device configuration was changed.

## Current pin and series

PLAN2 rebased the compositor from PR #3953 head `0be60b7` (plus two private
patches) onto upstream Niri `main` `fc9e8c425e2267d877a65918f030c86f3c698179`
(v26.04 + 193 commits), Smithay `8a270232f1bfc416c781273c98a3f52a812a0bba`.
The decision record is `artifacts/design/niri-rebase-decision.md`. PR #3953 is
still open and unmerged; patch 0001 carries it rebased (two trivial conflict
hunks plus the new `max_bpc` IPC field). Archive SHA256
`d7e8b62923429eba4187990cb94a9466e3df8840632f6302cd1c0fe20fa450d9`; Cargo.lock
SHA256 `a3774f2ee8da8ff96a1a7e9c4f795578fb60918724e1a4feb4618375f1809bc6`,
unchanged by the series. Per-patch hashes are in `pins.json`; the old pin is
under `previous` there for rollback.

The twelve patches (see [README](README.md)) add 2,815 and remove 66 lines,
of which about 1,150 are tests (`src/tests/remote_input.rs`,
`src/tests/cursor_capture.rs`) and 161 are upstream PR documentation
(`docs/Virtual-Outputs.md`). Dropped because upstream now provides them: the
keybinding-routed virtual keyboard rewrite, held-key release on disconnect,
Fcitx forwarded-key grab bypass and wheel v120. The private
`niri-rdp-cursor-v1` protocol was replaced by upstream ext-image-copy-capture
cursor sessions plus the small no-echo patch 0011 and the drag-icon patch 0012.

Files created by the series, which makepkg's `prepare()` moves aside when
reusing a src tree: `docs/Virtual-Outputs.md`, `src/utils/private_startup.rs`,
`src/tests/remote_input.rs`, `src/tests/cursor_capture.rs`.

The archive plus the series applied with `patch -Np1` is byte-identical to the
scratch branch `rebase/main-fc9e8c42` at `cfaad5f1`. `upstream/` was refreshed
to that tree, and `lab/build-niri.sh --offline` passed (debug). With the lab
sysroot and a private `XDG_RUNTIME_DIR`, `--lib backend::headless` passed 8
tests, `tests::remote_input` 9, `tests::cursor_capture` 7, and the combined
`-- tests::remote_input tests::cursor_capture utils::private_startup` run 19.
Lab integration (capture, input, Fcitx/Mozc, NVIDIA/NVENC, clipboard,
performance) and the installed Windows checks are PLAN2 Phase 5.

The packages ship the compositor and bridge in lockstep: the combined package
provides `niri-rdp-headless=26.04.remote2-1`, and the split bridge package
depends on `niri-rdp-headless>=26.04.remote2-1`, because the bridge now uses
cursor sessions and `no_keymap` virtual keyboards and needs patch 0011.

The sections below are the original 2026-10-10 PR #3953 checkpoint, kept as
history. Their patch names, hashes and test counts describe the previous pin.

---

# History: compositor checkpoint — 2026-10-10 (PR #3953 pin, superseded)

## Decision and exact source

`git ls-remote https://github.com/niri-wm/niri.git refs/pull/3800/head refs/pull/3953/head HEAD`
returned baseline `2c82b77a7116a1241ea2fd41dd008ff00b609aae`, PR3800
`7fd585dd44ed67644977dd9561bb6e8b3837f8f7`, and PR3953
`0be60b742762905b7bee1ea67bb5a2fb00859bf0`. The selected checkout is
`upstream/`, detached at the last revision. The PRs remain upstream work to
maintain; their presence is not proof of upstream support.

Both implementations have virtual-output rendering timers and frame callbacks.
PR3800 additionally enumerates physical libinput devices using `XDG_SEAT`
(default `seat0`), advertises DMA-BUF and includes GBM/portal integration. This
broader physical-input/device behavior is unnecessary for the initial seatless
SHM bridge. PR3953 has an explicit `NIRI_BACKEND=headless` path, renderer startup,
1920×1080 60 Hz default output, virtual-output IPC and safe failed DMA-BUF
imports, without physical seat input acquisition. We selected PR3953, using SHM
capture and existing Wayland virtual-input protocols only.

The archive, lockfile, Smithay and extracted libseat pins of that time are
under `previous` in `pins.json`. The Niri archive SHA256 was
`680374cae233d2ed1e0f8474aeb017e36872b30b9e291ce43b1521724b8fe2a9`.
Cargo.lock SHA256 was
`e71d9d5b104d297bc1185cebb4d7eb203f374c668c5739e41927e15a0b1e668c`.
Selected Smithay: `ff5fa7df392cecfba049ffed55cdaa4e98a8e7ef`. Exact read-only
Arch library/toolchain package versions observed during this build are also in
`pins.json`; production packages use compatible Arch dependencies rather than
replacing these libraries.
Alternative Smithay: `22571baa20d34d71092942dbb520c4e3fbbd6263`.

## Maintained local patch

`patches/0001-remote-headless-bounds-and-startup.patch`:

- Fail compositor startup when renderer/default-output setup fails; a socket
  without usable capture is not accepted as session success.
- Bound virtual outputs to dimensions 1–8192, count four and refresh 1–240 Hz;
  reject exhausted identifier counters before mutating output state.
- Bind `wayland-0` deterministically only in the headless backend. The broker
  supplies a private 0700 `XDG_RUNTIME_DIR`; normal desktop backend selection
  keeps upstream automatic socket names.
- Regression test exercises invalid requests with no state changes, capacity
  refusal, removal, capacity recovery, identity monotonicity and removal of the
  final output.
- Headless output configuration applies custom modes through the existing wlr
  output-management protocol. It preserves output identity and workspaces,
  updates frame pacing, layout, IPC and announced geometry, rejects dimensions
  outside 1–8192 and refresh outside 1–240 Hz, and retains one current mode.
  The resize regression exercises three mode changes and invalid requests,
  checking output identity, bounded mode storage and IPC/logical geometry.

Compile with `--locked --no-default-features`, and launch without `--session`.
This excludes D-Bus/systemd user environment import and portal integration.
Install this binary only at `/usr/lib/niri-rdp/niri-headless`; do not replace the
normal Niri package or register another local desktop session.

## Exact build and tests

The host is Arch; installed read-only native development libraries were usable
except libseat. Downloaded `seatd-0.9.3-1-x86_64.pkg.tar.zst` (SHA256
`b4b52430149e66aaeb4cb474f4b756b5c862adecf6e8ecbf358d67470d2b35de`) into
`lab/downloads/` and extracted it into `lab/sysroot/`, never through pacman.
Its pkg-config file was relocated to the extracted prefix. Cargo home is
`compositor/cargo-home/`; all source/build files are repository-local.

Initial build failed at missing libseat/pkg-config, then at missing link search
path because the extraction's prefix still said `/usr`. Relocating the prefix
and `cargo clean -p libseat-sys` fixed this. No host installation was used.

```sh
lab/build-niri.sh --offline
lab/build-app.sh
env CARGO_HOME="$PWD/compositor/cargo-home" \
    PKG_CONFIG_PATH="$PWD/lab/sysroot/usr/lib/pkgconfig" \
    LD_LIBRARY_PATH="$PWD/lab/sysroot/usr/lib" \
    cargo test --manifest-path compositor/upstream/Cargo.toml \
    --locked --offline --no-default-features --lib backend::headless
lab/run-isolated.sh timeout 8 compositor/upstream/target/debug/niri --config lab/headless.kdl
lab/run-isolated.sh lab/capture-proof.sh
```

Debug Niri build: PASS. Headless lifecycle regression: PASS, 1 test, 194 other
upstream tests filtered; harmless upstream unused import warning. Startup:
PASS, logs show `HEADLESS-1` and private `wayland-0`; timeout sends SIGTERM and
Niri exits cleanly. Renderer works with `LIBGL_ALWAYS_SOFTWARE=1`,
`GALLIUM_DRIVER=llvmpipe`, no `/dev/dri`, no physical input devices and no
inherited DISPLAY, WAYLAND_DISPLAY, local runtime directory or session bus.

`lab/run-isolated.sh` uses bubblewrap with private PID/user/mount/network/IPC
namespaces, a read-only `/usr`, minimal private `/dev`, `/tmp` and `/run`,
repository-only writable `/work`, private runtime 0700 and no host home or
system service manager. It uses the current Arch userspace; it is **not** a
fresh-boot VM or a real PAM/logind session. Elevated execution here is needed
because the outer tool sandbox disallows nested namespace creation.

`lab/dynamic-app` is a genuine Wayland xdg_toplevel SHM client. Its bottom stripe
animates on compositor frame callbacks. Application state colors match the RDP
integration client's BGRA contract: baseline `(180,40,30,255)`, A pressed
`(30,40,180,255)`, mouse pressed `(40,180,30,255)`. It acknowledges events in
linebuffered stdout and reports frame count/size. `xdg-shell.xml` is vendored
from locked wayland-protocols 0.32.12 with its original copyright/license.

Capture probe initially proved changing SHM capture hashes, actual app A and
right-Control press/release acknowledgments, but its corner/control/mouse
sequence did not reach the app button listener. Reordering the probe so the
center click occurs before control-key/corner input fixed this. Final run PASS:
frame hashes `97c639ed8d383b02` → `0a679dd9ba0cced4`, and actual application
`KEY 30 1/0`, `KEY 97 1/0`, `BUTTON 272 1/0` all observed. Logs in
`lab/results/` retain the final successful run. Root's separate
`lab/run-isolated.sh python3 scripts/headless-integration.py` real RDP/FreeRDP
integration already passed decoded changing application pixels, A and button
color acknowledgment/release and two reconnects. See root integration logs for
that stronger result; do not infer Windows compatibility from it.

## Remaining gates

The release results and binary hash below describe the original startup/bounds
patch before dynamic-resolution support. The current patch was rebuilt in debug
mode; both headless lifecycle and resize regressions pass, and the patch applies
cleanly to the pinned source archive. Its checksum is under `previous` in `pins.json`.

Release build PASS: `cargo build --release --locked --offline
--no-default-features`, `CARGO_BUILD_JOBS=2`, elapsed 4m01s. Unstripped binary
`upstream/target/release/niri` SHA256
`f6e7f7035cc643dfcf4a2c2450f03d2596e9dc6950d22c926d3f569311dedbd7`.
`git apply --check --reverse` confirms the checkout exactly contains the patch.
The optimized release binary also passes
`lab/run-isolated.sh env NIRI_RDP_LAB_NIRI=compositor/upstream/target/release/niri lab/capture-proof.sh`:
changing hashes `a85941e61e6ec6be` → `8ac152ca6629767e`, with all expected app
key/button press/release acknowledgments. Final source lifecycle regression
rerun again PASS (1 test). Shell syntax and pins JSON checks PASS.

The additional upstream compositor fixture test `tests::remove_output` hung
waiting for a Wayland roundtrip for over five minutes and its exact process was
terminated. A bounded `timeout 15 cargo test --locked --offline
--no-default-features --lib tests::fullscreen:: -- --test-threads=1` run passed
24 pure layout tests then hung in its first compositor/client fixture test and
timed out (exit 124). These fixture tests were not modified or removed. The
complete upstream suite is not claimed passing; investigate the fixture issue
before extending compositor work. Repository foundation tests are coordinated
by the root agent. The new lifecycle regression and real compositor/capture/
input integration are the passing acceptance evidence. Bounded resize and repeated output lifecycle with capture
are further validation work, beyond the one-output baseline.

Fresh Arch boot with no user logged in, real PAM/logind success and failure,
Windows mstsc.exe, physical display off/unplugged, account/device access and
host-service upgrade validation remain pending actual disposable test machines.
Nothing here claims those tests passed.
