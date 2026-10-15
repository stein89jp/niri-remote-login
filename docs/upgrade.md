# Upgrade and patched Niri maintenance

Treat the bridge, broker and headless compositor as one tested release. Preserve
the old combined package (or both split package files), source bundles, checksums, configuration, checkpoint
and machine acceptance record before upgrading the **separate test machine**.

Current maintained base: upstream Niri `main` at
`fc9e8c425e2267d877a65918f030c86f3c698179`, Smithay
`8a270232f1bfc416c781273c98a3f52a812a0bba`, with the 12-patch feature series in
`compositor/patches/` (0001 is the headless backend of PR #3953, rebased; see
the [compositor patch table](../compositor/README.md)). `compositor/pins.json`
records source/lock/patch hashes, the previous pin and the comparison with
PR #3800. The compositor and bridge must be upgraded together
(`niri-rdp-headless>=26.04.remote2-1`): the bridge's cursor relies on patch
0011. IronRDP is pinned to
`7b976a6965c96c76ff949313e174ea55dada8c44` by `bridge/Cargo.toml` and its lockfile.
No floating PR branch, git `main` or implicit dependency update is a release.
The [release history](#release-history) maps the "release N" names used in
older notes to package versions.

For a new compositor base, download its exact revision in an isolated checkout,
review the upstream diff and rebase the patch series one feature at a time,
dropping any patch whose behavior upstream now provides. Reevaluate startup backend,
render clock, output bounds/lifecycle, SHM capture, rejected buffers and privileged
virtual input. Regenerate the lockfile intentionally, record the new source and
patch hashes, then run the retained tests and disposable real-desktop integration.
Build the combined package (or both split packages) and inspect its contents. Repeat all separate-machine
acceptance checks, including Windows and monitor power-off, before promoting it.
Stock Niri's version changing does not justify automatically updating this fork.

On the test installation, log out of remote desktops, stop the frontend and broker,
follow Arch's full-upgrade practice for repository libraries, and install the new
combined package file with `pacman -U` (or both files for a split build). Review `.pacnew`/`.pacsave` changes;
never blindly replace the dedicated PAM policy. Reload systemd, start the
frontend and repeat rejection/cleanup checks before restoring boot startup.
Arch ABI changes require rebuilding local packages against the new libraries.
Avoid partial upgrades or workarounds involving library symlinks.

Rollback: stop both services, reinstall the retained combined package (or both split files), restore the
matching configuration, reload units and rerun the smoke checks. Rolling back
only a binary after an ABI-changing Arch upgrade is not guaranteed to work;
retain a disposable VM snapshot as the stronger recovery method.
To rebuild the old compositor pin instead, restore from git history
`compositor/patches/0001-remote-headless-bounds-and-startup.patch`,
`0002-remote-cursor-capture.patch`, `compositor/protocols/niri-rdp-cursor-v1.xml`,
the old `0be60b74` source tarball and the matching old bridge, and make the
`previous` block of `compositor/pins.json` the `selected` one. The old bridge
and old compositor also belong together.

When upstream Niri provides all required behavior, create a new tested headless
package from the upstream revision without the now-unnecessary patches. Keep the
private binary path and broker contract until the whole boot/capture/input/
cleanup matrix passes. Then a separately reviewed release can depend on stock
Niri and change the fixed worker path. No package performs this migration or
replaces `/usr/bin/niri` automatically.

## Rebuild the installed combined package

The compositor is now rebased onto upstream Niri `main` `fc9e8c42` with a
12-patch series (see [validation](validation.md#niri-rebase-onto-upstream-main-2026-10-15)).
The custom `niri-rdp-cursor-v1` protocol is gone: the bridge reads the cursor
through standard ext-image-copy-capture cursor sessions, and the compositor
carries a small no-echo patch (0011) and a drag-and-drop icon patch (0012).
A new bridge against an older compositor, or the reverse, loses the separate
cursor or makes it snap back while moving, so upgrade both together. The
combined package does this. A separate split build must install
`niri-rdp-headless` `26.04.remote2-1` together with `niri-remote-login`, which
now requires it. End any retained remote desktop and start a fresh one, so the
new compositor runs; reconnecting to a retained desktop keeps the old one.
Building in a reused `packaging/arch/niri-rdp` directory is safe: `prepare()` now
clears the copied source tree in `src/` (except the bridge's Cargo `target/`)
before copying, so files deleted from the checkout, such as the old compositor
patches or `bridge/src/wayland.rs`, no longer break the build. Your Niri
configuration, keyboard layouts and input-method setup need no changes.

The authentication, TLS and frontend-hardening changes after `1.5-1` need two
manual steps on an existing installation. `/etc/pam.d/niri-rdp` is a pacman
`backup` file: if you edited it, pacman installs the new policy as
`/etc/pam.d/niri-rdp.pacnew`. Merge that file, keeping its private faillock
directory, and do not keep an include of `system-remote-login`. If you use NVENC,
copy the packaged `nvidia-frontend.conf` over
`/etc/systemd/system/niri-rdp-frontend.service.d/nvidia.conf` again; older copies
have no `DeviceAllow` lines, and the hardened frontend then fails NVENC with a
device permission error. Existing generated certificates are adopted and renewed
automatically near expiry, which changes their fingerprint; see
[install](install.md#connect-with-windows-remote-desktop).

The frontend-to-broker protocol is now version 2 (`NRB2`), which carries the
client address for `PAM_RHOST` and the audit journal. The broker rejects
version 1 requests, so the frontend and broker must be upgraded together; the
combined package does this. With split packages, install both at once and
restart both services; a new frontend against an old broker (or the reverse)
rejects every login. Restarting the broker ends retained desktops, so start
fresh remote logins afterwards. `/etc/niri-rdp/broker.conf` is a new pacman
`backup` file holding `grace_seconds` (default 45, as before); see
[broker settings](install.md#broker-settings). The broker unit no longer uses
`PrivateTmp`, so new desktops share `/tmp` with the account's other logins and
use the PAM umask (022 by default); merge `pam_umask` from
`/etc/pam.d/niri-rdp.pacnew` if pacman created one. Optional fail2ban examples
are installed under `/usr/share/doc/niri-remote-login/fail2ban/` and are not
enabled; see [install](install.md#login-audit-log-and-fail2ban).

The clipboard now exchanges HTML and images as well as text, in both
directions. Files remain unsupported. The frontend provides this, so it applies
to connections made after the frontend restarts; no settings change is needed.

Microphone redirection (mstsc **Record from this computer**) adds a new session
helper, `/usr/lib/niri-rdp/session-microphone`, and a new package dependency,
`pipewire-audio`, which pacman installs with the package. The broker's `NRB2`
success reply may now carry a third descriptor for the microphone, which an
older frontend rejects, so the frontend and broker must be upgraded together;
the combined package does this, and split packages need both installed and both
services restarted. The feature is on by default (`NIRI_RDP_MICROPHONE=auto`);
if you edited `/etc/niri-rdp/frontend.conf`, pacman keeps your file and the new
commented setting appears only in `frontend.conf.pacnew`, which you need not
merge unless you want `off`. If you edited `/etc/niri-rdp/waybar.json`, merge the
new `privacy` microphone indicator from `waybar.json.pacnew`. End any retained
remote desktop and start a fresh one, so the session starts the new helper;
reconnecting to a retained desktop keeps playback but has no microphone. The
playback path and its accepted PCM configuration are unchanged. See
[remote microphone](install.md#remote-microphone).

Version `1.5-1` publishes the performance, picture-quality, clipboard and smooth
cursor improvements together. It retains the tested runtime source from
`1.1.0-2`; the user confirmed that the mstsc cursor fix works after reinstall.
Existing configurations and the accepted PCM audio controls remain applicable.
When upgrading from an older release, start a fresh remote desktop so the
frontend and headless compositor versions match.

Version `1.1.0-2` fixes jumping Windows 10 mstsc cursors by stopping the separate
cursor path from echoing ordinary mouse movement as client position updates.
It also prevents shape/fallback changes from replaying stale positions, while
retaining compositor-driven warps. Build the combined package while connected;
deploy from a local terminal or independent SSH session. Both the frontend and
headless compositor change, so end the old remote desktop before starting the
new version. Restarting only the frontend reconnects to the old retained
compositor and does not apply the full fix. A fresh session ends the old remote
applications; save their work before deployment. Check cursor motion, dragging,
shape changes and focus warps in mstsc after reconnecting.

Package release 20 corrects a playback-feedback blind spot: a completion for a
wave queued longer than one second was discarded even during continuous audio.
Fresh, unambiguous completions can now trigger bounded backlog recovery, with
guards against old playback runs and reused block numbers. PCM preference,
20 ms pacing, zero timestamps, capture bounds and transport credits are unchanged.
The normal [uninstall](uninstall.md) and NVIDIA [install](install.md) procedures
build the change; no new settings are needed. Verify speech clarity and growing
YouTube lip-sync delay after installation using the
[audio acceptance procedures](acceptance.md). Actual Windows playback with
release 20 remains pending; simulated queue recovery is not a listening result.

Package release 19 restores PCM preference over AAC and MS ADPCM after Windows
10 mstsc playback with release 18 produced continuous crackling. It preserves
release 18's immediate timestamps, capture limits and bounded playback recovery.
AAC remains a fallback for clients without PCM; waiting for transport credit
without dropping capture now preserves its encoder state. PCM needs about
1.54 Mbit/s before framing. Build the package while connected, then install and
restart only `niri-rdp-frontend.service` from a local terminal or independent SSH
session. The restart disconnects RDP; reconnect within the broker's 45-second
retention window and verify `RDP audio PCM` in the frontend journal. This change
does not require restarting the broker or ending the retained desktop. Check
speech and music clarity, pause/stop delay and long-running lip-sync using the
[audio acceptance procedures](acceptance.md).

Package release 18 fixes the release 17 audio recovery regression: missing
optional playback confirmations could permanently mute AAC and repeatedly lower
video quality. Playback feedback now expires, implausible timestamp delays are
ignored, and recovery resumes consecutive packets instead of one AAC priming
packet. The normal [uninstall](uninstall.md) and [install](install.md) procedures
build the fix; no additional settings are required. Connection failure logs also
include the underlying error so abandoned handshakes can be diagnosed.

Package release 15 fixes audio playback feedback that could be hidden by a
receipt confirmation and adds automatic MS ADPCM negotiation with PCM fallback.
It also fixes Fcitx5 disconnection on its native Wayland keymap, forwarded-key
handling, and startup waits that previously blocked Wayland. Existing Fcitx
profiles, environment settings and hotkeys are reused. Rebuild the whole package
and start a fresh remote desktop so both the frontend and compositor fixes apply.
The normal [uninstall](uninstall.md) and [install](install.md) procedures are sufficient;
no extra audio or input-method configuration is required.

Package release 14 prepares the NVIDIA GPU, control, and UVM device nodes
automatically before every frontend start. Release 13 did not explicitly create
`/dev/nvidiactl` or the UVM nodes, leaving CUDA startup dependent on another GPU
application or udev. The helper now requests the control-device minor with
`nvidia-modprobe -c 0 -c 255` and UVM nodes with `nvidia-modprobe -u -c 0`.
Existing NVIDIA encoder drop-ins continue to work. Install the rebuilt package,
reload systemd and restart the frontend; no separate helper enablement is needed.
If you applied the earlier manual workaround, its
`/etc/systemd/system/niri-rdp-nvidia-devices.service` and
`/etc/systemd/system/niri-rdp-frontend.service.d/nvidia-devices.conf` are now
redundant. Remove those two local files to use the package-owned helper, keeping
the `nvidia.conf` encoder override, then reload systemd.
After reboot, check both service journals before running `nvidia-smi` or another
GPU application; the frontend must start and report `NVENC H.264 preflight passed`
without that manual initialization. See the [cold-boot check](install.md#optional-nvidia-encoding).

Release 14 also waits for WirePlumber's default-device policy before selecting
the private remote audio sink. Previously a correct password could authenticate
successfully, then fail desktop startup with `pactl set-default-sink` reporting
`Not supported`. The login screen now distinguishes account/password rejection
from desktop startup failures and keeps the latter out of the failed-password
attempt limit. Restart both services after upgrading to load the new bridge and
worker; retained desktops must end before the updated audio startup runs.

Package release 12 fixes keyboard inheritance when the usual Niri `xkb` section
is empty. The headless build now receives the saved system XKB settings before
startup. The frontend also uses those settings with XKB translation in its login
form. Configured Fcitx/IBus startup uses private session state; Fcitx profile writes
cannot change the host profile, and IBus cannot reuse the host address file.
Reload systemd and restart the frontend after installing so the login form uses
the new environment launcher. End retained remote desktops and start a fresh
login to receive the new keyboard defaults and input-method startup.

Package release 10 adds private file open/save portals and updates the
private D-Bus activation environment after the remote display is available.
This fixes Firefox downloads whose save dialog previously failed in remote
sessions. Start a fresh remote login after upgrading; reconnecting to a retained
desktop does not run the new portal startup helper. The installed Niri chooser
is preserved, with GTK as a fallback; Firefox download preferences are unchanged.
See [configuration and troubleshooting](install.md#configuration-and-troubleshooting).

Package release 17 adds GNOME-compatible AAC-LC audio at 96 kbit/s with PCM
fallback, zero RDPSND timestamps for the RDPGFX desktop path, silence suppression,
fresh capture limits and elapsed-time receipt/render confirmation tracking.
Audio congestion now lowers the live video bitrate. Build and install the new
combined package through the normal manual procedure below. Restart the
frontend after installation and reconnect; verify `RDP audio AAC-LC` (or PCM
fallback) in its journal, then check speech, pause/stop delay and lip-sync for a
long-running video. See the [audio acceptance procedures](acceptance.md).

Package release 16 prefers PCM audio when both PCM and MS ADPCM are accepted.
This addresses reported static on Windows 10 mstsc with release 15's automatic
ADPCM preference. The existing pacing and playback-backlog controls still apply.
After installing release 16, restart only `niri-rdp-frontend.service` and reconnect
to negotiate PCM. This drops the RDP connection; the unchanged broker can retain
the desktop for reconnection within its 45-second grace window. The audio helper
is unchanged, so this fix does not require logging out or restarting the broker.
Confirm the new journal entry starts with `RDP audio PCM`, then check speech and
audio/video delay using the [acceptance procedure](acceptance.md).

The audio pacing and adaptive bitrate changes are included in package release 9.
Build with `makepkg` (without `-i`) while your remote session remains active.
Install and restart from a local terminal or an independent SSH session when
you are ready for the RDP connection to drop. The frontend needs restarting to
load the new bridge. A retained desktop keeps its running audio helper; log out
of that desktop and start a fresh login to load the new capture relay as well.
Stopping the broker logs out all retained desktops, so save application work
before following the full upgrade procedure below.

On the intended host, update the checkout and run `makepkg -fsi` again from
`packaging/arch/niri-rdp`. The local sources are copied under content-addressed
names so changed files cannot accidentally reuse stale cached inputs, including
uncommitted edits. The `-f` forces a rebuild even when a package archive with
the same version already exists; otherwise `makepkg -si` can install that
archive without building changed sources. Stop both
services before updating and review `.pacnew` files before restarting them.
Existing TLS files are not package-owned and are preserved by first-start setup.
When migrating from the old loopback configuration, pacman preserves your edited
`frontend.conf`; choose `NIRI_RDP_LISTEN` explicitly if you want direct access.

The remote desktop now loads your existing Niri configuration automatically,
with a private DankMaterialShell/Waybar startup helper. Existing optional
`~/.config/niri-rdp/config.kdl` files still take precedence; keep one for remote
customization, or move it aside to use your usual Niri settings on the next login.
Accounts without either user configuration get the system defaults, which start
a bar and bind Super+D to Fuzzel. Pacman keeps
an edited `/etc/niri-rdp/headless.kdl` and may place the new defaults in
`/etc/niri-rdp/headless.kdl.pacnew`. Merge the bar `spawn-at-startup` line and
updated `binds` into your existing remote configuration before reconnecting.
If you use `~/.config/niri-rdp/config.kdl`, that file takes precedence over the
system defaults and must be updated separately. Existing remote sessions need a
logout and fresh login to run new startup commands. A disconnect now retains
the existing desktop, so reconnecting does not rerun startup commands. The remote compositor
has its own Wayland runtime and D-Bus bus; shared systemd desktop services are
not started or given its environment.

Remote startup now inherits configured input-method and locale settings and runs
ordinary XDG autostart on the private display and bus. The configured input
framework also starts before user applications. The bridge inherits Niri's keyboard map instead of supplying a fixed
Japanese layout. Existing language profiles and startup configuration are reused.
Log out and start a fresh remote login to receive the new startup environment.

## Release history

Older notes, [acceptance](acceptance.md) and `CLAUDE.md` say "release N". That
is the `pkgrel` of version `0.1.0`: release 19 is `niri-remote-login 0.1.0-19`.
Versions from `1.0.0` restart `pkgrel` at 1. The table is reconstructed from Git
history: the date is that of the first commit carrying the version, not
necessarily the build or install date, and several builds may share one commit.
Releases 1–3 were split packages (`niri-remote-login` plus `niri-rdp-headless`);
the combined `packaging/arch/niri-rdp` package starts at release 4.
"Headless" is the `niri-rdp-headless` version built from the same commit.
Only `v1.5` is documented as a published GitHub release; whether earlier
versions were published is unknown.

| Package | Release | Date | Commit | Headless | Headline change |
| --- | --- | --- | --- | --- | --- |
| `0.1.0-1` | 1 | 2026-10-10 | `1a7b4b8` | `26.04.remote1-1` | Initial import: split packages, bridge, broker, patched Niri |
| `0.1.0-2` | 2 | 2026-10-10 | `9c93819` | `26.04.remote1-1` | Optional NVENC H.264 host encoding |
| `0.1.0-3` | 3 | 2026-10-10 | `1ee22b7` | `26.04.remote1-2` | Client-selected resolution and live resize |
| `0.1.0-4` | 4 | 2026-10-10 | `231646e` | `26.04.remote1-2` | Combined package, direct listening, first-start TLS, RDP login screen |
| `0.1.0-5` | 5 | 2026-10-10 | `9b97832` | `26.04.remote1-3` | NVIDIA-first EGL rendering, 60 fps NVENC budget |
| `0.1.0-6` | 6 | 2026-10-11 | `79ac410` | `26.04.remote1-4` | Login screen fixes; two later commits at this pkgrel add reconnect grace and phone/Unicode keyboard |
| `0.1.0-7` | 7 | 2026-10-11 | `d0a9ee2` | `26.04.remote1-4` | Remote audio playback |
| `0.1.0-8` | 8 | 2026-10-12 | `d5144eb` | `26.04.remote1-4` | Flatpak permission fixes |
| `0.1.0-9` | 9 | 2026-10-12 | `7240a83` | `26.04.remote1-4` | Audio pacing and adaptive NVENC bitrate |
| `0.1.0-10` | 10 | 2026-10-12 | `7169481` | `26.04.remote1-4` | Private file portals (Firefox downloads) |
| `0.1.0-11` | 11 | 2026-10-12 | `43b45d8` | `26.04.remote1-5` | Host keyboard layout and input-method inheritance |
| `0.1.0-12` | 12 | 2026-10-12 | `f73167a` | `26.04.remote1-5` | Keyboard inheritance with an empty Niri `xkb` section |
| `0.1.0-13` | 13 | 2026-10-12 | `7e2d191` | `26.04.remote1-5` | NVIDIA device-preparation service |
| `0.1.0-14` | 14 | 2026-10-12 | `dd602fd` | `26.04.remote1-5` | NVIDIA control/UVM nodes, WirePlumber wait, login error split |
| `0.1.0-15` | 15 | 2026-10-12 | `c0414ee` | `26.04.remote1-6` | Playback feedback fix, MS ADPCM, Fcitx5 fixes |
| `0.1.0-16` | 16 | 2026-10-12 | `ffb60d3` | `26.04.remote1-6` | PCM preferred over MS ADPCM (static) |
| `0.1.0-17` | 17 | 2026-10-12 | `a5f7df6` | `26.04.remote1-6` | AAC-LC, zero RDPSND timestamps, capture limits |
| `0.1.0-18` | 18 | 2026-10-12 | `49ca7f6` | `26.04.remote1-6` | Bounded audio recovery; AAC crackled in mstsc |
| `0.1.0-19` | 19 | 2026-10-12 | `b7e3995` | `26.04.remote1-6` | PCM preferred again: **accepted audio baseline** |
| `0.1.0-20` | 20 | 2026-10-13 | `a5c1b66` | `26.04.remote1-6` | Fresh-completion backlog recovery (Firefox delay) |
| `1.0.0-1` | — | 2026-10-13 | `31b47f2` | `26.04.remote1-6` | Version renumbering of the release 20 source; README |
| `1.0.1` | — | unknown | — | — | Unknown: no `1.0.1` appears in Git history |
| `1.0.2-1` | — | 2026-10-13 | `8c5471b` | `26.04.remote1-6` | Author attribution, licensing notes, packaged README |
| `1.1.0-1` | — | 2026-10-14 | `7abfd65` | `26.04.remote1-8` | Performance/desktop upgrades and text clipboard |
| `1.1.0-2` | — | 2026-10-15 | `a4e7b5e` | `26.04.remote1-8` | mstsc cursor-jumping fix, NVIDIA improvements |
| `1.5-1` | — | 2026-10-15 | `bae6626` | `26.04.remote1-9` | Published `v1.5`: 1.1.0-2 runtime as a release |
| `1.5.5-1` | — | 2026-10-15 | this release | `26.04.remote2-1` | Niri rebase, hardening, rich clipboard, microphone |

`niri-rdp-headless` `26.04.remote1-7` does not appear in any commit. The
PKGBUILDs carry `pkgver=1.5.5`/`pkgrel=1`. Bump the version again before
building or publishing later changes, so packages with different contents never
share a version.

The bridge crate (`bridge/Cargo.toml`) keeps version `0.1.0` on purpose. It is
not published (`publish = false`), nothing reads `CARGO_PKG_VERSION`, and the
package `pkgver`-`pkgrel` is the release identity. Keeping the crate version
fixed avoids changing `Cargo.lock` with every release.
