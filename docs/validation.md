# Validation record

Additional implementation evidence is tracked in
[compositor checkpoint](../compositor/CHECKPOINT.md),
[bridge checkpoint](../bridge/CHECKPOINT.md),
[broker checkpoint](../broker/CHECKPOINT.md), and the repository checkpoints.
The foundation record below is preserved as historical evidence. Current
actual-machine gates are listed in [acceptance](acceptance.md).

## Packaged integration, 2026-10-10

Both x86_64 Arch packages were built with `makepkg`, inspected, and extracted
inside the workspace. No pacman installation or service start was performed.
The extracted binaries passed real headless Niri-to-FreeRDP tests in a disposable
Arch namespace: animated Wayland application frames, two keyboard/pointer
roundtrips, bad-password rejection, release after a held-input disconnect, and
RDP closure within five seconds of compositor SIGKILL while the frontend survived.

The same application tests also passed in the frontend's production TLS-only
mode with a namespace-only broker fixture. That proves ClientInfo credential
validation, real Wayland FD binding, and lease EOF for all five accepted
connections. The fixture is not PAM/logind. FreeRDP retries bad credentials;
the test requires positive broker rejections and zero accepted bad logins.

The bridge has 10 passing unit tests and strict clippy validation. The broker has
9 passing native test groups with no skips, using mock PAM and no real login.
The patched compositor's bounded headless-output regression passes. Its optional
client/compositor lifecycle fixtures stalled and remain unresolved; the full
upstream compositor suite is not claimed. The 13 original prototype/PoC/test
source files retain their foundation SHA-256 hashes.

Package checks reject GNOME/GDM changes, stock Niri replacement, automatic
service enablement, install hooks and unsafe archive entries. Staged executable
paths were substituted only in temporary copies for `systemd-analyze verify`;
both units passed, without being started. See
[integration checkpoint](../checkpoints/001-integration.md) for commands, pins,
limitations and package receipts. Actual PAM/logind, Windows, fresh boot and
physical-monitor tests are still pending.

Executed on Arch Linux on 2026-10-10. All changes are workspace files or temporary
test artifacts. No packages, system units, PAM policies, GNOME settings, accounts,
firewall rules, or monitor/power settings were changed.

## Completed

- **7 Python session-isolation tests pass.** They use real child processes and
  a private D-Bus: no inherited display/socket/credential environment, private
  directories for two workers, visible startup failure, worker and descendant
  termination, cleanup, and refusal to run the harness as root.
- **5 Rust adapter unit tests pass.** Fixed loopback endpoint/peer policy,
  framebuffer stride/size/pixels and input-driven changes, extreme pointer
  coordinates, extended-key separation, and input release after disconnect.
- **4 startup failure tests pass.** Missing/empty credentials, missing or invalid
  TLS identity, forbidden CLI overrides and TLS key logging all fail closed.
- **1 real FreeRDP transport integration test passes.** A temporary synthetic IronRDP
  server at `127.0.0.1:33990` used TLS/CredSSP and random throwaway credentials.
  The displayless FreeRDP C client rejects an incorrect password specifically
  through NLA, verifies HYBRID/TLS negotiation, decodes the 640×480 bitmap,
  sends keyboard/pointer input and checks its pixel feedback, verifies key
  release, then repeats the exchange on a fresh connection. This does not test
  resuming a persistent desktop session. The server and
  TLS/credential artifacts are cleaned up. A preliminary `xfreerdp3` auth-only
  test also passed before this stronger test replaced it.

These are 17 automated tests across the three suites. The integration client
uses FreeRDP's software GDI without opening any X11 or Wayland display. Its
certificate-verification bypass is limited to the managed test's ephemeral
self-signed loopback certificate.

The sandbox initially denied Unix/network socket creation. Successful socket
tests were run with explicit execution permission outside the sandbox. Private
D-Bus uses a Unix socket; the RDP listener is loopback-only and temporary.

## Foundation limits (before the real bridge implementation)

No real Niri process, capture client, virtual keyboard/pointer bridge, PAM login
broker, or logind remote session was started. Niri is not installed on this host;
its behavior was investigated using pinned source. No Windows `mstsc.exe`
interoperability, physical monitor power-off, GPU/software rendering, resize,
remote account authentication, or same-UID desktop coexistence has been tested.
The proof of concept establishes transport/adapter and process-isolation
foundations, not completion of the target desktop.

The RDP prototype uses the **published** `ironrdp-server =0.13.0` and its lockfile.
The research snapshot is a later development revision with materially different
APIs despite the same manifest version. Its new credential/session binding APIs
must not be assumed to exist in the published crate used by the prototype.

## Reproduction

From the repository root:

```sh
python3 -m unittest discover -s tests -v
CARGO_HOME="$PWD/poc/.cargo-cache" cargo test --locked --offline --manifest-path poc/Cargo.toml
python3 poc/tests/startup.py
POC_RDP_BINARY="$PWD/poc/target/debug/niri-rdp-transport-poc" python3 -m unittest discover -s tests -v
```

The local Cargo cache is ignored. On a fresh checkout, dependency acquisition
must precede `--offline`; keep the Cargo home and target within the workspace.
See [PoC instructions](../poc/README.md) for explicit TLS setup, launch, and
integration commands. Default test discovery does not implicitly start an RDP
service.


## Dynamic resolution validation (2026-10-10)

The current bridge and maintained headless compositor patch support native
client-selected initial resolution and single-display live resize. The displayless
FreeRDP client negotiates 1001×701, then sends Display Control layouts for
702×1001 → 1002×701 → 702×1001 on the same connection. For every size, the harness
checks decoded frame geometry against Niri IPC's native and logical output size,
then requires application keyboard and center-pointer click/release feedback.
This verifies application reflow at native resolution as well as RDP geometry.

