# Install the Arch remote desktop host

Build on the **Arch Linux x86_64 machine that will host the remote desktop**.
Use an up-to-date Arch installation and a normal, unprivileged build account.
The repository is private, so cloning requires repository access. Rust 1.94 or
newer is required; `makepkg -fsi` installs the declared build/runtime dependencies.
The initial Rust/compositor build can take a while.

## Install, authorize, enable

```sh
sudo pacman -Syu --needed base-devel git
git clone https://github.com/stein89jp/niri-remote-login.git
cd niri-remote-login/packaging/arch/niri-rdp
makepkg -fsi
sudo usermod -aG niri-rdp-users "$USER"
sudo systemctl enable --now niri-rdp-frontend.service
```

For a fresh installation with an **NVENC-capable NVIDIA GPU and a working
NVIDIA driver**, use this block instead of the one above:

```sh
sudo pacman -Syu --needed base-devel git
git clone https://github.com/stein89jp/niri-remote-login.git
cd niri-remote-login/packaging/arch/niri-rdp
makepkg -fsi
sudo usermod -aG niri-rdp-users "$USER"

sudo install -d -m 755 /etc/systemd/system/niri-rdp-frontend.service.d
sudo install -m 644 /usr/share/doc/niri-remote-login/nvidia-frontend.conf \
  /etc/systemd/system/niri-rdp-frontend.service.d/nvidia.conf
sudo systemctl daemon-reload

sudo systemctl enable --now niri-rdp-frontend.service
```

When reinstalling from an existing checkout, skip `git clone` and use that
checkout's `packaging/arch/niri-rdp` directory. `makepkg -fsi` rebuilds from its
current files, including local edits. The `-f` matters when a package archive
already exists: without it, `makepkg -si` can reinstall that older archive
without compiling the changes. Then run the account authorization, optional
NVIDIA drop-in, and service commands from the appropriate block above.

