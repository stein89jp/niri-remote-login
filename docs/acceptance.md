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
| RDP login screen | Connect without a ClientInfo password; see the login screen without a compositor/session, reject a bad password, accept the Linux account, verify masked pixels, input, and disconnect cleanup | Automated login tests; installed PAM and Windows checks pending |
| Authentication policy | Wrong password, unknown user, root, nonmember, locked/expired user, additional PAM prompt, account with local graphical session; confirm no app/session/private directory remains | Pending real PAM tests; native failure tests cover contracts |
| Cold boot | Enable system frontend, reboot test machine, do not log in locally, connect directly using Windows Remote Desktop; record `loginctl` identity and terminal `id` output | Pending |
| Windows | mstsc displays dedicated Niri terminal and sends keyboard, pointer, wheel and modifiers; test ordinary TLS negotiation, missing-password login screen, optional TLS-only profile, and certificate verification | Pending |
| Dynamic resolution | Connect at a non-default size; resize the client window in portrait and landscape, verify application reflow, sharp text and accurate pointer clicks without reconnecting | Namespace FreeRDP validation recorded in validation.md; Windows and installed-service behavior pending |
| NVIDIA encoding | Rebuild the current frontend, enable its NVIDIA drop-in, verify NVENC initialization and AVC420 graphics negotiation; decode changing frames, exercise input/reconnect, and record GPU encoder activity, bitrate and latency against bitmap mode | Release NVENC/FreeRDP namespace app/input/reconnect test passed; Windows, systemd permissions and complete-path performance pending |
| Codec fallback and hardware failure | With NVENC enabled, a client without AVC420 graphics support receives bitmap updates; unavailable/denied NVIDIA hardware produces a clear frontend startup failure; restore bitmap mode and verify login | Release namespace bitmap fallback passed with NVENC configured; actual service/device-denial checks pending |
| Physical monitor off | Keep test machine awake, power monitor off while moving typing in app remotely; record continued changing pixels/input, independent output and session identity | Pending |
| Monitor disconnected | Disconnect physical display, retain remote session and repeat interaction; separately test boot without attached monitor | Pending |
| Logout | Super+Shift+Q: RDP disconnects, PAM/logind session closes, all app/compositor/private D-Bus descendants exit, desktop directory removed | Pending real session checks |
| Compositor crash | SIGKILL test compositor PID; RDP closes promptly, broker cleans session/app/bus/directory; new login succeeds | Disposable RDP close tested; real broker cleanup pending |
| Frontend/broker crash | Stop/SIGKILL each test service with active session; test `ExecStopPost` recovery and orphan scope teardown, restart and log in again | Pending |
| Input disconnect | Disconnect while key and button held; verify releases and no state carried into next connection | Automated real-app test passed |
| GNOME preservation | Compare GRD/GDM enablement/listeners and local session before/after test-machine experiment; do not alter working host | Packages have no GNOME configuration or stock Niri replacement |

Useful read-only observations on the test installation:

```sh
loginctl list-sessions
loginctl show-session SESSION_ID -p User -p Name -p Leader -p Service -p Type -p Class -p Remote -p Seat
systemctl status niri-rdp-broker.service niri-rdp-frontend.service
journalctl -b -u niri-rdp-broker.service -u niri-rdp-frontend.service --no-pager
nvidia-smi dmon -s u
ss -ltn 'sport = :33991'
pgrep -a -u niri-remote-test
find /run/niri-rdp/desktops -mindepth 1 -maxdepth 1 -type d
```

Expected after teardown: no project logind session, desktop directory, compositor,
private D-Bus or test application. A still-connected SSH/TTY administrator is not
an orphan project session. Broker socket and listener remain while services run.
The `nvidia-smi` observation applies only when testing NVIDIA encoding; encoder
activity during changing desktop content is useful hardware evidence, while an
encoder listed by FFmpeg alone proves only that the software was built with it.
Inspect service/UID before terminating any session. Monitor-off is not suspend;
keep the test machine awake without modifying the working host's power policy.

Single-session frontend, US physical-key input, normal transform and scale 1 are
the current contract. The frontend follows client-selected initial dimensions
and supports live single-display resolution requests through Display Control.
Rebuild both packages; see the [resolution contract](../bridge/README.md#dynamic-resolution). No
persistent reconnect, multi-monitor, clipboard, audio or Unicode/IME claim is made.
The optional host encoder uses H.264 AVC420; it does not enable host-side NVDEC
or ensure that the RDP client uses hardware decoding. Compositor rendering prefers
hardware EGL devices with software fallback. Record the selected renderer and
client decoding mode separately; capture still uses SHM and CPU color conversion.
Completion requires every requested actual-machine gate to pass and its evidence
to be recorded; pending rows mean the boot-to-remote-login goal is incomplete.