Three complete isolated TLS broker fixture suites passed: bitmap, NVENC AVC420,
and NVENC with a bitmap-only client. Receipts are
`artifacts/dynamic-resolution-bitmap.json`,
`artifacts/dynamic-resolution-nvenc.json`, and
`artifacts/dynamic-resolution-nvenc-fallback.json`. They also retain wrong-password
rejection, reconnect, held-input disconnect release, compositor-crash termination,
frontend survival and broker lease EOF checks. The last fallback run uses the
final disconnect responsiveness checks. The fixture supplies a real compositor
FD but does not authenticate through PAM.

All 27 bridge unit tests passed, including resize-before-pixels ordering,
cancellation, unsupported layouts, odd-size video decoding, encoder recreation,
and graphics surface reset. Both compositor headless regressions passed; the
maintained patch applies to the pristine pinned archive, and both debug binaries
build. Python regression discovery passed seven tests with the opt-in synthetic
transport test skipped. Socket tests and isolated integration required execution
outside the restrictive sandbox. No host service was installed or enabled.

Reproduce after building the patched compositor, bridge and strict FreeRDP probe:

```sh
cargo test --manifest-path bridge/Cargo.toml --locked --offline
lab/run-isolated.sh env NIRI_RDP_LAB_TLS_CONTRACT=1 \
  NIRI_RDP_LAB_REPORT=artifacts/dynamic-resolution-bitmap.json \
  python3 scripts/headless-integration.py
lab/run-isolated.sh --nvenc env NIRI_RDP_LAB_ENCODER=nvenc \
  NIRI_RDP_LAB_TLS_CONTRACT=1 \
  NIRI_RDP_LAB_REPORT=artifacts/dynamic-resolution-nvenc.json \
  python3 scripts/headless-integration.py
lab/run-isolated.sh --nvenc env NIRI_RDP_LAB_ENCODER=nvenc \
  NIRI_RDP_LAB_GFX=0 NIRI_RDP_LAB_TLS_CONTRACT=1 \
  NIRI_RDP_LAB_REPORT=artifacts/dynamic-resolution-nvenc-fallback.json \
  python3 scripts/headless-integration.py
```

See the [resolution contract](../bridge/README.md#dynamic-resolution) for bounds
and rebuilding both packages. Windows mstsc, installed-service operation and
real PAM/logind boot acceptance remain separate-machine gates.

## Simplified Arch host installation (2026-10-10)

`packaging/arch/niri-rdp/PKGBUILD` builds the compositor, bridge, broker, and
setup helpers as one `niri-remote-login` package. Source checksum verification,
portable `makepkg --allsource` export and verification outside the checkout,
pinned compositor patch application, package assembly, and staged package
structure checks passed. The assembly check reused existing compiled binaries;
a complete fresh combined release build and actual pacman installation were not
performed. The legacy split-package source exporter also excludes makepkg's
generated trees and cached inputs.

Python discovery passed 13 tests, with the opt-in synthetic transport test
skipped. The broker's 10 mock-PAM test groups passed. The bridge Rust suite
passed 35 library tests plus one frontend test, including direct listener configuration, restricted credential-less
login binding, keyboard/focus handling, password masking, rejected login, and
stream cancellation. Staged systemd units passed `systemd-analyze verify`; only
executable paths were redirected to the extracted package for this check. TLS
tests use temporary files and exercise real OpenSSL generation, identity
preservation, private-key permissions, and incomplete/symlink identity rejection.

Fresh bridge and FreeRDP probe builds passed three isolated real-desktop suites:

- `artifacts/easy-install-direct-tls-integration.json`: direct ClientInfo
  credentials through the broker protocol to a real Niri desktop.
- `artifacts/easy-install-login-integration.json`: omitted ClientInfo credentials
  reach the login form, wrong form credentials are rejected, and physical US
  scancodes submit valid credentials before real desktop interaction.
- `artifacts/easy-install-login-nvenc-integration.json`: the same login sequence
  switches to actual NVIDIA NVENC AVC420 with client-decoded desktop frames.

All suites exercise real application frames/input, client-selected initial
resolution, native/live portrait and odd-height resize, reconnect, held-input
release, compositor failure, frontend survival, and closure of each accepted
broker lease. The namespace broker supplies a real compositor socket but uses
fixture credentials: these results establish RDP/login/desktop transport,
not installed Linux PAM or logind sessions.

Reproduce the login-screen suites after building the patched compositor, bridge,
and strict FreeRDP probe:

```sh
lab/run-isolated.sh env NIRI_RDP_LAB_TLS_CONTRACT=1 \
  NIRI_RDP_LAB_LOGIN_SCREEN=1 \
  NIRI_RDP_LAB_REPORT=artifacts/easy-install-login-integration.json \
  python3 scripts/headless-integration.py
lab/run-isolated.sh --nvenc env NIRI_RDP_LAB_ENCODER=nvenc \
  NIRI_RDP_LAB_TLS_CONTRACT=1 NIRI_RDP_LAB_LOGIN_SCREEN=1 \
  NIRI_RDP_LAB_REPORT=artifacts/easy-install-login-nvenc-integration.json \
  python3 scripts/headless-integration.py
```

Microsoft's [RDP negotiation request specification](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/902b090b-9cb3-4efc-92bf-ee13373371e3)
allows a client offering CredSSP to also offer TLS. The pinned acceptor selects
the shared TLS protocol and supplies ClientInfo credentials even when they are
empty. This supports the ordinary-client/login-screen design; it is not evidence
that a specific Windows build interoperates. Actual Windows mstsc, installed
PAM/logind boot login, and physical-monitor acceptance remain pending.