The NVIDIA drop-in enables hardware video encoding with a default 60 fps capture
budget and gives the frontend service access to the NVIDIA and DRM devices.
After upgrading or reinstalling, run its `install` command again even if
`nvidia.conf` already exists: older copies lack the device allow-list. Your
remote Linux account also needs GPU device access for hardware compositor
rendering; see
[Optional NVIDIA encoding](#optional-nvidia-encoding) for permissions and checks.

Package release 14 automatically prepares NVIDIA device nodes before every
frontend start, including after a reboot. No manual helper installation,
udev rule, or `nvidia-smi` invocation is needed. The preparation helper
skips hosts without a loaded NVIDIA GPU and does not need to be enabled separately.

If the frontend was already running when you added the NVIDIA drop-in, restart
it to apply the change; `enable --now` does not restart an active service:

```sh
sudo systemctl restart niri-rdp-frontend.service
```

This single `niri-remote-login` package contains the patched headless compositor,
RDP frontend, PAM/logind broker, defaults, and systemd units. Arch's package hooks
create the `niri-rdp` service account and `niri-rdp-users` authorization group.
On first service start the root broker creates a self-signed TLS certificate
and private key. Existing certificates are preserved and validated. No separate
release-preparation command, build VM, certificate-generation commands, or SSH
tunnel is required. Services start only when you enable/start them.

Remote-session playback uses a private PipeWire output and travels inside the
encrypted RDP connection. The package includes the required PipeWire,
PipeWire-Pulse, WirePlumber and Pulse client utilities. In Windows mstsc, select
**Local Resources → Remote audio → Settings → Play on this computer**; saved
`.rdp` profiles use `audiomode:i:0`. The remote output is **Remote Desktop Audio**.
After upgrading, end any retained remote desktop and reconnect so the new session
starts its private audio services. See the [audio contract](../bridge/README.md#session-audio).
The Windows microphone can also be used in the remote desktop; it is on by
default and needs only a client setting, see [remote microphone](#remote-microphone).

The compositor installs at `/usr/lib/niri-rdp/niri-headless`, alongside stock
`/usr/bin/niri`. It launches a separate remote desktop. It does not share your
existing local desktop. It reuses the account's usual Niri configuration,
configured input-method environment, and XDG autostart entries on a private
display and bus. An optional `~/.config/niri-rdp/config.kdl` overrides the usual
Niri configuration.

**An account cannot have an existing local X11/Wayland session when it logs in
remotely.** If you authorize your current user, log out of that user's local
graphical session before connecting. SSH/TTY logins are allowed. Root, service
accounts, and accounts outside `niri-rdp-users` are rejected. Rejoining the group
does not require a new local login for the broker to see the membership.

For a separate account, replace the `usermod` command above with:

```sh
sudo niri-rdp-setup --create-user niri-remote
```

The helper creates the home, prompts for a Linux password, and adds the account
to the group. To configure an existing user and optionally use a DNS name/IP
for a certificate that has not yet been created:

```sh
sudo niri-rdp-setup --user "$USER" --hostname YOUR_HOST_NAME_OR_IP
```

## Flatpak applications and updates

Flatpak's Bubblewrap helper needs read and traversal access to every parent of
the private runtime directory. The broker therefore uses mode 0755 for
`/run/niri-rdp` and `/run/niri-rdp/desktops`; individual desktop directories and
session records remain private at mode 0700.

Older packages used mode 0711 for these parents, causing application launches
to fail with `bwrap: Can't mkdir parents for .../.dbus-proxy/: Permission denied`.
After upgrading, the following also fixes an already-running remote desktop
without restarting the broker or disconnecting:

```sh
sudo chmod 0755 /run/niri-rdp /run/niri-rdp/desktops
```

System-wide Flatpak changes can require administrator authentication in a
remote login. A graphical updater needs a working Polkit authentication agent
in the remote desktop. To update system-wide applications from a terminal with
administrator authentication:

```sh
sudo flatpak update --system
```

Applications installed per user instead use `flatpak update --user` without
sudo. A private desktop portal supplies your configured file open/save dialogs,
including Firefox downloads, on the remote display. Keyring integration may require
additional setup.

## Connect with Windows Remote Desktop

Open the regular Windows Remote Desktop client (`mstsc.exe`). Enter the host's
reachable DNS name or IP followed by the port, for example:

```text
192.168.1.50:33991
```

Use the Linux account name without a Windows domain prefix. The server negotiates
TLS RDP. If the client sends Linux username/password credentials, the broker
checks them directly. If it omits the password or sends remembered Windows domain credentials, a
login screen inside the remote connection accepts the Linux account name and
password before starting Niri. Windows domain credentials are never sent to PAM.
NLA/CredSSP authentication is not provided; Windows policies that require NLA
remain incompatible. Actual `mstsc` interoperability is still an acceptance gate,
not a demonstrated result. Protocol fallback and a login screen do not replace a
Windows machine test.

The default listens on `0.0.0.0:33991`, so clients connect directly over the
network. If the host has a firewall, allow TCP 33991 from the intended client
network using your existing firewall configuration. Port 3389 is left available
for another remote desktop service. Nothing opens the firewall automatically.

The generated self-signed certificate lasts 825 days. When it has fewer than
30 days left (or has expired), the broker replaces it automatically at its next
start and logs the new SHA-256 fingerprint
(`journalctl -u niri-rdp-broker.service | grep -i fingerprint`). The fingerprint
therefore changes on renewal, and mstsc asks you to trust the new certificate.
Renewal happens only when the broker starts; restart the frontend as well so it
loads the new certificate. Compare the current SHA-256 fingerprint with the
client certificate details before accepting it:

```sh
sudo openssl x509 -in /etc/niri-rdp/tls/cert.pem -noout -fingerprint -sha256
```

You can provide a trusted certificate and unencrypted private key at those same
paths before starting the service. The TLS directory must be root-owned mode
700, the key root-owned mode 600, and the certificate root-owned without group
or world write access. Invalid, expired, mismatched, or incomplete identities
fail startup rather than silently replacing your certificate. Certificates you
provide are never renewed or replaced: the broker logs a warning when fewer than
30 days remain and fails startup once they expire. Replacing an identity requires
restarting both services so the frontend loads new credentials.

The remote desktop loads your existing `~/.config/niri/config.kdl`, preserving
your bindings, appearance, includes and startup applications. DankMaterialShell
(when installed), otherwise Waybar, starts on the private session if configured
startup commands have not already launched a bar. Accounts without a Niri
configuration receive `foot` and a bar: Super+Enter opens a terminal, Super+D
opens Fuzzel, Super+Q closes a window, and Super+Shift+Q logs out.
Disconnecting or losing the network preserves the desktop, open windows and
applications for 45 seconds after the broker observes the connection detach
(`grace_seconds` in the [broker settings](#broker-settings)). Reconnect with
the same Linux account and authenticate again to resume it; held keys and mouse
buttons are released when the old connection closes. When that grace window
expires, the broker fully logs out the desktop and cleans up its applications
and session; a subsequent login starts a fresh desktop after cleanup.
Super+Shift+Q logs out and ends the desktop; compositor exit or stopping
the broker also ends it. On logout the desktop's applications receive SIGTERM
and have 2 seconds to save state before they are killed. A frontend restart preserves the desktop only if you reconnect
within the same 45-second grace window. After a silent network
failure, the frontend may temporarily reject a reconnect while it still considers
the old connection active. Keepalive/write deadlines bound detection; private
blackhole checks measured about 20–23 seconds, after which a retry can authenticate.
Each resume rechecks PAM authentication, account policy and group authorization;
a revoked or expired account cannot resume. While another connection is attached
to the account's desktop, a new login is refused rather than taking it over.
The compositor
tries headless GPU rendering automatically, preferring NVIDIA hardware, and falls
back to surfaceless rendering when hardware is unavailable. Optional NVIDIA
encoding is described below. This remains
experimental: real PAM/logind, installed-service boot, Windows, and physical
monitor tests are tracked in [acceptance](acceptance.md).

## Configuration and troubleshooting

Configuration priority is `~/.config/niri-rdp/config.kdl`, then your usual
`~/.config/niri/config.kdl`, then `/etc/niri-rdp/headless.kdl`. `XDG_CONFIG_HOME`
is honored when set. Each selected configuration is included by a temporary
wrapper that starts the private desktop portal and ordinary XDG autostart after
Niri creates its display.
For your usual configuration, the wrapper starts `HEADLESS-1` at scale 1 with
normal orientation and supplies a private bar startup helper. The frontend can
then apply client Display Control DPI scaling or an explicit `NIRI_RDP_SCALE`
override without rewriting your Niri configuration. Physical output
settings do not apply to this virtual output.
Your original files are preserved. The helper waits up to two seconds for
configured DMS/Waybar/Quickshell startup before supplying a bar; unusually slow or
custom bars should use an explicit remote configuration.

To opt into separate remote settings, create `~/.config/niri-rdp/config.kdl`.
This override keeps its own output and bar settings, so include your desired `spawn-at-startup`
commands for the bar. The remote session uses its own Wayland socket and D-Bus
bus. Local systemd desktop services are not started or given the remote environment.

The portal startup helper exports the remote runtime, display, and audio settings
to the private bus only. Portal service activation uses `XDG_CURRENT_DESKTOP=niri:niri-rdp`
so your installed Niri portal configuration and preferred chooser take precedence.
For example, the GNOME backend supplies Nautilus file dialogs. The packaged
`niri-rdp-portals.conf` supplies GTK when there is no Niri portal configuration.
User portal settings under `~/.config/xdg-desktop-portal/` also take precedence.
The helper replaces GNOME/GTK backends already activated before the remote display
was available, without starting unused backends. It does not change application
configuration directories or Firefox's download preferences.
Upgrading fixes newly created remote sessions; reconnecting to
a preserved session retains its old startup environment until that session logs out.

### Input methods and keyboard layouts

The worker reads the host's saved XKB layout, model, variant, and options from
localed's `/etc/vconsole.conf` and `/etc/X11/xorg.conf.d/00-keyboard.conf`.
User environment and `environment.d` settings take precedence. The selected
Niri configuration can override them as usual. This also works with an empty
`xkb` section in the headless build, including Japanese `jp` / `jp106` settings.
The login screen uses XKB with the host defaults for physical keyboard events;
Unicode input supplied by the RDP client is preserved directly.

The remote desktop runs the same XDG autostart entries under `~/.config/autostart/`
and the system XDG configuration directories, honoring user overrides and disabled
entries. When the configured environment selects Fcitx or IBus, that framework
starts on the private display before startup applications launch. Installing a
framework alone does not select it. Disabled autostart overrides are respected,
and duplicate framework entries are skipped. Fcitx receives a private copy of
its configured profile so remote layout changes cannot rewrite the host profile.
IBus receives a private address and discovery file. D-Bus activation receives
the remote display before startup applications launch.
Fcitx5 startup waits for its configured groups to be available while Wayland
continues serving clients. Its ordinary typing, input-method switching, and
native Wayland composition use the existing profile and hotkeys automatically.
The private bus and display remain session-specific; shared user systemd services
are not restarted or given the remote display.

Changes to the remote Fcitx profile last for that remote session. Make persistent
profile changes in the host configuration before starting a fresh remote login.

The RDP bridge takes its physical keyboard map from Niri, including your configured
layouts, variants, and options. It preserves layout switching and updates when
Niri's keyboard configuration changes. On a fresh installation, keep your usual
Niri, input-method, environment, and autostart configuration. No Niri-RDP-specific
language configuration is required.

Log out and start a fresh remote login after upgrading startup settings; a
reconnect preserves the running desktop and its environment.

Review `/etc/niri-rdp/frontend.conf`, `/etc/niri-rdp/headless.kdl`,
`/etc/niri-rdp/waybar.json`, `/etc/niri-rdp/renderer.conf`,
`/etc/niri-rdp/broker.conf` (see [broker settings](#broker-settings)), and
`/etc/pam.d/niri-rdp`. `NIRI_RDP_LISTEN` accepts a numeric IP address and port,
including bracketed IPv6. To use the standard RDP port on a host where it is free,
set `NIRI_RDP_LISTEN=0.0.0.0:3389`. To retain tunnel-only access, set
`NIRI_RDP_LISTEN=127.0.0.1:33991`. Restart the frontend after changing its config.

```sh
sudo systemctl status niri-rdp-broker.service niri-rdp-frontend.service
sudo journalctl -b -u niri-rdp-broker.service -u niri-rdp-frontend.service --no-pager
ss -ltn 'sport = :33991'
```

The login screen reports **Login denied. Check account and password.** for
authentication or account-policy failures. **Desktop unavailable. Try again.**
means the broker could not start or resume the desktop, or the account's desktop
was still starting or ending; check both service journals for the cause.
**Desktop in use by another connection.** means another RDP connection is
attached to the account's desktop; disconnect it first. **A local desktop session
is still open.** means the account has a local graphical session, including one
still closing; log out of it locally. These three messages appear only after the
password and `niri-rdp-users` membership were accepted; every earlier failure
shows the generic denial. These startup failures do not consume the login screen's
failed-password attempt limit. Release 14 waits for the private WirePlumber
default-device policy before selecting the remote audio output, fixing a startup
race that previously rejected a correctly authenticated login.
The **Password (length hidden)** field shows eight stars for any nonempty entry;
physical dead keys and Compose sequences produce the completed character.

The login screen disconnects after 60 seconds without a submitted attempt or
120 seconds in total; the limits pause while PAM and desktop startup run.
`NIRI_RDP_LOGIN_IDLE_SECS` (10–600, default 60) and `NIRI_RDP_LOGIN_TIMEOUT_SECS`
(30–1800, default 120) change them; the idle limit must not exceed the total,
and invalid values stop frontend startup. Rejected logins from one client
address also start a per-source backoff, for both the login screen and direct
client credentials: the first failure is free, then waits grow 1, 2, 4 … up to
60 seconds. During a wait the same generic rejection is returned without
contacting PAM. A successful login clears the source, and entries expire after
15 quiet minutes. IPv6 clients are grouped by /64, and clients behind one NAT or
tunnel share a backoff. Desktop startup failures are not counted.

`/etc/pam.d/niri-rdp` is a complete stack and does not include
`system-remote-login`. Only regular accounts in `niri-rdp-users` can authenticate,
and root or nonmember attempts take as long as a wrong password. Failed RDP logins
lock the account only for niri-rdp (5 failures, 300 seconds); the console, sudo
and other services keep their own tally. The broker sets `PAM_RHOST` to the RDP
client's address. The session stack includes `pam_limits` and `pam_umask`, as in
Arch's `system-login`. Remote desktops therefore get the umask from
`/etc/login.defs` (022 by default) and a core-dump limit of soft 0 / hard
unlimited unless `pam_limits` configures one. They share `/tmp` with the account's
other logins, like SSH and display-manager sessions. Reset an account with:

```sh
sudo faillock --dir /run/faillock-niri-rdp --user NAME --reset
```

The PAM file is package configuration. Upgrades over a locally edited copy
produce `/etc/pam.d/niri-rdp.pacnew`; merge it and do not keep an include of
`system-remote-login`.

A TLS-only fallback profile is available if a client's normal connection setup
fails. Save this as `niri.rdp` on Windows, replace the address and account, and
open it with the regular Remote Desktop client:

```text
full address:s:192.168.1.50:33991
username:s:niri-remote
enablecredsspsupport:i:0
authentication level:i:2
prompt for credentials:i:1
desktopwidth:i:1920
desktopheight:i:1080
```

Never store the password in the profile. This does not override an NLA-required
Windows policy. Client-selected initial resolution and single-display live
resizing are supported; see [the resolution contract](../bridge/README.md#dynamic-resolution).
Only password PAM authentication is implemented; MFA and password-expiration
conversations fail closed.

### Broker settings

`/etc/niri-rdp/broker.conf` holds the broker's settings, one `key = value` per
line with `#` comments. Its only key is `grace_seconds` (0–3600, default 45):
how long a disconnected desktop keeps running for a reconnect. `0` logs the
desktop out as soon as the RDP connection ends.

The broker reads the file once at start and checks it strictly. It must be a
regular file (not a symbolic link) owned by root, not writable by group or
others, and at most 4096 bytes. An unknown or repeated key, an invalid value or
a failed file check rejects the whole file: the broker logs a warning to its
journal and uses the default for every setting, but still starts. A missing
file also means defaults. The journal records the period in use as
`detach grace period: N seconds`.

Restart the broker to apply a change. **Restarting the broker ends every
connected and retained remote desktop**, so save your work and run it from a
local terminal or an independent SSH session:

```sh
sudo systemctl restart niri-rdp-broker.service
sudo journalctl -b -u niri-rdp-broker.service | grep -i 'grace\|broker.conf'
```

The file is package configuration: pacman keeps an edited copy and installs new
defaults as `/etc/niri-rdp/broker.conf.pacnew`.

### Login audit log and fail2ban

The broker writes one journal entry per login event with the identifier
`niri-rdp-broker` and the fields `NIRI_RDP_EVENT` (`auth-fail`, `login`,
`resume`, `detach`, `grace-expire`, `logout` or `error`), `NIRI_RDP_USER`,
`NIRI_RDP_RHOST` (the client address), `NIRI_RDP_RESULT` and
`NIRI_RDP_SESSION`. Passwords are never logged. Desktop events after login come
from the session's holder process, which runs in the logind `session-N.scope`
rather than the broker service, so `journalctl -u niri-rdp-broker.service` does
not show them. Query by identifier or field instead:

```sh
sudo journalctl -t niri-rdp-broker
sudo journalctl NIRI_RDP_EVENT=auth-fail
sudo journalctl NIRI_RDP_USER=alice -o verbose
```

A fail2ban filter and a commented jail example are installed as documentation
under `/usr/share/doc/niri-remote-login/fail2ban/`; nothing enables them. The
filter matches only `auth-fail` entries written by root inside
`niri-rdp-broker.service`, so a local user cannot forge them with `logger`. To
ban addresses at the firewall after repeated failures:

```sh
sudo pacman -S --needed fail2ban
sudo install -m 644 /usr/share/doc/niri-remote-login/fail2ban/niri-rdp.conf \
  /etc/fail2ban/filter.d/niri-rdp.conf
sudo install -m 644 /usr/share/doc/niri-remote-login/fail2ban/niri-rdp.jail.example \
  /etc/fail2ban/jail.d/niri-rdp.local
sudoedit /etc/fail2ban/jail.d/niri-rdp.local
sudo systemctl enable --now fail2ban.service
sudo fail2ban-client status niri-rdp
```

In `niri-rdp.local`, uncomment the jail, set `port` to the port of
`NIRI_RDP_LISTEN` and add your own client networks to `ignoreip`, so a mistyped
password cannot lock you out of a remote-only host. The frontend's per-address
backoff and the PAM lockout apply with or without fail2ban.

### Remote microphone

The Windows microphone can be used by applications in the remote desktop
(Firefox, Discord, `pw-record` and similar). In mstsc, open **Show Options →
Local Resources → Remote audio → Settings** and select **Record from this
computer** (saved `.rdp` profiles use `audiocapturemode:i:1`). Keep **Play on
this computer** selected for playback. In Windows 10, **Settings → Privacy →
Microphone → Allow desktop apps to access your microphone** must be on.

The frontend setting `NIRI_RDP_MICROPHONE` in `/etc/niri-rdp/frontend.conf`
accepts `auto` (the default, also when the line is absent) or `off`. Any other
value stops the frontend at startup with an error. `off` offers no microphone
channel to the client at all; playback is unaffected. The remote source still
exists with `off` but only records silence. Changing the setting needs
a frontend restart, which disconnects the current RDP connection, so arrange it
from a local terminal or SSH.

Inside the remote desktop the microphone is the PipeWire source
**Remote microphone (RDP)** (`niri-rdp-microphone`), and it is the default
source, so applications use it without configuration. The default output stays
**Remote Desktop Audio**. Windows records only while a remote application
listens: the Windows microphone indicator appears when an application starts
recording and goes off about 5 seconds after the last one stops. With no
recorder, nothing is uploaded. The source outputs silence while no audio
arrives.

The microphone uses 48 kHz mono 16-bit PCM in 20 ms packets, about 0.8 Mbit/s
of upload from the client before framing (44.1 kHz or stereo only when the
client cannot record 48 kHz mono). Measured in the lab, a sample takes about
120–145 ms from the client's capture to a remote recorder, of which about
55 ms is client-side; queues on both sides hold at most 100 ms, so the delay
cannot grow. The packaged Waybar (`/etc/niri-rdp/waybar.json`, used when your
Niri configuration starts no bar of its own) shows a microphone icon while a
remote application records; with DankMaterialShell or your own Waybar, use their
privacy or microphone indicator instead.

The microphone path is separate from playback: its own dynamic RDP channel,
socket, session helper and restart budget. A microphone failure never ends the
connection or the desktop, and the accepted PCM playback configuration is
unchanged. A failed session helper is restarted twice at most (then the
microphone stays off until a fresh remote desktop); reconnect after a restart
to use the microphone again. A frontend-side failure leaves it off until the
next connection. To troubleshoot:

```sh
# In the remote desktop: the source exists and is the default.
pactl list sources short
pactl get-default-source
# On the host: microphone events and per-connection counters.
sudo journalctl -u niri-rdp-frontend | grep -E "RDP microphone|connection summary"
sudo journalctl -u niri-rdp-broker | grep "remote microphone"
```

In the [connection summary](#connection-summaries), `mic=` names the format
used (for example `mic=pcm_48000_mono`; `mic=none` means Windows never recorded,
typically because **Record from this computer** is off or no application
listened), followed by `mic_opens`, `mic_packets`, `mic_bytes`, `mic_dropped`
and `mic_errors`. A nonzero `mic_errors` means the client refused or did not
answer a recording request within 10 seconds, sent invalid audio, or the
session helper connection failed. A growing `mic_dropped` points at a stalled
helper or late bursts after network stalls (audio older than 100 ms is
discarded rather than delayed). After a refused or unanswered request, Windows is asked again only when
an application starts recording at least 30 seconds later.

### Connection summaries

The frontend writes one `RDP connection summary` line to its journal when each
connection ends, including failed TLS handshakes and connections dropped without
a clean disconnect. It is always on and needs no diagnostics setting:

```sh
sudo journalctl -u niri-rdp-frontend | grep "connection summary"
```

Each line shows the client address, duration, `result=ok` or `error`, the video
codec and mode actually used (`video_codec=bitmap` means no H.264 frames were
sent), frame and IDR counts, average video kbps, the audio codec, waves,
discarded blocks, credit waits and codec recoveries, clipboard transfers each
way, timeouts and oversize drops, and the [microphone](#remote-microphone)
format, opens, packets, bytes, drops and errors. It contains no usernames, error text, clipboard
content or credentials; [performance controls](performance.md#configuration)
describe every field. For example, `audio_codec=pcm` confirms the accepted PCM
path, and a growing `audio_discarded_blocks` or `clipboard_timeouts` points at the
link or client rather than the desktop.

Local clipboard text, HTML or image data larger than its limit (1 MiB, 4 MiB
and 32 MiB) is not offered to Windows. The frontend logs one warning naming only
the format and limit, such as `local clipboard text exceeded 1048576 bytes and
was not offered to the client`, and counts it in `clipboard_oversize_drops`.

## Maintainer builds and older artifacts

The older split package workflow remains available:

```sh
python3 scripts/prepare-release.py
```

It exports hash-checked source bundles under `artifacts/release/` for separate
`niri-rdp-headless` and `niri-remote-login` builds. The new direct build folder
needs the complete repository checkout; copy or clone the repository rather than
copying only the PKGBUILD. `makepkg --allsource` exports a portable source bundle.
The combined package provides and conflicts with `niri-rdp-headless`, allowing
pacman to replace a previously installed split compositor package. For a split
build, install both packages together: `niri-remote-login` requires
`niri-rdp-headless>=26.04.remote2-1`, because the bridge's cursor depends on the
current compositor patches (see [upgrade](upgrade.md#rebuild-the-installed-combined-package)).

The archived `0.1.0-1` delivery is a preserved snapshot. It lacks current
resolution support, NVENC, direct listening, automatic certificate setup, and
the login screen. Rebuild current source to use this installation procedure.
Use [uninstall](uninstall.md) and [upgrade](upgrade.md) for removal and maintenance.

## Optional NVIDIA encoding

The compositor now uses EGL device rendering to support NVIDIA headless graphics,
without a local desktop or display server. NVIDIA documents the
[EGL device platform](https://developer.nvidia.com/docs/drive/drive-os/6.0.6/public/drive-os-linux-sdk/api_reference/EGL_EXT_platform_device.html).
The logged-in account needs permission to use the GPU's render node and NVIDIA
device nodes. Inspect their ownership on the test machine; if group access is
needed, add the remote login account to the appropriate `render`/`video` groups
and create a fresh remote session. The frontend drop-in below affects the encoding
service account; it does not grant compositor GPU access to remote login accounts.

`/etc/niri-rdp/renderer.conf` contains one word: `auto` (default), `hardware`, or
`software`. Set `hardware` during GPU validation so missing drivers or permissions
fail compositor startup rather than quietly using the fallback. Set `software`
to force the previous llvmpipe behavior. `software` also restricts glvnd to
Mesa's EGL vendor (`/usr/share/glvnd/egl_vendor.d/50_mesa.json`) for the
compositor and session apps, so llvmpipe is used even with NVIDIA drivers
installed. Changes apply to new sessions. Inspect
the broker journal for `headless renderer: hardware EGL device` and the render
node path and `nvidia=true` to verify NVIDIA selection. The isolated lab has
validated actual NVIDIA rendering with NVENC and FreeRDP decoding on an RTX
4070 Ti; installed session permissions and end-to-end throughput still need
the [actual-machine checks](acceptance.md).

`NIRI_RDP_FPS` in `frontend.conf` limits capture/transmission to 1–60 frames per
second (default 60 for NVENC, 30 for bitmap). `NIRI_RDP_BITRATE_MBPS` accepts
1–100 megabits per second (default 12) as the NVENC bitrate ceiling. Each
connection starts at that ceiling and steps down through six quantized levels
toward 1 Mbps when graphics acknowledgements slow relative to the connection's
baseline, stall, or report a client decoding backlog, at most one level per
second. Audio receipt or playback congestion also reduces the video target,
including while graphics acknowledgements are suspended. It rises one level
after 10 seconds of healthy delivery without audio congestion, applied at the
next due keyframe or within 30 seconds.
Changes apply to the live encoder without resetting the desktop. Reduce the
frame rate or ceiling for limited networks; increasing the ceiling can improve
AVC420 quality when the connection has enough bandwidth. Bitmap fallback does
not use this bitrate controller.

Audio prefers stereo 48 kHz PCM at about 1.54 Mbps before framing. This restores
the format that was intelligible on Windows 10 mstsc after continuous static
was reported with AAC. Clients that do not accept PCM can use stereo AAC-LC at
96 kbit/s, followed by MS ADPCM. Digital silence is suppressed
after a short codec tail. Blocks are paced by their exact sample duration and keep
recent capture samples when the source or network stalls. Recent, validated
playback feedback can briefly discard capture to let a client buffer drain.
Optional second confirmations never gate playback; missing replies and old idle
waves cannot mute sound or continually reduce video quality. Recovery resumes
consecutive fresh AAC packets after a bounded wait. Fresh completion feedback
can recover a backlog longer than one second while the same playback run is
active and the block number is unambiguous; old-run and reused-number feedback
cannot restart recovery. Each valid measurement extends recovery by at most
one second. Recovery may skip sound
during congestion. No manual delay or
codec setting is needed. These controls limit accumulating delay; actual mstsc
audio/video presentation still needs the playback check below.

To inspect playback, the frontend journal normally reports `RDP audio PCM`
(`RDP audio AAC-LC` for clients without PCM), plus
`RDP audio delay: receipt ... ms, render ... ms` during buffer recovery
and `RDP graphics adaptive H.264 target` during video adaptation. Test a clip with visible
claps or simultaneous flash/tone for several minutes, then repeat after a short
network stall. Record whether the offset stays constant or keeps growing; see
the [audio/video acceptance check](acceptance.md). When upgrading from an active
RDP desktop, build first and arrange the service restart from a local terminal
or SSH: restarting the frontend disconnects the client, and stopping the broker
logs out the desktop.

The default `NIRI_RDP_ENCODER=bitmap` sends bitmap updates. A rebuilt frontend
supports `NIRI_RDP_ENCODER=nvenc`, which passes captured desktop frames to
FFmpeg's `h264_nvenc` encoder and sends H.264 through the RDP graphics channel
as AVC420 by default. Explicit AVC444 selection also requires client negotiation;
see [performance controls](performance.md). Without compatible graphics support the
connection uses bitmap updates. This is host-side encoding. Decoding happens
on the RDP client, whose hardware acceleration depends on its own GPU and client.
The current combined package includes NVENC; the archived
`0.1.0-1` frontend binaries contain neither these fixes nor NVENC support.
NVENC uses supported DMA-BUF capture by default (`NIRI_RDP_GPU_CAPTURE=auto`):
EGL conversion and CUDA/NVENC surfaces, with SHM fallback. Set `shm` to force
CPU readback and full-range BT.709 conversion; bitmap mode always uses SHM.
Periodic keyframes are a 30-second safety net (`NIRI_RDP_GOP_SECONDS`, 1–3600);
refresh, resize, resume and frame drops still force keyframes immediately.
See [device requirements and measurements](performance.md); GPU compositor
rendering alone does not prove that capture stays on the GPU.

On the separate test installation, use an NVENC-capable NVIDIA GPU, a working
NVIDIA driver with its userspace libraries, and an FFmpeg build providing
`h264_nvenc`. The driver must support the NVENC API version used by that FFmpeg
build. Listing the encoder confirms build support; it does not prove that the
GPU can initialize it:

```sh
nvidia-smi
ffmpeg -hide_banner -h encoder=h264_nvenc
```

The bridge links the distribution's FFmpeg libraries; its build and runtime do
not need a separately installed CUDA toolkit or Video Codec SDK. NVIDIA's
[FFmpeg guide](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/ffmpeg-with-nvidia-gpu/index.html)
describes the runtime requirements for a compiled FFmpeg installation. Installing
or building FFmpeg itself is a separate task.

The standard frontend unit hides GPU devices. Enable the packaged NVIDIA
drop-in, which makes host devices visible but allows only the NVIDIA (`nvidia`,
`nvidiactl`, `nvidia-modeset`, `nvidia-uvm`) and DRM device groups, and adds the
`video` and `render` supplementary groups; the base unit keeps
`DevicePolicy=closed`:

```sh
sudo install -d -m 755 /etc/systemd/system/niri-rdp-frontend.service.d
sudo install -m 644 /usr/share/doc/niri-remote-login/nvidia-frontend.conf \
  /etc/systemd/system/niri-rdp-frontend.service.d/nvidia.conf
sudo systemctl daemon-reload
sudo systemctl restart niri-rdp-frontend.service
sudo journalctl -u niri-rdp-frontend.service -n 50 --no-pager
```

Re-copy the drop-in after upgrading: drop-ins copied from older releases have no
`DeviceAllow` lines, so NVENC fails with a device permission error.

Explicit NVENC configuration fails startup if hardware encoder initialization
fails. Check the journal for missing driver libraries, device permissions or an
incompatible GPU/driver. Client codec fallback does not mask that host startup
failure. Actual NVIDIA encoding and Windows graphics negotiation require the
[acceptance checks](acceptance.md); the existing software-rendered bitmap
integration result does not establish their performance.

The frontend automatically starts `niri-rdp-nvidia-devices.service` first.
It runs `nvidia-modprobe -c 0 -c 255` and `nvidia-modprobe -u -c 0` as root
to prepare `/dev/nvidia0`, `/dev/nvidiactl`, `/dev/nvidia-uvm`, and
`/dev/nvidia-uvm-tools`. Release 13 omitted the control-device minor and only
loaded UVM without explicitly creating its nodes. A headless boot can otherwise
have the driver loaded but lack the devices CUDA needs, causing
`CUDA_ERROR_NO_DEVICE`. The frontend keeps `NoNewPrivileges=yes`, so it cannot
use NVIDIA's setuid fallback to create those nodes itself. The helper also runs
with existing NVIDIA drop-ins from older package releases. Inspect its journal
if device preparation fails:

```sh
sudo journalctl -b -u niri-rdp-nvidia-devices.service --no-pager
```

After reinstalling, reboot and use SSH or a TTY to inspect the journals before
starting a local graphical session or running `nvidia-smi`, FFmpeg, or a GPU
application. Those can create the missing nodes and conceal a boot failure:

```sh
sudo journalctl -b -u niri-rdp-nvidia-devices.service \
  -u niri-rdp-frontend.service --no-pager
systemctl is-active niri-rdp-frontend.service
```

The NVIDIA helper must succeed and the frontend journal must report
`NVENC H.264 preflight passed`. The frontend must be active without a manual
GPU initialization command.

Test the encoder independently using the same service account, supplementary
groups and GPU visibility. This probe opens no RDP listener and uses synthetic
frames. Its output path must not already exist:

```sh
sudo systemd-run --unit=niri-rdp-nvenc-probe --wait --pipe --collect \
  --property=User=niri-rdp --property=Group=niri-rdp \
  --property='SupplementaryGroups=video render' --property=PrivateDevices=no \
  /usr/lib/niri-rdp/encoder-probe /tmp/niri-rdp-nvenc.h264
ffmpeg -v error -i /tmp/niri-rdp-nvenc.h264 -f null -
ffprobe -v error -select_streams v:0 -count_frames \
  -show_entries stream=codec_name,width,height,nb_read_frames \
  -of default=noprint_wrappers=1 /tmp/niri-rdp-nvenc.h264
```

Require a successful probe, error-free decoding and `codec_name=h264`,
`width=1920`, `height=1088`, `nb_read_frames=30`. AVC420 requires coded dimensions
aligned to 16 pixels, so the standalone recording includes eight padded rows.
RDP region masks crop those rows to the logical 1920×1080 desktop. This tests the hardware encoder
and device permissions; it does not establish RDP client negotiation or recreate
every frontend service hardening setting.

To restore the standard bitmap configuration:

```sh
sudo rm /etc/systemd/system/niri-rdp-frontend.service.d/nvidia.conf
sudo systemctl daemon-reload
sudo systemctl restart niri-rdp-frontend.service
```

## Performance and desktop upgrades

The combined 1.5 package includes damage-aware capture, input/resize coalescing,
cached login rendering, stale-ACK recovery, separate negotiated cursor updates,
a private bidirectional clipboard and deliberate DPI scaling. The clipboard
carries Unicode text (up to 1 MiB), HTML (up to 4 MiB) and images (up to 32 MiB
and 32768 pixels per side) in both directions, with finite transfers. Files are
not supported: copying files in Windows leaves the Linux clipboard unchanged.
A clipboard message over the size limit is dropped and that paste fails; the
session continues. Enable clipboard in mstsc Local Resources.

[Performance controls](performance.md) document optional AVC444, GPU capture,
quality profiles, coordinated graphics cadence/window, total link budget and
diagnostics. Existing speed/AVC420/SHM defaults and the accepted PCM audio controls
remain the baseline. New audio framing and helper supervision are packaged with
the frontend; a clean reinstall starts matching components. Audio failure permits
a usable desktop; reconnect after helper recovery obtains fresh capture.

After your clean reinstall, run the Windows and installed-machine checklist in
[acceptance](acceptance.md), including thirty-minute playback. The recorded
namespace/decoder checks do not substitute for that listening or display test.
