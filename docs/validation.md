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
