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
| RDP login screen | Connect without a ClientInfo password; see the login screen without a compositor/session, reject a bad password, accept the Linux account, verify masked pixels, input, and disconnect detachment. An idle form drops at 60 s and any form at 120 s total; repeated wrong passwords from one IP get growing (≤60 s) generic rejections; a correct login after the backoff works | Automated login, deadline and backoff tests; installed PAM and Windows checks done, user confirmed on the installed host 2026-10-17 |
| Authentication policy | Wrong password, unknown user, root, nonmember, locked/expired user, additional PAM prompt, account with local graphical session; confirm no app/session/private directory remains. A correct and a wrong root password both take about 2 s; 5 bad RDP attempts lock only niri-rdp, while sudo and the console still work | Namespace PAM stack validated; native failure tests cover contracts; installed-host check done, user confirmed on the installed host 2026-10-17 |
| Busy and local-session messages | While one mstsc connection is attached, log in to the same account from a second client; with the account's local graphical session open (or still closing), log in remotely. Expect **Desktop in use by another connection.** and **A local desktop session is still open.**; a wrong password, unknown user or nonmember still gets only the generic denial | Native broker and frontend tests passed 2026-10-15; installed-host mstsc check done, user confirmed on the installed host 2026-10-17 |
| Audit journal and fail2ban | Fail, succeed, disconnect, resume, let the grace expire and log out from a known client; `journalctl -t niri-rdp-broker` shows `auth-fail`, `login`, `detach`, `resume`, `grace-expire` and `logout` with the real client address and no password; `faillock --dir /run/faillock-niri-rdp --user NAME` lists the same source address from `PAM_RHOST`. Optionally copy the fail2ban example, verify `fail2ban-client status niri-rdp` bans after repeated failures, and that `logger` entries from a local user never match | Broker tests and lab NRB2 integrations passed 2026-10-15; installed journal and fail2ban checks done, user confirmed on the installed host 2026-10-17 |
| Connection summary | Connect from mstsc, play audio, copy text and an image each way, then disconnect normally; reconnect and end once by killing mstsc. `journalctl -u niri-rdp-frontend \| grep "connection summary"` shows exactly one line per connection with plausible `result`, codec/mode (`audio_codec=pcm`) and counters, and no username, error text or clipboard content | Frontend tests passed 2026-10-15, including one summary per abruptly dropped TCP connection; installed-host mstsc check done, user confirmed on the installed host 2026-10-17 |
| Session environment | In a remote terminal, `umask` prints `0022` (or the `/etc/login.defs` value), `ulimit -Sc` is 0 and `ulimit -Hc` unlimited (or the `pam_limits` value), and a file written to `/tmp` is visible from an SSH login of the same account | Native broker tests passed 2026-10-15; installed-host check done, user confirmed on the installed host 2026-10-17 |
| Cold boot | Enable system frontend, reboot test machine, do not log in locally, connect directly using Windows Remote Desktop; record `loginctl` identity and terminal `id` output | Pending |
| NVIDIA cold boot | Install release 14 or newer and the NVIDIA encoder override; reboot without a local graphical login or running `nvidia-smi`/FFmpeg first; require successful device preparation followed by `NVENC H.264 preflight passed` and an active frontend; repeat after another reboot | Explicit GPU/control/UVM requests and startup contracts checked; installed reboot pending |
| Windows | mstsc displays dedicated Niri terminal and sends keyboard, pointer, wheel and modifiers; test ordinary TLS negotiation, missing-password login screen, optional TLS-only profile, and certificate verification | User confirmed Windows 10 mstsc desktop and smooth cursor movement after reinstall; detailed negotiation/login-screen/profile/certificate checks remain pending |
| Dynamic resolution | Connect at a non-default size; resize the client window in portrait and landscape, verify application reflow, sharp text and accurate pointer clicks without reconnecting | Namespace FreeRDP validation recorded in validation.md; Windows and installed-service behavior pending |
| NVIDIA encoding | Rebuild the current frontend, enable its NVIDIA drop-in, verify NVENC initialization and AVC420 graphics negotiation; decode changing frames, exercise input/reconnect, and record GPU encoder activity, bitrate and latency against bitmap mode | NVENC/FreeRDP namespace app/input/reconnect and comparative capture/transport/decode workloads passed; Windows presentation and systemd permissions pending |
| Audio clarity | Windows 10 mstsc with remote playback enabled: install release 19 or newer, restart the frontend and reconnect; require `RDP audio PCM` in its journal (since the 2026-10-20 rate change `RDP audio PCM: 44100 Hz stereo, 1411 kbps, 20.00 ms blocks; channels=stereo rate=44100 drift_ppm=100 drift_skip=on`, preceded by an `RDP audio formats: client accepted [...]` line listing what mstsc accepted; mono is opt-in and sounded pitched-up and robotic). Play speech and music, check intelligibility and absence of static and clicks, and repeat after reconnect | User confirmed release 19 stereo PCM fixed playback on 2026-10-12 after continuous AAC crackling with release 18. The 2026-10-18 mono PCM preference was rejected on 2026-10-20 (chipmunk/robotic in mstsc; stereo clean, [validation](validation.md#crackling-with-the-blind-drift-skip-and-the-earned-skip-2026-10-20)); the drift skip ([validation](validation.md#growing-audio-delay-mono-pcm-and-drift-skip-2026-10-18)) still needs long-session listening. On 2026-10-20 the user reported crackling, robotic sound with the blind 30 s skip; its replacement, the [earned skip](validation.md#crackling-with-the-blind-drift-skip-and-the-earned-skip-2026-10-20), needs this check too, as does the user-approved 44.1 kHz resampled PCM ([validation](validation.md#crackling-with-the-blind-drift-skip-and-the-earned-skip-2026-10-20)). Repeated-reconnect listening not recorded |
| Audio/video delay | Windows 10 mstsc with remote playback enabled: play a Firefox video with visible claps or simultaneous flash/tone for at least 30 minutes; pause and resume periodically and measure how soon audio stops. Repeat under constrained bandwidth and after a short network stall. Record sound offset, whether delay grows, recovery time, audible skips, negotiated codec and frontend bitrate/audio-delay entries; require no growing seconds/minutes of trailing audio. Since 2026-10-18, also play continuous music for 30+ minutes over the internet: the delay must stay near its starting value, the 20 ms drift skips must not be audible as clicks, and since 2026-10-20 the connection summary should show few `audio_drift_skips`: about one per 200 s of continuous sound after the first 800 s, plus at most one per 30 s of starvation credit during network trouble (`audio_model_starvations` counts those events) | User confirmed minimal latency with release 19 on 2026-10-12; on 2026-10-13 reported growing 1–2 second Firefox YouTube lag while local VLC clips remained synced. Release 20 corrects a reproduced delayed-feedback blind spot; actual Windows clarity, queue recovery, 30-minute playback, constrained-bandwidth and stall recovery listening remain pending. On 2026-10-18 the user reported delay growing from near zero during internet sessions; the drift-skip change for it awaits this check (long-session stereo lip-sync over the internet is still to be watched). On 2026-10-20 the user reported perfect sync with the blind skip; the earned skip that replaced it must keep that sync. Also on 2026-10-20, 48 kHz stereo PCM fell about 25 s behind video after 5 minutes of continuous YouTube; the 44.1 kHz PCM preference for it needs this check with at least 5–30 minutes of continuous sound (the audio offset must stay constant) |
| Audio recovery | Windows 10 mstsc with remote playback enabled: kill the session's private PipeWire 3+ times over 15 minutes, waiting over 10 minutes between the 2nd and 3rd kill; audio recovers each time after reconnect, and rapid repeated kills still stop after 2 helper restarts / 3 graph attempts | Restart-budget and virtual-clock tests passed 2026-10-15; installed-host mstsc check done, user confirmed on the installed host 2026-10-17 |
| Microphone | Windows 10 mstsc with **Record from this computer**: run the [microphone acceptance](#microphone-acceptance) steps below, including 10 minutes of playback and microphone together with unchanged playback clarity and latency | Namespace FreeRDP microphone integration passed 2026-10-15 ([validation](validation.md#microphone-redirection-ms-rdpeai-2026-10-15)); installed-host mstsc check done, user confirmed on the installed host 2026-10-17 |
| Codec fallback and hardware failure | With NVENC enabled, a client without AVC420 graphics support receives bitmap updates; unavailable/denied NVIDIA hardware produces a clear frontend startup failure; restore bitmap mode and verify login | Release namespace bitmap fallback passed with NVENC configured; actual service/device-denial checks pending |
| Physical monitor off | Keep test machine awake, power monitor off while moving typing in app remotely; record continued changing pixels/input, independent output and session identity | Pending |
| Monitor disconnected | Disconnect physical display, retain remote session and repeat interaction; separately test boot without attached monitor | Pending |
| Logout | Super+Shift+Q: RDP disconnects, PAM/logind session closes, all app/compositor/private D-Bus descendants exit, desktop directory removed. Applications receive SIGTERM and get 2 seconds to save state (for example an editor's unsaved-file recovery) before SIGKILL | Native SIGTERM-grace test passed 2026-10-15; real session checks done, user confirmed on the installed host 2026-10-17 |
| Compositor crash | SIGKILL test compositor PID; RDP closes promptly, broker cleans session/app/bus/directory; new login succeeds | Disposable RDP close tested; real broker cleanup pending |
| Persistent reconnect | Drop the network or close mstsc with an open app; reconnect within `grace_seconds` (default 45) of broker-observed detachment to the same account, verify identical logind session ID/compositor PID and application state; repeat with frontend restart and held keys/buttons. Set another `grace_seconds` in `/etc/niri-rdp/broker.conf`, restart the broker and check the journal's `detach grace period`; a group-writable or misspelled file logs a warning and keeps 45 | Native broker, configuration and real-app input checks; installed PAM/logind and Windows resume pending |
| Resume authorization | Reject wrong password, expired/locked account and removed `niri-rdp-users` membership on resume; preserve the detached desktop without creating another session | Pending real PAM tests |
| Frontend/broker crash | Frontend stop/SIGKILL retains desktop for reconnect within the 45-second grace window; broker stop/SIGKILL ends desktops. Test `ExecStopPost` recovery and orphan scope teardown, restart and log in again | Pending |
| Input disconnect | Disconnect while key and button held; verify releases and no state carried into next connection | Automated real-app test passed |
| Fcitx5 input | Start a fresh remote desktop with the existing Fcitx profile; toggle with the configured hotkey, type and convert Japanese in native Wayland, GTK/Qt and XWayland apps; reconnect and verify the daemon survives and the saved host profile is unchanged | Automated native Wayland Fcitx5/Mozc profile, Ctrl+Space and Japanese commit passed; installed toolkit/Windows checks pending |
| GNOME preservation | Compare GRD/GDM enablement/listeners and local session before/after test-machine experiment; do not alter working host | Packages have no GNOME configuration or stock Niri replacement |

## Niri rebase acceptance

The compositor was rebased onto upstream Niri `main` `fc9e8c42` with a 12-patch
series, and the cursor now comes from standard ext-image-copy-capture cursor
sessions (see [validation](validation.md#niri-rebase-onto-upstream-main-2026-10-15)).
After the clean reinstall, start a fresh remote desktop and run these in Windows
10 mstsc. Also repeat the Windows, Dynamic resolution, NVIDIA encoding, Audio
clarity, Logout, Persistent reconnect and Fcitx5 input rows above, and the
clipboard and cursor rows below, against the rebased compositor.

| Rebase gate | Executed evidence | Remaining actual-machine procedure | Current status |
| --- | --- | --- | --- |
| Keyboard layout and bindings | Namespace jp106/Unicode/Mod+D integration 11/11 and compositor `tests::remote_input` passed 2026-10-15 | Type in the configured layout; Mod+D opens the launcher; configured Mod+wheel binds act once per notch; press the layout-switch binding, then type and check the new layout | Done, user confirmed 2026-10-17 |
| Layout reload while connected | Compositor test `no_keymap_device_follows_a_layout_reload` passed 2026-10-15 | While connected, change the `xkb` layout in the Niri configuration and save; type without reconnecting and check the new layout, then reconnect and check again | Done, user confirmed 2026-10-17 |
| Emoji and phone Unicode, then reconnect | Namespace phone Unicode and Fcitx committed Unicode passed 2026-10-15 | Type emoji and other Unicode from the Windows touch keyboard or emoji panel with and without Fcitx active; disconnect, reconnect and type ordinary text and emoji again; keys stay in the right layout | Done, user confirmed 2026-10-17 |
| Cursor shape, hotspot and smoothness | Cursor capture policy 9/9 at 100/150/200% DPI with zero motion echoes, 2026-10-15 | At 100% and 200% Windows scaling: move smoothly without snap-back, click small targets with arrow, I-beam and hand shapes, hide the cursor while typing, and check animated cursors | Done, user confirmed 2026-10-17 |
| Drag-and-drop icon | Compositor unit tests only (patch 0012) | Drag a file in a file manager and text or a tab in a browser; the icon follows the pointer and disappears on drop or cancel; a large icon falls back without a wrong or blank cursor | Done, user confirmed 2026-10-17 |
| Software renderer | Lab: `software` gives llvmpipe with Mesa's EGL vendor and NVIDIA without the restriction | Only if used: set `software` in `/etc/niri-rdp/renderer.conf`, start a fresh desktop; the broker journal shows the surfaceless fallback and session apps render (`glxinfo -B` or `eglinfo` reports llvmpipe) | Done, user confirmed 2026-10-17 |

## Microphone acceptance

Microphone redirection (MS-RDPEAI) is separate from playback; see
[remote microphone](install.md#remote-microphone). The accepted PCM playback
baseline must stay exactly as it was: **if playback clarity or latency changes
in any step, stop and roll back** (set `NIRI_RDP_MICROPHONE=off` or reinstall
the previous package) and record the result. After the clean reinstall, start a
fresh remote desktop and run these in Windows 10 mstsc. The namespace results
in [validation](validation.md#microphone-redirection-ms-rdpeai-2026-10-15) do
not check off any of them; the user confirmed all of them on 2026-10-17.

| Microphone gate | Procedure and evidence | Current status |
| --- | --- | --- |
| Voice clarity | Enable **Local Resources → Remote audio → Settings → Record from this computer**, connect, and run `pw-record /tmp/t.wav` (or a browser microphone test) while speaking; play it back. The voice is clear, with no crackle and a low delay. `pactl get-default-source` prints `niri-rdp-microphone`; the summary shows `mic=pcm_48000_mono` | Done, user confirmed 2026-10-17 |
| Playback unchanged | Run playback and the microphone together for 10 minutes (YouTube plus a microphone test). Playback clarity and latency are the same as before; the summary still shows `audio_codec=pcm` | Done, user confirmed 2026-10-17 |
| Recording indicator | The Windows microphone indicator appears when `pw-record` starts and goes off about 5 seconds after it stops. With the packaged Waybar, its microphone icon shows while recording | Done, user confirmed 2026-10-17 |
| Repeated start/stop | Start and stop recording 5 times, each for a short phrase; the delay must not grow, and no stale audio from an earlier take appears at the start of a later one | Done, user confirmed 2026-10-17 |
| Reconnect | Disconnect and reconnect within the grace window; the microphone works again | Done, user confirmed 2026-10-17 |
| Recording disabled | Connect with **Record from this computer** off: no errors in the frontend journal, `mic=none mic_errors=0` in the summary, and playback is fine | Done, user confirmed 2026-10-17 |
| Setting off | Set `NIRI_RDP_MICROPHONE=off`, restart the frontend from a local terminal or SSH, connect: no `RDP microphone` journal lines and no `AUDIO_INPUT` channel; `pw-record` gets silence, the Windows microphone indicator stays off, and the summary shows `mic=none`. Restore `auto` afterwards | Done, user confirmed 2026-10-17 |

## Performance upgrade acceptance after the clean reinstall

The user will perform actual Windows-to-this-host checks after reinstalling the
completed source/package upgrade. Automated results below are scoped to private
fixtures; none checks off an installed Windows listening/display gate.

| Upgrade gate | Executed evidence | Remaining actual-machine procedure |
| --- | --- | --- |
| Input during capture, queue overload and resize | Wake/ordering regressions; one-FPS real-app key-up 2.139 ms; real odd/portrait resize and held-release integration | Repeat typing/drag/wheel/modifiers during resize and stalls in mstsc |
| Idle/damage and bitmap regions | Sparse/dropped-frame regression; quiet desktop and real refresh/suppression/resume passed | Leave a quiet desktop for minutes, refresh, minimize/resume, then edit sparse regions; verify every changed pixel |
| Cursor and DPI | Default cursor shape/physical corners, scaled hotspots and 100/150/200% logical click mapping passed; user confirmed smooth Windows 10 mstsc movement after the cursor fix | mstsc shape/hide/animation/grab/warp; Windows scale settings, explicit override, resize and reconnect |
| Cursor shape cache | Pointer cache unit tests passed 2026-10-15: repeated shapes become cached references within min(negotiated size, 32), each activation starts empty, size 0 sends full shapes, and a shape skipped before transmission is never referenced | Switch quickly between arrow, I-beam and hand; animated cursors; 200% DPI; resize mstsc and reconnect; shapes are never wrong or blank |
| Capture damage and SHM pool | Damage-hint comparison, capture-log and SHM pool reuse regressions passed 2026-10-15; lab llvmpipe/SHM workloads at 1080p and 4K recorded no damage misses | With `NIRI_RDP_DIAGNOSTICS=1`, type, scroll and drag windows at 1080p and 4K in bitmap and NVENC modes; resize and minimize/restore; no stale regions, `capture_damage_missed` stays 0, audio unchanged |
| Clipboard text, HTML and images | Real FreeRDP ↔ private Wayland, Japanese/emoji/multiline/empty, oversize recovery, rapid ownership, reconnect and two-desktop isolation passed; 2026-10-15 lab cycle passed PNG → DIB, HTML → `HTML Format` and the reverse | mstsc ↔ native Wayland, GTK/Qt and XWayland; verify local-resource client setting. Print Screen in Windows → paste in a Linux image app; copy an image in a Windows browser → paste in Linux; `grim - \| wl-copy` → paste in Paint and Word; rich text both ways (Word ↔ Firefox/LibreOffice); copying files in Explorer leaves the Linux clipboard unchanged |
| Stale graphics ACK and delivery policy | RTT/queue/reset/audio regressions, corrected cadence jitter, and 16 final-package shaped comparisons passed; opt-in policy has mixed throughput | Observe convergence under the actual client and link; compare frame age and smoothness before promoting optional policy |
| Quality profiles, refinement, AVC444 and GPU capture | Native profiles, guarded color/padding/reference tests, 24 final-package comparisons and strict GPU/fallback integrations passed | Compare colored small text, one-pixel edges, settled scrolls and video at 1080p/4K, with fallback and service-account GPU access |
| Audio capture age and helper recovery | Source/relay/socket age contract, finite readiness worker, private graph/monitor/helper kills, usable desktop without audio passed | Keep PCM/timestamps/controls; at least 30 minutes Firefox/YouTube and VLC; pause/resume, constrained bandwidth, stalls, reconnect and helper failure |
| Silent loss and desktop resume | Exact Rust TCP blackhole detection 22.812 s quiet / 20.414 s busy; reconnect succeeded; healthy idle and two-second stall survived | Blackhole mstsc traffic; reconnect after detachment with fresh PAM auth; verify same compositor PID and application state before 45-second grace expires |

See [performance controls and procedures](performance.md). NVENC now defaults to
GPU capture with SHM fallback. New quality, codec and coordinated graphics
policies remain explicit options until their
comparative and Windows evidence justifies changing defaults. The accepted PCM
combination is unchanged. Cold boot, monitor off/disconnected, real PAM/logind,
service-account device access and final session cleanup retain their pending
status in the main matrix above.

Useful read-only observations on the test installation:

```sh
loginctl list-sessions
journalctl -t niri-rdp-broker
journalctl -u niri-rdp-frontend | grep "connection summary"
loginctl show-session SESSION_ID -p User -p Name -p Leader -p Service -p Type -p Class -p Remote -p Seat
systemctl status niri-rdp-broker.service niri-rdp-frontend.service
journalctl -b -u niri-rdp-nvidia-devices.service -u niri-rdp-broker.service -u niri-rdp-frontend.service --no-pager
nvidia-smi dmon -s u
ss -ltn 'sport = :33991'
pgrep -a -u niri-remote-test
find /run/niri-rdp/desktops -mindepth 1 -maxdepth 1 -type d
```

Disconnect retains the desktop for `grace_seconds` (default 45) after
broker-observed detachment.
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

Single-session frontend, inherited physical-key input, normal transform and deliberate
client/explicit DPI scaling are the current contract. The frontend follows client-selected initial dimensions
and supports live single-display resolution requests through Display Control.
Rebuild the combined package (or both split packages); see the [resolution contract](../bridge/README.md#dynamic-resolution). No multi-monitor or client-side RDP IME composition claim is made. Clipboard is
negotiated Unicode text, HTML and images through private-session Wayland data
control; files are not supported. Host
Fcitx5/Mozc native Wayland composition is covered by the disposable integration test. Session audio
has private PipeWire capture and RDP packet tests; installed-service playback
through Windows mstsc, including reconnect, still requires actual-machine validation.
Broker mode retains
desktops for authenticated reconnect; installed PAM/logind and Windows validation
of that behavior remains pending.
The optional host encoder defaults to H.264 AVC420; AVC444 requires explicit
selection and client capability negotiation. It does not enable host-side NVDEC
or ensure that the RDP client uses hardware decoding. Compositor rendering prefers
hardware EGL devices with software fallback. Record the selected renderer and
client decoding mode separately. NVENC capture defaults to GPU (`auto`), which
requires compatible DMA-BUF/EGL/CUDA/NVENC support and falls back to SHM and CPU
color conversion when unavailable; bitmap mode always uses SHM.
Completion requires every requested actual-machine gate to pass and its evidence
to be recorded; pending rows mean the boot-to-remote-login goal is incomplete.
