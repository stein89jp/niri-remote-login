# Remote login and RDP integration research

Implementation update: [bridge](../bridge/README.md) uses the pinned development
IronRDP APIs and [broker](../broker/README.md) implements the PAM/logind lifecycle.
Linux real-desktop transport is tested; actual PAM/logind, boot, mstsc and monitor
acceptance remains pending. The following is the original research record.

Research date: 2026-10-10. This document describes a proposed system; it does not claim that a Niri RDP desktop has been demonstrated. No host services, PAM configuration, or GNOME settings were changed for this research.

## Recommendation

Build a compositor adapter and a separate session broker around an existing RDP implementation. Use IronRDP for an isolated Rust proof of concept because its inspected server API exposes desktop updates, input callbacks, credential validation, and post-authentication session binding. Keep FreeRDP as the alternative if Windows interoperability or production graphics requirements outweigh the Rust integration benefit. A server library handles wire protocol and negotiation; project code handles session policy and the Niri display/input bridge. Do not implement RDP PDUs or security protocols here.

The first real desktop experiment belongs in a VM or equivalent disposable environment, with a dedicated test user, a private configuration and runtime directory, and a loopback listener on a nonstandard port. Windows can later reach that loopback listener through an explicitly configured private tunnel. Do not take port 3389, alter GDM, reuse GNOME Remote Login credentials, or run upstream example servers with their defaults.

## What GNOME actually provides

GNOME distinguishes sharing an existing desktop, independently starting a single-user headless desktop, and multi-user Remote Login. Remote Login authenticates initial access using system-wide RDP credentials, then presents a graphical login screen for the user's own credentials. Its documented deployment enables both GDM and the system GNOME Remote Desktop service. Thus a login-independent system service and compositor/session creation are distinct requirements. [GNOME configuration](https://github.com/GNOME/gnome-remote-desktop/blob/main/docs/configuration.md).

