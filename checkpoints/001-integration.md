# Installable integration checkpoint — 2026-10-10

The deliverable is an experimental pair of x86_64 Arch packages and matching
source build bundles for a separate test installation. Nothing was installed on
the working host. No account, PAM policy, system service, GNOME Remote Login,
GDM, firewall, power policy or stock Niri binary was changed. All listeners used
temporary loopback endpoints, with the real-desktop test in a private network
namespace. Git metadata is unavailable; `001-integration.json` records source
integrity, and delivery `SHA256SUMS` records the exact package/bundle hashes.

## Decisions and dependency revisions

- Selected Niri PR #3953 at `0be60b742762905b7bee1ea67bb5a2fb00859bf0`,
  Smithay `ff5fa7df392cecfba049ffed55cdaa4e98a8e7ef`. Archive SHA-256
  `680374cae233d2ed1e0f8474aeb017e36872b30b9e291ce43b1521724b8fe2a9`;
  upstream lockfile `e71d9d5b104d297bc1185cebb4d7eb203f374c668c5739e41927e15a0b1e668c`.
  Build with `--no-default-features`, no `--session`, private `wayland-0` socket,
  one normal scale-1 output and software llvmpipe baseline. This avoids physical
  input acquisition and global systemd-user/D-Bus display environment changes.
- Evaluated alternative PR #3800 at `7fd585dd44ed67644977dd9561bb6e8b3837f8f7`,
  Smithay `22571baa20d34d71092942dbb520c4e3fbbd6263`; not selected because it
  initializes seat0 libinput and adds broader GPU/portal integration. Selection
  is for this tested SHM baseline, not a claim that either patch is upstream.
- Maintained patch `compositor/patches/0001-remote-headless-bounds-and-startup.patch`
  SHA-256 `ca40b7d756cc9b01ff75095392e8da382e6ceef2883ae7976cba288ededde4d8`:
  renderer startup failure propagation, output dimension/refresh/count bounds,
  counter overflow rejection and private socket selection. Package
  `niri-rdp-headless` installs `/usr/lib/niri-rdp/niri-headless` alongside stock
  Niri. Its source archive has no Git metadata, so the binary reports unknown
  commit; the package source pins identify it exactly.
- IronRDP git `bf019ca502d1e52616853a5ded68c885cec1e322` supplies credential
  validation and connection binding APIs absent from published server 0.13.0.
  No custom RDP protocol code. Production broker mode uses library TLS-only RDP;
  disposable transport mode uses NLA with random test credentials. Fixed
  `127.0.0.1:33991`, single connection, no preemption/resume/UDP.
- Adapter dependencies include wayland-client 0.31.14, Wayland wlr/misc 0.3.12,
  xkbcommon 0.9.0 and tempfile 3.20.0. `bridge/Cargo.lock` pins the entire graph.
  Captures real SHM frames and sends virtual keyboard/pointer events; no synthetic
  production framebuffer. Rendering/input supports one 1920×1080 output, US
  physical-key input, scale 1 and normal transform. No resize, IME, clipboard,
  audio or persistent desktop claim.
- Native broker uses Linux-PAM and logind, authorized frontend SO_PEERCRED,
  dedicated allowed-account group, bounded password-only conversation, graphical
  session exclusion and one login per UID. Wayland FD plus live Unix lease binds
  the frontend to the dedicated worker. PAM/logind lifecycle and crash recovery
  are implemented, but have not run against a real PAM/logind boot session.
- Separate agent ownership: compositor/lab, bridge, broker. Coordinator owns
  packages, documentation and combined verification. Component checkpoints
  preserve detailed development decisions and failure evidence.

Read-only build ABI: rust/cargo 1.99.0; gcc `16.2.1+r23+gd564253eb6c8-1`;
glibc `2.44+r50+g1848099f063e-1`; pam 1.7.3-1; systemd/systemd-libs 262-1;
dbus 1.16.2-1; FreeRDP `2:3.32.1-1`; OpenSSL 3.6.5-1; Wayland 1.26.0-1;
libxkbcommon 1.13.2-1; bubblewrap 0.13.0-1. Remaining native compositor library
versions are recorded in `compositor/pins.json`. Missing seatd 0.9.3-1 was
downloaded and extracted into `lab/sysroot`, never installed; SHA-256
`b4b52430149e66aaeb4cb474f4b756b5c862adecf6e8ecbf358d67470d2b35de`.
Rebuild on a disposable Arch builder if the target's ABI differs.

## Commands and results

Preserved baseline commands/results are in `000-foundations.md`: 7 Python,
5 Rust, 4 startup tests and 1 FreeRDP synthetic transport test pass. All 13
baseline source/test files match the foundation inventory; none was rewritten.

```sh
CARGO_HOME=/tmp/niri-rdp-bridge-cargo cargo test --locked --offline --manifest-path bridge/Cargo.toml
CARGO_HOME=/tmp/niri-rdp-bridge-cargo cargo clippy --locked --offline --manifest-path bridge/Cargo.toml --all-targets -- -D warnings
make -C broker check
```

Results: 10 bridge tests pass; strict clippy passes; 9 native broker groups pass,
0 skipped outside the execution sandbox. Tests use fake PAM, not real accounts.
The new patched `backend::headless` regression passes (1 test, 194 filtered).
24 pure layout/fullscreen cases pass. Optional compositor/client roundtrip
fixtures hung and timed out; their investigation and the full upstream suite
remain pending. Broker sanitizer leak checking was blocked by sandbox ptrace
restrictions; ordinary native checks passed.

