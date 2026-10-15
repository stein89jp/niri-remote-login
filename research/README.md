# Research evidence

`upstream/` holds selected source files downloaded from pinned public commits,
not full buildable checkouts. Only `upstream/manifest.json` is tracked: it
records repository, commit date, source URL, file size and SHA-256 digest, so
the copies can be fetched again and verified. The copies themselves are local
research references (superseded by `bridge/vendor/` and `compositor/pins.json`),
not copied into the implementation, not shipped and not part of release source
bundles. They retain upstream licensing.

The Niri manifest declares GPL-3.0-or-later; the included standard license text
comes from Arch's installed SPDX license collection. IronRDP's MIT/Apache
license texts come from the installed crates.io `ironrdp-server-0.13.0` source
cache. These license texts are not additional upstream code snapshots.

Read-only host observations on 2026-10-10:

- `/etc/os-release` identifies Arch Linux.
- Niri is not installed; `niri` is absent from PATH.
- GNOME Remote Desktop 51.0-1 and FreeRDP 3.32.1-1 are installed.
- Rust 1.99.0, Python 3.14.7, Wayland 1.26.0 and libei 1.6.0 are installed.
- `dbus-run-session`, `openssl`, `cc`, and FreeRDP server pkg-config metadata
  are available.

No GNOME credentials or configuration were read. No service state or power
state was changed. Findings apply to the pinned Niri source, rather than a
locally tested compositor. External package/PR versions can change after this
research date.
