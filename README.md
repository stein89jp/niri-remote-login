# Niri Remote Login — experimental Arch packages

An implementation of a dedicated headless Niri desktop, an IronRDP frontend,
and a PAM/logind session broker, prepared for testing on a **separate Arch Linux
Niri installation**. Nothing is installed or enabled on the working host.

The real Niri-to-RDP bridge has passed a disposable Arch namespace test: changing
Wayland application frames, keyboard and pointer feedback, authentication
rejection, reconnect and input releases. A maintained pinned Niri PR #3953 patch
supplies the headless backend. Its binary is packaged alongside stock Niri.
The privileged broker is implemented and native-tested; **actual PAM/logind boot
login, Windows mstsc, and physical-monitor-off validation remain pending**.
This is not yet a demonstrated production remote-login service.

- [Install/build on the separate test machine](docs/install.md)
- [Actual-machine acceptance and pending gates](docs/acceptance.md)
- [Uninstall](docs/uninstall.md) and [upgrade/rollback](docs/upgrade.md)
- [Compositor pins and patches](compositor/README.md)
- [Real Wayland/RDP bridge](bridge/README.md) and [session broker](broker/README.md)
- [Integration checkpoint and remaining work](checkpoints/001-integration.md)

The prepared delivery is `artifacts/installable/`: two x86_64 Arch packages,
matching source bundles, SHA-256 checksums, and validation receipts. A portable
archive is `artifacts/niri-remote-login-arch-0.1.0.tar.gz`. Use these only on the
separate test installation; boot-to-login acceptance has not been demonstrated.

The private repository also keeps the validated snapshot in
[the Arch delivery archive](releases/arch-x86_64/0.1.0/niri-remote-login-arch-0.1.0.tar.gz)
with its [SHA-256 checksum](releases/arch-x86_64/0.1.0/niri-remote-login-arch-0.1.0.tar.gz.sha256).
Download or clone it, verify the checksum, and extract it to obtain `installable/`.
The source, checkpoint and validation receipts inside that archive describe the
exact package build; subsequent repository changes do not alter this snapshot.

Prepare source package bundles without installing anything:

```sh
python3 scripts/prepare-release.py
```

The resulting `artifacts/release/` directories contain hash-checked PKGBUILDs and
sources. Build with `makepkg` on a disposable Arch builder, inspect the packages,
then install them only on the intended test machine. The frontend is fixed to
`127.0.0.1:33991`; Windows must use an explicit private tunnel. Broker login mode
uses TLS-only RDP and password-only PAM; NLA-required Windows policies currently
prevent this mode. No custom RDP protocol is implemented.

No GNOME Remote Login settings, services, accounts, packages, or power settings
were changed. The packages contain no GNOME/GDM files, no stock Niri replacement,
and no automatic service enablement.

- [Architecture and phase gates](docs/architecture.md)
- [Niri APIs and upstream changes](docs/niri-findings.md)
- [Login, authentication, and RDP libraries](docs/login-and-rdp.md)
- [RDP library proof of concept](poc/README.md)
- [Validation record](docs/validation.md)
- [Pinned source manifest](research/upstream/manifest.json)

The preserved initial prototype has two independent parts: a synthetic RDP display and a
current-user process-isolation harness. They do not yet form a Niri desktop or
authenticate Linux users. The process harness uses trusted test workers, private
directories and D-Bus, and separate process groups. It is not a security boundary
against hostile code and does not create a PAM/logind session.

Run the isolation tests with:

```sh
python3 -m unittest discover -s tests -v
```

These tests need permission to create a temporary Unix socket for private D-Bus.
See the PoC README for the separate Rust tests and explicit loopback launch.
Nothing starts a network service automatically.

Baseline validation: 17 tests passed, including a displayless FreeRDP client that verifies
TLS/CredSSP rejection/success, decoded pixels, keyboard/pointer feedback, input
release and a fresh connection. The opt-in integration test starts and stops a
temporary listener on `127.0.0.1:33990`. Actual PAM/logind boot login, Windows `mstsc.exe`, and
physical-monitor power-off remain actual-machine gates. See the validation
record for the additional real-desktop integration and package results.

