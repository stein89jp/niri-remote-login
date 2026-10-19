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
- `make check`: run `make lint` (bridge `cargo fmt --check` and Clippy `-D warnings`), then the Python, broker, and bridge Rust checks. Rust checks use locked dependencies offline; run `make fetch` once, or rely on the makepkg cache in `packaging/arch/niri-rdp/src/cargo-home`.
- `make check-poc`: run the historical `poc/` transport prototype tests and startup checks separately; `make check` no longer includes them.
- `lab/run-isolated.sh python3 scripts/headless-integration.py`: exercise real compositor capture/input and RDP in private namespaces after preparing lab artifacts; see `compositor/README.md`.
- `makepkg` from `packaging/arch/niri-rdp/`: build the combined Arch package as an unprivileged user.

## Coding Style & Naming Conventions

Follow adjacent code: four-space indentation for Rust, Python, and C; tabs for Makefile recipes. Use `snake_case` functions/modules and descriptive `test_` names. Format Rust with `cargo fmt --manifest-path bridge/Cargo.toml`; use Cargo Clippy for lint review. Broker compilation enforces C17 and warnings as errors. Preserve dependency pins and lockfiles.

## Testing Guidelines

Use Python `unittest`, Rust unit tests, and `broker/tests.c` assertions. Add regressions for behavioral fixes, including failure/cleanup paths. Python suites use `tests/test_*.py`. No numeric coverage threshold is configured. Record integration evidence in `docs/validation.md`; installed PAM/logind and Windows acceptance remain separate checks in `docs/acceptance.md`.

## Preserve the Working Audio Combination

On 2026-10-12, the user confirmed that release 19 fixed continuous crackling and delivered minimal audio latency from this Arch/Niri host to Windows 10's native `mstsc.exe`. On 2026-10-18, after reporting audio delay that grew steadily during internet sessions, the user explicitly requested two changes to that baseline: mono PCM preference and a periodic 20 ms drift skip. Their actual mstsc clarity/latency validation is **pending**. Treat this combination as the accepted baseline:

- Prefer **48 kHz mono S16LE PCM** (stereo capture downmixed in the bridge as (L+R)/2, about 768 kbit/s before framing), then **48 kHz stereo S16LE PCM** (about 1.54 Mbit/s), paced in **20 ms blocks**, ahead of AAC and MS ADPCM. Compressed formats remain fallbacks for clients without PCM; the PCM bandwidth cost is intentional.
- During continuous audible PCM, drop one 20 ms block only when the drop is earned: a model of mstsc's queue, fed by first Wave Confirms plus a 100 ppm client-clock drift allowance and a starvation credit (20 ms when the model runs dry for 100 ms or more, at most once per 30 s), capped at 1 s and reset at silence suppression, must show a surplus of at least a 60 ms jitter reserve plus 20 ms. The frontend settings `NIRI_RDP_AUDIO_CHANNELS` (default `mono`) and `NIRI_RDP_AUDIO_DRIFT_PPM` (default 100) must default to this baseline. Drop at most once per 10 s (counted also from a run's start or resume), preferring a quiet block and forcing one after 5 s more; the next block fades in over 5 ms from the dropped block. The drop keeps the send schedule, never drops digital silence, and is not capture loss, a credit wait or a pressure event. It is disabled for AAC/ADPCM. mstsc confirms waves on arrival, so its playback queue is otherwise invisible to the server. On 2026-10-20 the user reported perfect A/V sync but crackling, robotic sound with the earlier blind drop every 30 s, and asked to fix quality without reverting; this earned drop replaced it, and its mstsc listening validation is pending.
- Keep **both RDPSND timestamps zero** for the current RDPGFX desktop path, which has no shared RDPEVOR presentation clock. Use internal monotonic time for pacing and delay measurements.
- Preserve bounded capture and transport: four pending waves, three capture blocks, 60 ms capture expiry, silence suppression, and recent validated playback feedback with finite recovery. Optional second confirmations must never become required credits or indefinite playback gates.
- Ordinary transport-credit waits must preserve recent queued capture and AAC state. Reset AAC after actual sample loss, not merely because the credit window fills.
- Do not promote AAC/ADPCM or undo the latency controls as a routine optimization or upstream-alignment change. Earlier ADPCM playback produced static; AAC decoded successfully in FFmpeg, FreeRDP and FDK fixtures but still produced continuous crackling in actual mstsc playback. PCM alone with the older controls also accumulated minutes of delay: preserve the codec choice and current controls together.

Any deliberate change to this baseline needs an explicit user request or approval and actual Windows 10 mstsc clarity/latency validation; automated decoder success alone is insufficient. Keep the PCM preference/wire-byte, downmix, drift-skip and credit-wait continuity regressions. See [the validation record](docs/validation.md#continuous-mstsc-crackling-and-pcm-playback-2026-10-12), [the 2026-10-18 change](docs/validation.md#growing-audio-delay-mono-pcm-and-drift-skip-2026-10-18) and [acceptance checks](docs/acceptance.md). The user confirmed the immediate 2026-10-12 stereo result; the 2026-10-18 mono/drift-skip listening check, long-running lip-sync, constrained-bandwidth and repeated-reconnect listening checks remain separate acceptance work.

## Commit & Pull Request Guidelines

History uses brief descriptions such as “Added remote audio support.” and “Fixed Firefox issues.” Keep commits focused. PRs should explain behavior changes, link relevant issues, list validation and limitations, and include screenshots for login/UI changes.

The user handles all Git and GitHub operations (`git add`, `git commit`, `git push`, branches, PRs) themselves. Do not stage, commit, push or open PRs unless the user explicitly asks for that specific action; leave changes in the working tree and summarize them instead.

## Change Validation on This Host

The user develops on and connects to this same Arch/Niri host through Niri-RDP from Windows 10 `mstsc.exe`. After a change to the system, the user validates it with a clean reinstall: remove the current installation with [docs/uninstall.md](docs/uninstall.md), then install fresh with [docs/install.md](docs/install.md). Do not live-patch installed services or files under `/usr`, `/etc` or systemd as a shortcut. When a change affects installation, packaging, services or configuration, keep both documents accurate, and tell the user when a change needs that reinstall cycle before it can be tested. Restarting the installed services ends the user's current RDP session, so never do it on their behalf.

## Security & Configuration

Develop with private display sockets and disposable sessions. Keep credentials and TLS keys out of Git. Stage broker installation with an absolute `DESTDIR`; validate packages before changing host services.