The upstream design uses four participants: RDP client, system dispatcher, per-session handover daemon, and GDM. GDM creates a headless greeter and then a headless user session. The dispatcher associates routing identifiers with sessions, performs RDP redirection, and passes an accepted connection FD to the appropriate handover daemon. Caller/session matching and one-time credentials guard these transitions. This is a useful architectural reference, but adopting its exact GDM/Mutter coupling would substantially expand the Niri project. [GNOME developer's Remote Login Design](https://blogs.gnome.org/joantolo/category/remote-desktop/), [handover interface](https://github.com/GNOME/gnome-remote-desktop/blob/main/src/org.gnome.RemoteDesktop.xml).

Inference: a direct-login Niri broker can initially omit the graphical greeter and redirection chain. Authenticate the user, start one separate headless Niri process, then bind that desktop to the accepted RDP connection. GNOME-like graphical login, arbitrary PAM conversation, and reconnect routing are later features. Starting a nested compositor inside the existing local session proves only adapter behavior, not login independence.

## RDP implementation choices

| Implementation | Reusable part | Suitability and limits |
| --- | --- | --- |
| FreeRDP | C peer/listener API, graphics codecs, security and virtual channels | Strong reference implementation; already used by desktop projects. Requires a C/Rust FFI boundary or C adapter and explicit session integration. Its shadow server is not itself a Niri login broker. |
| IronRDP | Rust `ironrdp-server`, acceptor, codecs and channel crates | Good Rust adapter fit. Inspected revision has credential validator and post-auth binder. Windows client compatibility and particular graphics/channel combinations need direct testing. |
| xrdp | Existing server plus sesman/PAM session machinery | Valuable login/session reference and potentially reusable architecture. Standard documented desktop backends are xorgxrdp or Xvnc; installing xrdp does not provide a native Niri Wayland backend. A new backend would be needed. |
| KRdp | FreeRDP-based desktop library with capture/input integration | Useful integration reference and reports Windows client support. Its normal deployment is a logged-in user service; its README explicitly describes an existing login or SDDM autologin requirement. It is not a drop-in independent Niri session launcher. |

Sources: [FreeRDP project](https://github.com/FreeRDP/FreeRDP), [peer API](https://pub.freerdp.com/api/structrdp__freerdp__peer.html), [shadow API](https://github.com/FreeRDP/FreeRDP/blob/master/include/freerdp/server/shadow.h), [IronRDP project](https://github.com/Devolutions/IronRDP), [xrdp upstream](https://github.com/neutrinolabs/xrdp), [KRdp README](https://raw.githubusercontent.com/KDE/krdp/master/README.md). KRdp documentation contains some historical limitations alongside newer settings; use its code and the exact package version before treating every README limit as current.

### Pinned IronRDP findings

Local inspected source: `research/upstream/IronRDP`, revision `bf019ca502d1e52616853a5ded68c885cec1e322`; these are selected downloaded files, not a full buildable checkout. `ironrdp-server` manifest identifies version 0.13.0 and Rust minimum 1.94. Do not assume crates.io releases have every API in this revision.

- `RdpServerInputHandler` receives scancode/Unicode keyboard and absolute/relative mouse events; the adapter still must map these into the selected compositor input mechanism.
- `CredentialValidator::validate_received` distinguishes Client Info credentials from CredSSP-delegated credentials. Rejection/backend errors reject setup.
- `ConnectionBinder` installs identity-bound display/input objects after authentication and negotiated size, before input dispatch and desktop updates. Resize retains those bound objects.
- TLS and Hybrid/CredSSP modes are available. The example falls back to no security without certificate/key: production and PoC launchers must instead refuse missing TLS identity.
- The default serve loop has one active desktop connection with queue/reject/preempt policies. Build a distinct server/session instance per concurrent desktop; do not assume a shared server object isolates users.
- TLS preemption can evict a session before credential validation; use reject/queue initially. Automatic reconnect cookies bypass the validator; leave them disabled until resume authorization and revocation are implemented.

Evidence: [pinned server source](https://github.com/Devolutions/IronRDP/blob/bf019ca502d1e52616853a5ded68c885cec1e322/crates/ironrdp-server/src/server.rs), [input callbacks](https://github.com/Devolutions/IronRDP/blob/bf019ca502d1e52616853a5ded68c885cec1e322/crates/ironrdp-server/src/handler.rs), [example security setup](https://github.com/Devolutions/IronRDP/blob/bf019ca502d1e52616853a5ded68c885cec1e322/crates/ironrdp/examples/server.rs). These claims come from local source inspection; the browser could not retrieve this exact commit page.

NLA does not turn a PAM password checker into an NTLM/Kerberos verifier. CredSSP first requires an SSPI authentication backend, and only then may expose delegated credentials for authorization/PAM. Shadow passwords are not NTLM password-equivalent secrets. Choose a documented supported SSPI backend, or use RDP TLS with credentials inside the protected Client Info exchange and verify the intended `mstsc.exe` configuration. A GNOME-style access credential followed by a PAM graphical greeter is another later option. Do not silently disable NLA policy on Windows. The upstream design discussion explains this distinction but predates some inspected APIs. [IronRDP credential-handling discussion](https://github.com/Devolutions/IronRDP/issues/1357).

## Broker, session ownership, and no prior login

The planned broker is a small privileged session manager, isolated from the unprivileged RDP parser and renderer. It accepts requests only over authenticated local IPC. It resolves an allowed account using PAM's resulting identity, performs account authorization, opens a PAM session, starts the desktop under that UID/GID and supplementary groups, and owns cleanup. The RDP process cannot submit arbitrary executables or environment variables. Credentials must not appear in arguments, logs, or persistent state.

Proposed PAM lifecycle: `pam_start`, bounded conversation, `pam_authenticate`, `pam_acct_mgmt`, credential establishment, `pam_open_session`; retain the PAM handle until logout, then close session, delete credentials, and end PAM. A failed intermediate step unwinds every acquired resource. PAM may change the authenticated username, so bind to its final account. Authentication and session opening are separate operations; session setup can require privilege. [Linux-PAM authentication API](https://raw.githubusercontent.com/linux-pam/linux-pam/master/doc/man/pam_authenticate.3.xml), [session API](https://raw.githubusercontent.com/linux-pam/linux-pam/master/doc/man/pam_open_session.3.xml).

On an Arch test system, a dedicated future PAM service should use the distribution's applicable policy and `pam_systemd`, without editing existing GDM/GRD stacks. Register a remote Wayland user session with desktop `niri`, remote host metadata, and no physical seat/VT. Use PAM-provided runtime/session environment; do not create a fake `/run/user/$UID` or depend on linger/autologin. `pam_systemd` creates the runtime directory, session scope, and user manager when needed. This is the mechanism for starting from zero local sessions. [systemd PAM documentation source](https://raw.githubusercontent.com/systemd/systemd/main/man/pam_systemd.xml), [logind session API](https://raw.githubusercontent.com/systemd/systemd/main/man/org.freedesktop.login1.xml).

Policy proposal: initially allow one remote session per dedicated account and reject accounts already running a graphical session. `/run/user/$UID` and the systemd user manager are shared across a user's sessions. Consequently, unique Wayland and Niri IPC socket names alone do not isolate desktop services. Global environment imports, `graphical-session.target`, portal selection, PipeWire, keyrings, notifications, and single-instance application D-Bus names need a separate audit. A private `dbus-run-session` can isolate selected app bus names but is not proof that systemd-activated desktop services are isolated. Same UID means shared files and process access; separate sessions are not a security boundary. [runtime-directory semantics](https://www.freedesktop.org/software/systemd/man/251/pam_systemd.html).

## Monitor-off requirement

A true remote session must render into its own virtual output, with its own frame scheduling and input seat, without owning the physical KMS output or requesting DRM master on the local seat. Turning off, unplugging, or disabling the physical monitor must not remove the remote output or stop its frame clock. A render node/software renderer may still be required; display independence does not imply GPU independence. These are architecture requirements and test criteria, not verified Niri behavior.

Physical monitor off also does not mean host suspended. The disposable integration test should remain awake by an explicit test policy; do not alter the working machine's power policy. Never test this requirement by switching off the working GNOME display via project code.

## Phases and acceptance gates

1. **Offline policy PoC:** fake authentication/session/capture/input backends, no listener and no privilege. Test rejected credentials never start a desktop, ownership binding, rollback on launch failure, independent remote output identity, input release on disconnect, and resource bounds. Passing these tests proves contract behavior only.
2. **RDP library interop PoC:** disposable loopback TLS listener using pinned library code and a synthetic framebuffer. Test successful/wrong credentials, frame receipt, keyboard/pointer round trips, bounded dimensions, disconnect cleanup, and `mstsc.exe` over a private tunnel. No local desktop capture or PAM yet.
3. **Separate Niri desktop:** disposable VM with the required headless-output/input support. Start dedicated Niri process and test an app's changing pixels plus input acknowledgement. Repeat with virtual display disconnected and no physical output. Verify software rendering as a fallback and ensure local-display resources are untouched.
4. **Actual remote login:** VM with test PAM/logind broker, no user logged in initially. Assert remote Wayland session identity, privileges, runtime directory, process scope, account-policy failures, compositor crash cleanup, and remote logout. Test a wrong password causes no session/process/runtime leak.
5. **Usable desktop:** resize/reconnect, per-session authorization, clipboard/audio by explicit policy, portals, keyring, suspend behavior, multiple distinct UIDs, and supported same-UID policy. Add packaging and system units only after these gates. Continue binding only loopback/private explicitly approved access.

Upstream priorities: Niri's explicit headless/virtual-output lifecycle and renderer/frame-clock support are the central dependency; secure remote-input capability and capture of those outputs follow. Exact gaps are tracked in the compositor research. IronRDP already has the inspected auth/binding hooks, so do not propose adding them again; upstream work is conditional on actual failing interop tests, stable released APIs, or missing SSPI backend integration. Generic broker functionality can remain external to Niri. GNOME/GDM modifications are unnecessary for this route.