```sh
python3 scripts/prepare-release.py
# Package builds in the generated directories, without -i/-s:
CARGO_NET_OFFLINE=true CARGO_BUILD_JOBS=2 \
  CARGO_TARGET_DIR=/home/stein/Documents/My-Software/Niri-RDP/bridge/target \
  makepkg --force --nodeps --noconfirm
```

`makepkg` built both real packages, including their check stages. Existing
workspace Cargo caches were linked into the generated build `src/cargo-home`;
these cache links are not delivered. The headless build additionally used
`compositor/upstream/target`, `PKG_CONFIG_PATH=lab/sysroot/usr/lib/pkgconfig`
and `LD_LIBRARY_PATH=lab/sysroot/usr/lib` (absolute paths in actual invocation).
The extracted libseat pkg-config prefix was relocated to the workspace. The
`--nodeps` exception avoided host dependency installation; required libraries
were inspected/read-only. A separate clean-chroot build remains pending because
devtools/QEMU/container runtimes are unavailable and host installation is forbidden.

Initial package attempts failed on an incorrect offline-cache link and makepkg's
exported CFLAGS overriding aws-lc jitterentropy's required -O0; the cache link
was fixed and only the Rust build/test commands unset CFLAGS/CXXFLAGS. Final
builds passed. `makepkg` warns that a Rust binary embeds a source path; that is
debug/panic metadata, not a runtime source-directory dependency. Dependency
license/notice inventories are installed in both packages. No namcap result is
claimed; namcap is unavailable and was not installed.

```sh
python3 scripts/validate-packages.py \
  artifacts/release/niri-rdp-headless/niri-rdp-headless-26.04.remote1-1-x86_64.pkg.tar.zst \
  artifacts/release/niri-remote-login/niri-remote-login-0.1.0-1-x86_64.pkg.tar.zst
lab/run-isolated.sh env \
  NIRI_RDP_LAB_NIRI=artifacts/package-root/usr/lib/niri-rdp/niri-headless \
  NIRI_RDP_LAB_BRIDGE=artifacts/package-root/usr/lib/niri-rdp/niri-rdp-bridge \
  NIRI_RDP_LAB_REPORT=artifacts/packaged-integration.json \
  python3 scripts/headless-integration.py
lab/run-isolated.sh env \
  NIRI_RDP_LAB_NIRI=artifacts/package-root/usr/lib/niri-rdp/niri-headless \
  NIRI_RDP_LAB_BRIDGE=artifacts/package-root/usr/lib/niri-rdp/niri-rdp-bridge \
  NIRI_RDP_LAB_TLS_CONTRACT=1 \
  NIRI_RDP_LAB_REPORT=artifacts/packaged-tls-integration.json \
  python3 scripts/headless-integration.py
```

Package inspection/extraction passes without installation: both package identities,
expected files, ELF executables, no unsafe paths/links/setuid/install hooks,
no GNOME/GDM changes, stock Niri replacement or service enablement. Dynamic
libraries resolve against read-only Arch userspace plus extracted libseat. All
three ELF executables have no RPATH/RUNPATH into the build workspace.
Two temporary unit copies with only executable paths changed to workspace-stage
paths pass `systemd-analyze verify --man=no`; this does not start units or prove
service sandbox compatibility with real logins.

Both extracted-package real-app integrations pass: wrong disposable credentials,
two changing-frame keyboard/pointer roundtrips, animated application event
acknowledgments, held-key/button release on disconnect and fresh reconnect,
and compositor SIGKILL closing active RDP within five seconds while the frontend
survives. The TLS-only run additionally passes NRB1 connected-Wayland-FD handoff
and lease EOF for all five accepted logins. Its namespace test broker is not PAM.
FreeRDP retries rejected TLS logins; assertions require at least one positive
rejection and zero bad-login accepts. Test certificate bypass exists only in the
throwaway displayless client, not normal installation instructions.

The isolated namespace has private network/devices/runtime and read-only Arch
userspace, no inherited local display or physical GPU. It is not an independent
fresh boot with systemd/logind. Tool sandbox socket/namespace restrictions
required authorized test execution outside that sandbox; no host configuration
or installation was performed. Temporary test processes/credentials were cleaned.

## Delivery and dependent work

`artifacts/installable/` contains the two packages, exact source build bundles,
Niri upstream source tar/maintained patch, checkpoint/docs and validation receipts.
`SHA256SUMS` covers every delivered file. The portable archive is
`artifacts/niri-remote-login-arch-0.1.0.tar.gz`; its adjacent checksum identifies
the entire delivery. `source-manifest.json` and Cargo lockfiles identify source
inputs. Install, uninstall and upgrade/rollback instructions are reviewable now.

The coordinator assembles the portable delivery with
`python3 scripts/assemble-delivery.py` after package inspection and both runtime
proofs. It verifies the built packages' `.BUILDINFO` PKGBUILD hashes against the
delivered PKGBUILDs and package hashes against the inspection receipt. Build
caches, source checkout symlinks and private runtime credentials are excluded.

The next required milestone is on actual test machines: install the artifacts
on a separate Arch Niri installation, validate Linux-account authentication and
seatless logind sessions, then run Windows mstsc through a private tunnel after
boot with no local login. Validate monitor off/disconnected, authentication
policy failures and logout/compositor/frontend/broker crash cleanup. Record
package hashes and machine evidence per `docs/acceptance.md`. Those gates remain
pending; the requested demonstrated boot-to-remote-login workflow is incomplete.
