# Repository Guidelines

## Project Structure & Module Organization

- `bridge/src/`: Rust IronRDP frontend, Wayland capture/input, audio, and native video encoding (`video.c`).
- `broker/`: C PAM/logind session broker, native tests, and session helpers/configuration under `config/`.
- `compositor/`: pinned Niri sources, revision metadata, and maintained patches in `patches/`.
- `tests/`: Python regression suites and C client/video fixtures. `poc/` and `prototype/` retain foundational implementations.
- `lab/` and `scripts/`: disposable integration fixtures, build helpers, and release/package validation tools.
- `packaging/`: Arch PKGBUILDs, systemd units, and host configuration. `docs/` and component `CHECKPOINT.md` files record architecture and validation; generated evidence belongs in ignored `artifacts/`.

## Build, Test, and Development Commands

Run from the repository root unless noted:

- `cargo build --manifest-path bridge/Cargo.toml --release --locked`: build the frontend; requires Rust >=1.94, libxkbcommon, FFmpeg libraries, and pkg-config.
- `make -C broker`: build the C broker against PAM and systemd.
- `python3 -m unittest discover -s tests -v`: run Python regressions.
- `make check`: run Python, Rust, PoC startup, and broker checks. Rust checks use locked dependencies offline; populate the Cargo cache first.
- `lab/run-isolated.sh python3 scripts/headless-integration.py`: exercise real compositor capture/input and RDP in private namespaces after preparing lab artifacts; see `compositor/README.md`.
- `makepkg` from `packaging/arch/niri-rdp/`: build the combined Arch package as an unprivileged user.

## Coding Style & Naming Conventions

Follow adjacent code: four-space indentation for Rust, Python, and C; tabs for Makefile recipes. Use `snake_case` functions/modules and descriptive `test_` names. Format Rust with `cargo fmt --manifest-path bridge/Cargo.toml`; use Cargo Clippy for lint review. Broker compilation enforces C17 and warnings as errors. Preserve dependency pins and lockfiles.

## Testing Guidelines

Use Python `unittest`, Rust unit tests, and `broker/tests.c` assertions. Add regressions for behavioral fixes, including failure/cleanup paths. Python suites use `tests/test_*.py`. No numeric coverage threshold is configured. Record integration evidence in `docs/validation.md`; installed PAM/logind and Windows acceptance remain separate checks in `docs/acceptance.md`.

## Commit & Pull Request Guidelines

History uses brief descriptions such as “Added remote audio support.” and “Fixed Firefox issues.” Keep commits focused. PRs should explain behavior changes, link relevant issues, list validation and limitations, and include screenshots for login/UI changes.

## Security & Configuration

Develop with private display sockets and disposable sessions. Keep credentials and TLS keys out of Git. Stage broker installation with an absolute `DESTDIR`; validate packages before changing host services.
