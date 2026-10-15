# Targets:
#   make fetch    download locked Rust dependencies (network) into the
#                 project-local caches poc/.cargo-cache and bridge/.cargo-cache
#   make check    Python, broker and bridge tests plus `make lint`, offline
#   make check-poc  the historical poc/ transport proof of concept, offline
#   make lint     cargo fmt --check and clippy -D warnings for the bridge, offline
# Offline Cargo caches are chosen in this order: an explicit POC_CARGO_HOME or
# BRIDGE_CARGO_HOME, a populated project-local .cargo-cache, $CARGO_HOME, the makepkg
# cache packaging/arch/niri-rdp/src/cargo-home, then ~/.cargo.
.PHONY: check check-poc lint fetch check-transport release checkpoint
PACKAGE_CARGO_HOME := $(CURDIR)/packaging/arch/niri-rdp/src/cargo-home
DEFAULT_CARGO_HOME := $(firstword $(CARGO_HOME) $(wildcard $(PACKAGE_CARGO_HOME)) $(HOME)/.cargo)
# A project-local cache counts only once it holds downloaded crates, so an empty
# directory left by an interrupted fetch falls through to the next candidate.
local_cargo_home = $(if $(wildcard $(CURDIR)/$(1)/.cargo-cache/registry/cache/*),$(CURDIR)/$(1)/.cargo-cache)
POC_CARGO_HOME ?= $(firstword $(call local_cargo_home,poc) $(DEFAULT_CARGO_HOME))
BRIDGE_CARGO_HOME ?= $(firstword $(call local_cargo_home,bridge) $(DEFAULT_CARGO_HOME))
POC_FETCH_CARGO_HOME ?= $(CURDIR)/poc/.cargo-cache
BRIDGE_FETCH_CARGO_HOME ?= $(CURDIR)/bridge/.cargo-cache

check: lint
	python3 -m unittest discover -s tests -v
	$(MAKE) -C broker check
	CARGO_HOME="$(BRIDGE_CARGO_HOME)" cargo test --locked --offline --manifest-path bridge/Cargo.toml

# The poc/ crate is the 0.1.0 foundation prototype; nothing it builds is packaged.
check-poc:
	CARGO_HOME="$(POC_CARGO_HOME)" cargo test --locked --offline --manifest-path poc/Cargo.toml
	CARGO_HOME="$(POC_CARGO_HOME)" cargo build --locked --offline --manifest-path poc/Cargo.toml
	python3 poc/tests/startup.py

lint:
	cargo fmt --manifest-path bridge/Cargo.toml --check
	CARGO_HOME="$(BRIDGE_CARGO_HOME)" cargo clippy --locked --offline --manifest-path bridge/Cargo.toml --all-targets -- -D warnings

fetch:
	CARGO_HOME="$(POC_FETCH_CARGO_HOME)" cargo fetch --locked --manifest-path poc/Cargo.toml
	CARGO_HOME="$(BRIDGE_FETCH_CARGO_HOME)" cargo fetch --locked --manifest-path bridge/Cargo.toml

check-transport:
	POC_RDP_BINARY="$(CURDIR)/poc/target/debug/niri-rdp-transport-poc" python3 -m unittest tests.test_rdp_transport -v

release:
	python3 scripts/prepare-release.py

checkpoint:
	python3 scripts/checkpoint.py integration
