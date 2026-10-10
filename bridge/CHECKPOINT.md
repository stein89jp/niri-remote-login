# Bridge checkpoint — 2026-10-10

## Decisions and exact dependencies

- RDP is entirely upstream IronRDP, git revision
  `bf019ca502d1e52616853a5ded68c885cec1e322`. crates.io `ironrdp-server=0.13.0`
  was rejected for this implementation because it lacks the credential validator
  and authenticated desktop binder hooks needed by the PAM/session broker.
- Minimum Rust 1.94. Actual compile/test: Arch Rust
  `rustc 1.99.0 (b940084d7 2026-09-28)`.
- Direct Wayland/XKB pins: `wayland-client=0.31.14`,
  `wayland-protocols-wlr=0.3.12`, `wayland-protocols-misc=0.3.12`,
  `xkbcommon=0.9.0`, `tempfile=3.20.0`. Every transitive exact version and
  crates.io checksum is in `Cargo.lock`; all Git IronRDP crates use one revision.
- SHM screencopy + virtual keyboard/pointer reuse Niri's protocols. Selected
  `HEADLESS-1` output must have normal transform, scale=1 and pointer v2 output
  mapping. Anonymous buffers bounded to <=4096 dimensions/stride, at most 30 fps,
  no dma-buf, physical device injection, inherited Wayland or custom RDP code.
- Frontend fixed `127.0.0.1:33991`; both modes explicitly
  `ConnectionPolicy::Reject`; no UDP/preemption/cookies. Broker owns Linux
  authorization/session; frontend starts desktop capture only after auth.
- Broker mode TLS-only ClientInfo credentials -> NRB1 PAM broker, retaining a
  lease until disconnect/worker failure. Arbitrary PAM passwords cannot provide
  the static NTLM secret for current default library NLA. Windows TLS-only
  profile and secure tunnel still require actual mstsc testing. Disposable mode
  library NLA uses disposable current-UID credentials only.
- Broker provides exactly one CLOEXEC Wayland FD; fragmented replies supported,
  malformed credentials, FD count and truncation rejected. Active stop closes
  lease immediately even if library retains Desktop handler clones. Worker
  failures close the lease independently of those clones.

## Commands and results

Only repository/temp builds and foreground disposable execution; no package,
PAM, systemd, GDM/GNOME, device or host configuration change performed.

```sh
CARGO_HOME=/tmp/niri-rdp-bridge-cargo cargo fetch --manifest-path bridge/Cargo.toml
CARGO_HOME=/tmp/niri-rdp-bridge-cargo cargo check --manifest-path bridge/Cargo.toml --locked --offline
CARGO_HOME=/tmp/niri-rdp-bridge-cargo cargo build --manifest-path bridge/Cargo.toml --locked --offline
CARGO_HOME=/tmp/niri-rdp-bridge-cargo cargo test --manifest-path bridge/Cargo.toml --locked --offline
CARGO_HOME=/tmp/niri-rdp-bridge-cargo cargo clippy --manifest-path bridge/Cargo.toml --locked --offline --all-targets -- -D warnings
```

Fetch required approved network access; Cargo cache is disposable `/tmp`, not
host Cargo home. Check and debug build passed. Ten tests passed (zero failures):
scancode E0 identity/unknown codes, malicious SHM layout rejection, padding/
vertical-inversion/color conversion, loopback listener, credential limits,
fragmented FD passing + CLOEXEC + broker EOF, auth rejection/no session,
active stop/lease EOF with handler clones retained, failed compositor startup/
lease EOF, terminal dead/closed capture stream (no recoverable-error hotloop). Unix socket tests required approved unsandboxed execution because the
outer sandbox rejects UnixListener.bind with EPERM. All-target clippy with
warnings denied passed after routine style fixes.

The final disposable capture/input probe passed inside the isolated Arch
namespace (no /dev/dri or inherited local display):

```sh
lab/run-isolated.sh bash lab/capture-proof.sh
```

Actual Niri app pixel hashes changed `97c639ed8d383b02` ->
`0a679dd9ba0cced4`. The app acknowledged KEY30 1/0, KEY97 1/0 and
BUTTON272 1/0. An earlier probe interleaved Control and corner motion before
clicking and did not produce button observations; reordering the center click
before Control/corners resolved this probe sequencing issue. Logs retained
under lab/results/capture.log and app.log (ignored generated evidence).

