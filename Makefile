# Friendly entry point for Anvil's Cargo builds. Cargo remains the source of truth.
.DEFAULT_GOAL := help

CARGO ?= cargo
INSTALL ?= install
CARGO_TARGET_DIR ?= target
PREFIX ?= /usr/local
DESTDIR ?=
SESSION_DIR ?= /usr/share/wayland-sessions

NO_BAR_FEATURES := anvilctl,launcher,overview,layer-shell
RELEASE_DIR := $(CARGO_TARGET_DIR)/release
BUILD_STAMP := $(CARGO_TARGET_DIR)/.anvil-make-variant
BUILD_SUMS := $(CARGO_TARGET_DIR)/.anvil-make-checksums
BIN_DIR := $(DESTDIR)$(PREFIX)/bin
DESKTOP_DIR := $(DESTDIR)$(SESSION_DIR)

.PHONY: help build build-no-bar build-minimal build-xwayland build-all dev run-nested \
        fmt lint test test-make check clean install uninstall

##@ Get started
help: ## Show this help (also the default target)
	@printf '\n  Anvil  ·  Build, test and install\n\n'
	@awk '\
		/^##@ / { section = substr($$0, 5); printf "\n  %s\n", section; next } \
		/^[a-zA-Z0-9_-]+:.*## / { \
			name = $$0; sub(/:.*/, "", name); \
			info = $$0; sub(/^.*## /, "", info); \
			printf "    %-19s %s\n", name, info \
		}' $(firstword $(MAKEFILE_LIST))
	@printf '\n  Example: make build-no-bar && sudo make install\n'
	@printf '  Paths:   PREFIX=/usr/local  SESSION_DIR=/usr/share/wayland-sessions\n\n'

##@ Build (release)
# A stamp and checksums tie 'install' to exactly the last Make-built release variant.
# Never run Cargo from 'sudo make install' or silently install a stale/wrong profile.
define build_release
	@rm -f "$(BUILD_STAMP)" "$(BUILD_SUMS)"
	@CARGO_TARGET_DIR="$(CARGO_TARGET_DIR)" $(CARGO) build --release --locked $(1)
	@set -eu; { \
		sha256sum "$(RELEASE_DIR)/anvil"; \
		if [ "$(2)" != minimal ]; then sha256sum "$(RELEASE_DIR)/anvilctl"; fi; \
	} > "$(BUILD_SUMS)"; \
	printf '%s\n' '$(2)' > "$(BUILD_STAMP)"; \
	printf 'Built Anvil (%s). Install with: sudo make install\n' '$(2)'
endef

build: ## Full desktop (bar, launcher, overview, IPC, layer-shell)
	$(call build_release,,default)

build-no-bar: ## Desktop without the built-in bar; keeps IPC, launcher and layer-shell
	$(call build_release,--no-default-features --features $(NO_BAR_FEATURES),no-bar)

build-minimal: ## Lean compositor without optional UI, IPC or layer-shell
	$(call build_release,--no-default-features,minimal)

build-xwayland: ## Standard desktop with optional XWayland support
	$(call build_release,--features xwayland,xwayland)

build-all: ## Enable every Cargo feature
	$(call build_release,--all-features,all)

##@ Develop
dev: ## Compile the default debug build
	@CARGO_TARGET_DIR="$(CARGO_TARGET_DIR)" $(CARGO) build --locked

run-nested: ## Run a nested Anvil session for testing
	@CARGO_TARGET_DIR="$(CARGO_TARGET_DIR)" $(CARGO) run --locked -- --nested

fmt: ## Check Rust formatting
	@$(CARGO) fmt --check

lint: ## Run strict Clippy checks
	@CARGO_TARGET_DIR="$(CARGO_TARGET_DIR)" $(CARGO) clippy --all-features --all-targets --locked -- -D warnings

test: ## Run Rust tests with all features
	@CARGO_TARGET_DIR="$(CARGO_TARGET_DIR)" $(CARGO) test --all-features --locked

test-make: ## Test build profiles and staged installs (no Rust build needed)
	@python3 -m unittest discover -s tests/build -p 'test_*.py'

check: fmt test lint test-make ## Run formatting, tests, lint and Makefile tests

clean: ## Delete Cargo build artifacts and installation stamp
	@CARGO_TARGET_DIR="$(CARGO_TARGET_DIR)" $(CARGO) clean

##@ Install
install: ## Install last Make-built release + desktop session (no root Cargo build)
	@set -eu; \
	if [ ! -f "$(BUILD_STAMP)" ] || [ ! -f "$(BUILD_SUMS)" ]; then \
		printf '%s\n' 'No verified release build found. Run make build (or make build-no-bar / build-minimal) as your normal user first.' >&2; exit 1; \
	fi; \
	if ! sha256sum -c --status "$(BUILD_SUMS)"; then \
		printf '%s\n' 'Release binaries have changed since the Make build. Re-run your chosen make build target.' >&2; exit 1; \
	fi; \
	variant=$$(cat "$(BUILD_STAMP)"); \
	case "$$variant" in default|no-bar|minimal|xwayland|all) ;; *) printf '%s\n' 'Unknown build variant; rebuild with make.' >&2; exit 1 ;; esac; \
	$(INSTALL) -d "$(BIN_DIR)" "$(DESKTOP_DIR)"; \
	$(INSTALL) -m 755 "$(RELEASE_DIR)/anvil" "$(BIN_DIR)/anvil.new"; \
	mv -f "$(BIN_DIR)/anvil.new" "$(BIN_DIR)/anvil"; \
	if [ "$$variant" != minimal ]; then \
		$(INSTALL) -m 755 "$(RELEASE_DIR)/anvilctl" "$(BIN_DIR)/anvilctl.new"; \
		mv -f "$(BIN_DIR)/anvilctl.new" "$(BIN_DIR)/anvilctl"; \
	fi; \
	$(INSTALL) -m 644 anvil.desktop "$(DESKTOP_DIR)/anvil.desktop"; \
	sed -i 's|^Exec=.*|Exec=$(PREFIX)/bin/anvil|' "$(DESKTOP_DIR)/anvil.desktop"; \
	printf 'Installed Anvil (%s) to %s and session to %s\n' "$$variant" "$(BIN_DIR)" "$(DESKTOP_DIR)"; \
	if [ "$$variant" = minimal ]; then \
		printf '%s\n' 'Minimal build: anvilctl was not installed (any previously installed anvilctl is left untouched).'; \
	fi

uninstall: ## Remove Anvil binaries and desktop session from selected paths
	@rm -f "$(BIN_DIR)/anvil" "$(BIN_DIR)/anvilctl" "$(DESKTOP_DIR)/anvil.desktop"
	@printf 'Removed Anvil files from %s and %s\n' "$(BIN_DIR)" "$(DESKTOP_DIR)"
