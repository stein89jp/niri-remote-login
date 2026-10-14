.PHONY: check check-transport release checkpoint
DEFAULT_CARGO_HOME := $(if $(CARGO_HOME),$(CARGO_HOME),$(HOME)/.cargo)
POC_CARGO_HOME ?= $(if $(wildcard $(CURDIR)/poc/.cargo-cache),$(CURDIR)/poc/.cargo-cache,$(DEFAULT_CARGO_HOME))
BRIDGE_CARGO_HOME ?= $(if $(wildcard $(CURDIR)/bridge/.cargo-cache),$(CURDIR)/bridge/.cargo-cache,$(DEFAULT_CARGO_HOME))

check:
	python3 -m unittest discover -s tests -v
	CARGO_HOME="$(POC_CARGO_HOME)" cargo test --locked --offline --manifest-path poc/Cargo.toml
	CARGO_HOME="$(POC_CARGO_HOME)" cargo build --locked --offline --manifest-path poc/Cargo.toml
	python3 poc/tests/startup.py
	$(MAKE) -C broker check
	CARGO_HOME="$(BRIDGE_CARGO_HOME)" cargo test --locked --offline --manifest-path bridge/Cargo.toml

check-transport:
	POC_RDP_BINARY="$(CURDIR)/poc/target/debug/niri-rdp-transport-poc" python3 -m unittest tests.test_rdp_transport -v

release:
	python3 scripts/prepare-release.py

checkpoint:
	python3 scripts/checkpoint.py integration
