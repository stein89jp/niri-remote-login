# Niri compositor findings and upstream work

Implementation update: a maintained pinned PR #3953 build and disposable
real-app capture/input evidence are now available under [compositor](../compositor/README.md)
and [validation](validation.md). The research below describes the original stock
baseline; its statements about unstarted compositor tests are historical.

Implementation update (2026-10-15): the compositor is rebased onto upstream
`main` `fc9e8c425e2267d877a65918f030c86f3c698179` (Smithay `8a270232`) with a
12-patch feature series, 0001 being PR #3953's headless backend. The cursor now
uses upstream's ext-image-copy-capture cursor sessions instead of a private
protocol. Upstream now routes virtual keyboards through Niri's keybindings,
releases held keys and supports input-method forwarded keys and v120 wheel
values, so those local patches were dropped. See the
[rebase record](validation.md#niri-rebase-onto-upstream-main-2026-10-15).

Research date: 2026-10-10. Baseline: Niri commit `2c82b77a7116a1241ea2fd41dd008ff00b609aae`, manifest version 26.4.0, Smithay commit `19c16d3e34172fcc8fb3d293bcae563b0a19ea70`. Selected source files were inspected locally under `research/upstream/niri` (listed with URLs and SHA-256 digests in `research/upstream/manifest.json`; the copies are not tracked); this is a research snapshot, not a complete buildable checkout. Findings below derive from that snapshot unless explicitly attributed to a PR. No compositor, capture client, input injector, or network listener was started.

## Feasibility

The intended architecture is feasible, but the inspected upstream baseline does not expose a production headless-session launch path. Niri already has useful capture and synthetic-input mechanisms. The central missing dependency is a supported independent headless backend with virtual-output lifecycle and reliable frame scheduling. A broker must additionally create a login session without depending on an existing Wayland desktop. See [login and RDP integration](login-and-rdp.md) for that separate responsibility.

Niri uses Smithay rather than wlroots. Setting `WLR_BACKENDS=headless` does not select its backend. Its internal `State::new` chooses `Headless` only when an explicit boolean is true; otherwise inherited `WAYLAND_DISPLAY`, `WAYLAND_SOCKET`, or `DISPLAY` selects the nested Winit backend, and absence of those selects TTY. The ordinary executable passes `headless=false`. Thus launching a nested Niri window inside GNOME is useful for later adapter testing but cannot establish the requested login independence. [Pinned manifest](https://github.com/niri-wm/niri/blob/2c82b77a7116a1241ea2fd41dd008ff00b609aae/Cargo.toml), [backend selection](https://github.com/niri-wm/niri/blob/2c82b77a7116a1241ea2fd41dd008ff00b609aae/src/niri.rs#L728), [executable startup](https://github.com/niri-wm/niri/blob/2c82b77a7116a1241ea2fd41dd008ff00b609aae/src/main.rs#L186).

## Existing interfaces

| Interface | What exists in the inspected baseline | Role in this project |
| --- | --- | --- |
| Niri JSON IPC | Output/window/workspace queries, event stream, actions, transient output configuration | Discovery and lifecycle observability; not a pixel transport or arbitrary keyboard/mouse injection interface |
| Output IPC | On/off, modes/custom modelines, scale, transform, position, VRR and bit depth | Configures output names; does not create a virtual connector in this revision |
| Internal headless backend | Surfaceless EGL/GLES renderer initializer and `add_output` creating a Smithay output | Starting point for compositor patch; currently intended for tests |
| `zwlr_screencopy_manager_v1` | Damage-aware capture and SHM/dma-buf capture rendering | Simplest first pixel bridge; begin with SHM and bounded CPU copies |
| `ext_output_image_capture_source_manager_v1` / `ext_image_copy_capture_manager_v1` | Output sources and explicit capture/cursor sessions | Preferred evolving capture interface if supported by the adapter and tested revision |
| Virtual keyboard / wlr virtual pointer | Registered compositor globals | Inject through the remote compositor's Wayland socket and seat, without physical `/dev/uinput` devices |
| PipeWire screencast integration | Feature-gated compositor capture path | Later efficient capture option; introduces portal/PipeWire/session dependencies |

The `Request` and `OutputAction` enums enumerate the available IPC operations. The server returns `OutputWasMissing` when a requested output name is absent and schedules a transient configuration change; it supplies no virtual-output constructor in this baseline. [IPC schema](https://github.com/niri-wm/niri/blob/2c82b77a7116a1241ea2fd41dd008ff00b609aae/niri-ipc/src/lib.rs), [output request handling](https://github.com/niri-wm/niri/blob/2c82b77a7116a1241ea2fd41dd008ff00b609aae/src/ipc/server.rs#L402).

## Headless implementation details

`Headless::new` has no renderer. `add_renderer` initializes `EGLSurfacelessDisplay`, an EGL context, GLES resources and shaders. `add_output` publishes an output with a 60 Hz mode and calls `Niri::add_output`. `primary_render_node` returns `None`, `import_dmabuf` is unimplemented, and animation continuation has a FIXME. These details require deliberate initialization and testing before real applications or dma-buf clients can safely use this backend. Surfaceless EGL may use GPU or software rendering depending on the driver; neither render-node nor software-driver behavior has been validated here. [Headless source](https://github.com/niri-wm/niri/blob/2c82b77a7116a1241ea2fd41dd008ff00b609aae/src/backend/headless.rs).

The backend's `render` sends presentation feedback and updates redraw bookkeeping rather than presenting a physical framebuffer. **This does not prove capture is impossible.** `Niri::redraw` subsequently calls `render_for_screencopy_with_damage`, `render_for_image_copy_capture`, and cursor capture through `with_primary_renderer`. The SHM screencopy branch renders into an XRGB8888 client buffer. The likely initial bridge is therefore renderer initialization plus a virtual output, capture via SHM, and an independent output frame clock. Actual changing-pixel capture remains an integration gate. [Redraw and capture path](https://github.com/niri-wm/niri/blob/2c82b77a7116a1241ea2fd41dd008ff00b609aae/src/niri.rs#L4739), [SHM capture rendering](https://github.com/niri-wm/niri/blob/2c82b77a7116a1241ea2fd41dd008ff00b609aae/src/niri.rs#L5965).

A remote compositor process should own only virtual outputs and its own seat. Its render clock must continue when the physical monitor is off or absent, without a local VT, KMS connector, or DRM-master ownership. A working render device may still be necessary. Avoid physical-output DPMS as a remote-session mechanism: the requirement is independent rendering, not mirroring the local output after switching it off.

## Input, capture access, and coordinate contract

Niri installs virtual keyboard, virtual pointer, screencopy, output capture-source, and image-copy globals using `client_is_unrestricted`, which checks the Wayland client's `restricted` flag. This is a coarse privileged-protocol filter, not broker identity authentication or a per-RDP-session capability. Keep the adapter inside the dedicated remote session and connect only to its socket; ordinary applications should not gain new privileges merely to make the bridge work. For stronger separation, evaluate a compositor-issued connected Wayland FD or narrowly scoped authenticated adapter capability. [Protocol registration and filter](https://github.com/niri-wm/niri/blob/2c82b77a7116a1241ea2fd41dd008ff00b609aae/src/niri.rs#L2391).

The adapter needs a tested conversion contract: RDP scancodes/extended flags to Linux keycodes, an XKB keymap for the virtual keyboard, modifiers and lock-state synchronization, Unicode input policy, absolute pointer coordinates into the selected output's logical space, transform/fractional-scale handling, wheel deltas, and release of held keys/buttons on disconnect. Do not equate an RDP Unicode event with a hardware key press. Single-output sessions simplify the first experiment. Test upper/lower bounds and all corners again after resize; attach input to the same identity and output generation as capture.

libei provides a client input transport; a compositor must implement the EIS server side and decide when to accept emulation. Installing libei alone does not add that compositor support. A future libei route is an alternative only after verifying compositor and portal support; it is not needed to exercise the existing virtual protocols. [libei/libeis API documentation](https://libinput.pages.freedesktop.org/libei/api/index.html).

## Existing upstream work to reuse

[PR #3800](https://github.com/niri-wm/niri/pull/3800) was open when checked. It proposes headless and TTY virtual outputs, creation/removal through IPC and configuration, with headless remote-session demonstrations. Its discussion also includes output-removal, portal naming, driver and portability issues. Reuse and test a pinned branch instead of assuming it is upstream or production-ready.

[PR #3953](https://github.com/niri-wm/niri/pull/3953) was also open. It proposes `NIRI_BACKEND=headless`, dynamic virtual outputs, and fixes including headless dma-buf import and virtual-output frame callbacks. Its advertised functionality closely matches the compositor prerequisite. Compare both branches against the inspected baseline before selecting a patch; these proposed commands are absent from that baseline.

## Proposed upstream patch sequence

1. **Headless startup:** supported explicit backend selection; initialize renderer and default/configured virtual output; clear errors when rendering cannot initialize; no attempt to acquire the physical seat or nested display. Keep test-only constructors distinct from production startup.
2. **Output lifecycle:** create/remove/resize virtual outputs with stable identity, bounded dimensions/count/refresh rate, IPC/events, configuration, and correct client hotplug behavior. Cover removal while disabled, removing the last output, resize during capture, and cleanup after failure.
3. **Frame scheduling:** independent timer-driven frame clock, damage/animation continuation, cursor and drag-icon callbacks, correct presentation feedback, and bounded idle work. Confirm that monitor power and unrelated physical-output hotplug do not alter remote rendering.
4. **Rendering/capture:** support SHM applications/capture first; safely reject unavailable dma-buf imports instead of panicking. Then test render-node discovery, dma-buf import/export and synchronization, software fallback, image-copy cursor behavior, and locked-session capture behavior.
5. **Adapter authorization:** existing virtual protocols can serve a dedicated single-user PoC. Add a narrower capability only if isolation tests show a gap; document the trust model and revoke injected input state when the adapter disappears.

RDP negotiation, credentials, PAM/logind session creation, transport encryption, and reconnect policy belong in the external RDP server/broker. Niri should provide compositor primitives, not contain a new RDP wire implementation.

## Acceptance tests before claiming monitor-off remote login

The repository's process-isolation and synthetic adapter tests establish only those foundations; they do not test a real broker or Niri desktop. Subsequent VM tests must prove changing application pixels and input acknowledgements in a separate Niri process, no inherited desktop/display dependency, no physical display device ownership, bounded resize, held-input release, capture termination on output removal, and compositor-crash cleanup. Repeat with no physical connector and with the VM display detached. Finally start from no logged-in user and verify a PAM/logind remote Wayland session is created and removed correctly. A manual Windows `mstsc.exe` pass is still necessary for interoperability; neither mocked frames nor Wayland unit tests demonstrate it.