Root's disposable real RDP/app integration passed normal NLA/TLS desktop,
keyboard/button explicit release and wrong-password rejection. An added held-key
and held-button disconnect/reconnect case exposed lost teardown releases; this
was not counted passed. Worker cleanup now releases input, destroys virtual
input objects and awaits a bounded 500 ms wl_display.sync acknowledgement before
closing its socket. A flush alone handed bytes to the socket but did not prove
Niri consumed final releases before a readable+hangup event. Debug rebuilt and
clippy passed; root rerun passed held key/button release and successful fresh
connection. App observed KEY30 0 and BUTTON272 0 after disconnect.

Root's compositor-kill test then exposed an upstream API contract mistake:
IronRDP treats `Err(next_update)` as recoverable and retries, causing immediate
errors to form an async hotloop. Desktop updates now use terminal `Ok(None)` for
dead workers/watch-channel closure; upstream dispatch maps this to connection
disconnect. Added async regression, debug rebuilt/clippy passed; final real crash
retry passed: SIGKILL of active compositor closed the RDP client within 5 seconds
and the frontend process survived to serve another connection.

The final full integration command passed inside the disposable Arch namespace:

```sh
lab/run-isolated.sh python scripts/headless-integration.py
```

Results: wrong disposable NLA credentials rejected; two real Niri/app RDP
roundtrips decoded changing app pixels and keyboard/button input responses;
frame animation and actual app event acknowledgment passed; disconnect with A
and left-button held produced releases and a fresh connection restored baseline;
compositor SIGKILL terminated the client within 5 seconds while frontend
survived. These are real SHM/application/FreeRDP checks, not synthetic frames or
Windows/PAM boot proof.

## Production TLS-only broker transport contract

After the NLA disposable check, the coordinator added a namespace-only NRB1
broker fixture supplying a real Niri Wayland FD. Current debug bridge broker
mode passed real TLS-only FreeRDP ClientInfo authentication flow, two app/frame/
input roundtrips, held disconnect release/reconnect, compositor crash closure,
and lease EOF for all five accepted connections. The fixture is deliberately
not PAM/logind and does not authenticate Linux accounts.

First retry failed only because the assertion required exactly one bad-password
broker request; FreeRDP retries failed TLS authentication and the fixture recorded
two positive rejections, zero accepts. The passing run temporarily adjusted
fixture assertions in memory (`fixture.rejected == 1` -> `>= 1`, final `!= 1`
-> `< 1`) while leaving accepted/closed counts exact. Exact command:

```sh
lab/run-isolated.sh env NIRI_RDP_LAB_TLS_CONTRACT=1 python -c 'import sys; sys.path.insert(0, "/work/scripts"); path="/work/scripts/headless-integration.py"; source=open(path).read().replace("fixture.rejected == 1", "fixture.rejected >= 1").replace("fixture.rejected != 1", "fixture.rejected < 1"); exec(compile(source, path, "exec"), {"__name__":"__main__", "__file__":path})'
```

No bridge authentication change was needed. Safe lifecycle error diagnostics
were added to stderr without credential values/tracing; the C test client must
set `AutoLogonEnabled=TRUE` to deliver TLS-only ClientInfo credentials. Root
will make the equivalent fixture assertion correction and rerun the release
packages. TLS rejection currently surfaces to FreeRDP as transport failure, so
positive broker rejection evidence is required, not merely a nonzero client exit.

## Remaining acceptance work

- PAM/logind on separate disposable Arch test machine, no local user session.
- Windows `mstsc.exe`, fresh boot, physical-monitor off/disconnected and crash/
  logout resource cleanup on actual machines: pending access; cannot be claimed
  passed by mock or namespace-only tests.
- Arbitrary keyboard layouts/IME/Unicode, Pause/E1, dynamic resize/input contract,
  clipboard/audio, lock-screen and compositor output lifecycle regressions.
- Maintained patched Niri Arch package plus separately prepared service/broker
  packages and upgrade/uninstall documentation are coordinated by root agent.
