# Broker checkpoint — 2026-10-10

## Decisions

- Native C broker uses established Linux-PAM and libsystemd; no RDP protocol code.
- Separate frontend UID, kernel SO_PEERCRED, bounded binary Unix IPC with Wayland
  FD handoff. Fixed worker/executable/environment, final PAM identity authorized.
- Dedicated Arch PAM service, remote Wayland user session, no seat/VT, required
  logind metadata verification. One dedicated account / one connection; reject
  existing graphical sessions and unsupported PAM multi-prompt flows.
- Stock Niri/GNOME/GDM/GRD are untouched. Worker uses the coordinator's pinned
  maintained side-by-side `/usr/lib/niri-rdp/niri-headless` package.
- Frontend retains the control stream; disconnect/logout/crash triggers lifecycle
  cleanup. Parent/service logind cleanup supplements native process groups.
  Failed termination retains root-only records for 5s retries; logind can recover
  missing crash records. Async termination keeps directories until a later retry.
- The working host receives no package installation, PAM change, service
  enablement, account creation, listener or desktop change.

## Exact dependency versions inspected/built

| Dependency | Revision/version |
|---|---|
| Linux-PAM | Arch `pam 1.7.3-1`, pkg-config `1.7.3` |
| systemd/libsystemd | Arch `systemd 262-1`, pkg-config `262`; `sd_uid_get_sessions` implementation checked against upstream tag `v262` |
| GCC | Arch `gcc 16.2.1+r23+gd564253eb6c8-1` |
| glibc | Arch `glibc 2.44+r50+g1848099f063e-1` |

These are recorded build dependencies, not bundled binaries or claims that Arch
ABI versions can never change. Rebuild packages and repeat the acceptance suite
for upgrades. Exact compositor/RDP source pins live in the coordinator's
upstream lock manifest/checkpoints.

Primary API evidence: [pam_systemd](https://www.freedesktop.org/software/systemd/man/251/pam_systemd.html),
[sd-login source at v262](https://github.com/systemd/systemd/blob/v262/src/libsystemd/sd-login/sd-login.c).
Missing UID session state returns zero sessions; the broker does not treat
absence of an earlier login as an error. Query mode -1 includes closing sessions.

## Commands and results

- `make -C broker all check`: builds with GCC, `-Wall -Wextra -Werror`, `_FORTIFY_SOURCE=3`;
  sandbox result **8 native groups passed, 1 positive peer-credential check skipped**
  because sandbox `getsockopt(SO_PEERCRED)` returns EPERM. Fail-closed behavior
  tested in that environment.
- `./broker/broker-tests` outside the execution sandbox, unprivileged: **9 native
  groups passed, 0 skipped**. No real PAM or logind session created. Tests cover
  bounded/malformed protocol, zeroization, conversation limits, acquired-resource
  rollback, FD handoff, descendants surviving leader exit, mocked PAM init/auth/
  account failure and final identity authorization, fixed-executable launch
  rejection with PAM cleanup, actual SO_PEERCRED UID checks and safe session IDs.
- `make -C broker DESTDIR="$PWD/artifacts/broker-stage" install`: **passed**, artifacts
  staged only inside the repository. Binary, fixed worker, dedicated PAM policy,
  compositor config and system unit present. Packaging consumes the same target.
- `systemd-analyze verify broker/config/niri-rdp-broker.service`: **exit 0**, with
  environment socket-option restrictions printed by systemd. This verifies unit
  parsing, not runtime PAM privileges or logind behavior.
- ASan/UBSan build **compiled**. `ASAN_OPTIONS=detect_leaks=1 ./broker/broker-tests`
  in the sandbox **blocked** by LeakSanitizer's ptrace limitation; not a passed
  sanitizer gate. Normal native binaries rebuilt and tested afterward.

## Remaining acceptance work (pending, do not claim completed)

1. Disposable Arch systemd boot with no local login; valid account must create
   the checked seatless remote logind session, UID worker and separate desktop.
2. Actual wrong-password, denied/locked/expired account, malformed IPC and
   concurrent/same-UID graphical-session rejection with no resource leaks.
3. Real launch failure, logout, disconnect, compositor/holder/broker SIGKILL and
   service restart; assert session scope, bus/app descendants and private runtime
   cleanup. Especially audit crash during PAM registration and logind termination
   failures; intentional persistent user-manager units are outside native tests.
4. Windows mstsc controls the new Niri session through the private tunnel;
   physical monitor off/disconnected without suspending machine. Real-machine
   access required; native tests and Linux bridge tests do not meet this gate.
5. Rebuild/update/rollback on a disposable target and repeat all above before
   describing the packages as a validated boot-to-remote-login release.
