# Upgrade and patched Niri maintenance

Treat the bridge, broker and headless compositor as one tested release. Preserve
the old combined package (or both split package files), source bundles, checksums, configuration, checkpoint
and machine acceptance record before upgrading the **separate test machine**.

Current maintained base: Niri PR #3953 at
`0be60b742762905b7bee1ea67bb5a2fb00859bf0`, Smithay
`ff5fa7df392cecfba049ffed55cdaa4e98a8e7ef`, and the patch under
`compositor/patches/`. `compositor/pins.json` records source/lock hashes and the
comparison with PR #3800. IronRDP is pinned to
`bf019ca502d1e52616853a5ded68c885cec1e322` by `bridge/Cargo.toml` and its lockfile.
No floating PR branch, git `main` or implicit dependency update is a release.

For a new compositor base, download its exact revision in an isolated checkout,
review the upstream diff and reapply the local patch. Reevaluate startup backend,
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

When upstream Niri provides all required behavior, create a new tested headless
package from the upstream revision without the now-unnecessary patch. Keep the
private binary path and broker contract until the whole boot/capture/input/
cleanup matrix passes. Then a separately reviewed release can depend on stock
Niri and change the fixed worker path. No package performs this migration or
replaces `/usr/bin/niri` automatically.

## Rebuild the installed combined package

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
