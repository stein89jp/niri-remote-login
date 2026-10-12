# Acceptance on actual test machines

Record machine/kernel/package versions, exact package hashes, compositor and
IronRDP revisions, Windows build, mstsc version, GPU/driver, rendering mode, date
and each result. Store evidence without credentials. Automated disposable checks
are in `scripts/headless-integration.py`; they do not substitute for this matrix.

| Gate | Procedure and evidence | Current status |
| --- | --- | --- |
| Headless desktop bridge | Private namespace, no physical devices or inherited display; animated Wayland app, decoded RDP frame, key/pointer changes and releases | Automated real-app test passed |
| TLS broker transport | TLS-only FreeRDP credentials, connected Wayland FD transfer and lease EOF for each connection | Passed with namespace test broker; no Linux-account/PAM proof |
| Linux-account login | Valid dedicated account creates exactly one remote Wayland logind session of service `niri-rdp`, expected UID, no seat; invalid account/password creates none | Pending separate Arch machine |
| RDP login screen | Connect without a ClientInfo password; see the login screen without a compositor/session, reject a bad password, accept the Linux account, verify masked pixels, input, and disconnect detachment | Automated login tests; installed PAM and Windows checks pending |
| Authentication policy | Wrong password, unknown user, root, nonmember, locked/expired user, additional PAM prompt, account with local graphical session; confirm no app/session/private directory remains | Pending real PAM tests; native failure tests cover contracts |
| Cold boot | Enable system frontend, reboot test machine, do not log in locally, connect directly using Windows Remote Desktop; record `loginctl` identity and terminal `id` output | Pending |
| NVIDIA cold boot | Install release 14 or newer and the NVIDIA encoder override; reboot without a local graphical login or running `nvidia-smi`/FFmpeg first; require successful device preparation followed by `NVENC H.264 preflight passed` and an active frontend; repeat after another reboot | Explicit GPU/control/UVM requests and startup contracts checked; installed reboot pending |
| Windows | mstsc displays dedicated Niri terminal and sends keyboard, pointer, wheel and modifiers; test ordinary TLS negotiation, missing-password login screen, optional TLS-only profile, and certificate verification | Pending |
| Dynamic resolution | Connect at a non-default size; resize the client window in portrait and landscape, verify application reflow, sharp text and accurate pointer clicks without reconnecting | Namespace FreeRDP validation recorded in validation.md; Windows and installed-service behavior pending |
| NVIDIA encoding | Rebuild the current frontend, enable its NVIDIA drop-in, verify NVENC initialization and AVC420 graphics negotiation; decode changing frames, exercise input/reconnect, and record GPU encoder activity, bitrate and latency against bitmap mode | Release NVENC/FreeRDP namespace app/input/reconnect test passed; Windows, systemd permissions and complete-path performance pending |
| Audio/video delay | Windows 10 mstsc with remote playback enabled: play a clip with visible claps or simultaneous flash/tone for at least five minutes; repeat under constrained bandwidth and after a short network stall. Record observed sound offset, whether delay grows, recovery time, audible skips, and frontend bitrate/audio-delay journal entries | Automated timing/backpressure checks; actual client playback measurement pending |
| Codec fallback and hardware failure | With NVENC enabled, a client without AVC420 graphics support receives bitmap updates; unavailable/denied NVIDIA hardware produces a clear frontend startup failure; restore bitmap mode and verify login | Release namespace bitmap fallback passed with NVENC configured; actual service/device-denial checks pending |
| Physical monitor off | Keep test machine awake, power monitor off while moving typing in app remotely; record continued changing pixels/input, independent output and session identity | Pending |
| Monitor disconnected | Disconnect physical display, retain remote session and repeat interaction; separately test boot without attached monitor | Pending |
| Logout | Super+Shift+Q: RDP disconnects, PAM/logind session closes, all app/compositor/private D-Bus descendants exit, desktop directory removed | Pending real session checks |
| Compositor crash | SIGKILL test compositor PID; RDP closes promptly, broker cleans session/app/bus/directory; new login succeeds | Disposable RDP close tested; real broker cleanup pending |
| Persistent reconnect | Drop the network or close mstsc with an open app; reconnect within 45 seconds of broker-observed detachment to the same account, verify identical logind session ID/compositor PID and application state; repeat with frontend restart and held keys/buttons | Native broker and real-app input checks; installed PAM/logind and Windows resume pending |
| Resume authorization | Reject wrong password, expired/locked account and removed `niri-rdp-users` membership on resume; preserve the detached desktop without creating another session | Pending real PAM tests |
| Frontend/broker crash | Frontend stop/SIGKILL retains desktop for reconnect within the 45-second grace window; broker stop/SIGKILL ends desktops. Test `ExecStopPost` recovery and orphan scope teardown, restart and log in again | Pending |
| Input disconnect | Disconnect while key and button held; verify releases and no state carried into next connection | Automated real-app test passed |
| GNOME preservation | Compare GRD/GDM enablement/listeners and local session before/after test-machine experiment; do not alter working host | Packages have no GNOME configuration or stock Niri replacement |

Useful read-only observations on the test installation:

```sh
loginctl list-sessions
loginctl show-session SESSION_ID -p User -p Name -p Leader -p Service -p Type -p Class -p Remote -p Seat
systemctl status niri-rdp-broker.service niri-rdp-frontend.service
journalctl -b -u niri-rdp-nvidia-devices.service -u niri-rdp-broker.service -u niri-rdp-frontend.service --no-pager
nvidia-smi dmon -s u
ss -ltn 'sport = :33991'
pgrep -a -u niri-remote-test
find /run/niri-rdp/desktops -mindepth 1 -maxdepth 1 -type d
```

Disconnect retains the desktop for 45 seconds after broker-observed detachment.
Verify expiry fully closes the PAM/logind session and applications, and the next
login starts a fresh desktop after cleanup. Rejected resume authentication must
not extend the grace window. Also check cleanup after compositor logout/exit or
broker shutdown.

Expected after teardown: no project logind session, desktop directory, compositor,
private D-Bus or test application. A still-connected SSH/TTY administrator is not
an orphan project session. Broker socket and listener remain while services run.
The `nvidia-smi` observation applies only when testing NVIDIA encoding; encoder
activity during changing desktop content is useful hardware evidence, while an
encoder listed by FFmpeg alone proves only that the software was built with it.
Record the NVIDIA cold-boot service results before running either tool: NVIDIA
client utilities can create missing device nodes and mask a startup regression.
Inspect service/UID before terminating any session. Monitor-off is not suspend;
keep the test machine awake without modifying the working host's power policy.

Single-session frontend, Japanese JP106 physical-key input, normal transform and scale 1 are
the current contract. The frontend follows client-selected initial dimensions
and supports live single-display resolution requests through Display Control.
Rebuild both packages; see the [resolution contract](../bridge/README.md#dynamic-resolution). No
multi-monitor, clipboard or remote IME composition claim is made. Session audio
has private PipeWire capture and RDP packet tests; installed-service playback
through Windows mstsc, including reconnect, still requires actual-machine validation.
Broker mode retains
desktops for authenticated reconnect; installed PAM/logind and Windows validation
of that behavior remains pending.
The optional host encoder uses H.264 AVC420; it does not enable host-side NVDEC
or ensure that the RDP client uses hardware decoding. Compositor rendering prefers
hardware EGL devices with software fallback. Record the selected renderer and
client decoding mode separately; capture still uses SHM and CPU color conversion.
Completion requires every requested actual-machine gate to pass and its evidence
to be recorded; pending rows mean the boot-to-remote-login goal is incomplete.
