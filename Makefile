# Convenience targets for the render-parity test corpus. The data-driven render tests
# (crates/rpt-render/tests/postgres_fixtures.rs) seed committed SQL fixtures into a PostgreSQL server
# and diff the rendered Page IR against committed baselines. PostgreSQL is the single DB technology for
# render testing (see docker-compose.yml). The database is provisioned by docker compose.
#
# The build/run targets below are for the binaries themselves. `make help` lists everything.

RPT_DB_PORT ?= 55432
RPT_DB_URL  ?= postgres://rpt:rpt@localhost:$(RPT_DB_PORT)/rptfixtures
export RPT_DB_URL

# The folder `make run` serves, and the port it binds on 127.0.0.1.
REPORTS_DIR ?= ./reports
PORT        ?= 8080

# Cross-builds go under target/cross so .gitignore's /target/ already covers them, and so a cross
# build never invalidates the native one (a different toolchain would rebuild the whole tree).
CROSS_DIR   ?= $(CURDIR)/target/cross

# Cross-compiling needs rustup's toolchain; the native build is happy with whatever `cargo` resolves
# to. Override if rustup lives elsewhere:  make dist RUSTUP_BIN=$$HOME/.cargo/bin
RUSTUP_BIN  ?= /opt/homebrew/opt/rustup/bin

# A binary you hand to someone to run is stripped. The workspace release profile deliberately keeps
# line tables so a panic backtrace names functions and source lines — worth it for a binary you
# debug, dead weight for one you ship. On Windows that debug info is embedded in the .exe rather
# than split into a sidecar, which took it to 78 MB: past the attachment and upload limits of most
# mail, chat and file-share systems, so the file could not even be delivered. Stripped it is ~17 MB.
# Overridable — `make dist STRIP=0` keeps the debug info.
STRIP ?= 1
ifeq ($(STRIP),1)
STRIP_ENV = CARGO_PROFILE_RELEASE_DEBUG=false CARGO_PROFILE_RELEASE_STRIP=symbols
endif

CROSS_CARGO  = PATH="$(RUSTUP_BIN):$$PATH" CARGO_TARGET_DIR=$(CROSS_DIR) $(STRIP_ENV) cargo

# The Windows GNU target links with mingw; the musl target links with rustup's bundled rust-lld, so
# no C toolchain is needed for Linux.
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER      ?= x86_64-w64-mingw32-gcc
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER  ?= rust-lld
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS ?= -C target-feature=+crt-static

.PHONY: help db-up db-down test-fixtures bless-fixtures test-fixtures-clean \
        build build-debug run test lint fmt check \
        dist dist-macos-arm64 dist-macos-x86_64 dist-linux dist-linux-oracle dist-windows \
        cross-setup dist-list

## List the available targets.
help:
	@awk '/^## /{ desc = substr($$0, 4); next } \
	      /^[a-zA-Z0-9_-]+:/ { if (desc != "") { split($$0, t, ":"); \
	                                             printf "  \033[1m%-20s\033[0m %s\n", t[1], desc; \
	                                             desc = "" } next } \
	      { desc = "" }' $(MAKEFILE_LIST)

## Start the test PostgreSQL (blocks until healthy).
db-up:
	docker compose up -d --wait

## Stop and discard the test PostgreSQL (data is ephemeral).
db-down:
	docker compose down

## Run the render-parity corpus against the running PostgreSQL.
test-fixtures:
	cargo test -p rpt-render --test postgres_fixtures

## Regenerate the committed Page IR baselines from the current render.
bless-fixtures:
	RPT_BLESS=1 cargo test -p rpt-render --test postgres_fixtures

## One-shot: bring the DB up, run the corpus, tear it down.
test-fixtures-clean: db-up
	$(MAKE) test-fixtures; status=$$?; docker compose down; exit $$status

# --- Build and run -----------------------------------------------------------------------------

## Build every binary for this machine, optimised (target/release/).
build:
	cargo build --release --workspace

## Build every binary for this machine, unoptimised but fast to compile.
build-debug:
	cargo build --workspace

## Serve REPORTS_DIR with rpt-compat on 127.0.0.1:PORT (e.g. make run REPORTS_DIR=../reporter/sample).
run:
	cargo run --release -p rpt-compat -- --port $(PORT) --reports-dir $(REPORTS_DIR)

## Run the whole test suite.
test:
	cargo test --workspace

## Clippy, warnings denied — what CI runs.
lint:
	cargo clippy --workspace --all-targets --all-features -- -D warnings

## Rewrite sources to rustfmt's style.
fmt:
	cargo fmt --all

## The pre-push gate: formatting, lints and tests.
check:
	cargo fmt --all --check
	$(MAKE) lint
	$(MAKE) test

# --- Cross-built binaries ----------------------------------------------------------------------
#
# Hosted on macOS/arm64. The three cross targets need rustup (Homebrew's rust ships only the host
# std) and, for Windows, mingw-w64. `make cross-setup` installs both.

## Install the toolchains the cross targets need (rustup targets + mingw-w64).
cross-setup:
	@command -v $(RUSTUP_BIN)/rustup >/dev/null 2>&1 \
	  || { echo "rustup not found at $(RUSTUP_BIN) — brew install rustup"; exit 1; }
	@command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1 \
	  || { echo "mingw-w64 not found — brew install mingw-w64"; exit 1; }
	$(RUSTUP_BIN)/rustup target add \
	  x86_64-apple-darwin x86_64-unknown-linux-musl x86_64-pc-windows-gnu

## Build rpt-compat for macOS, Linux and Windows (into target/cross/).
dist: dist-macos-arm64 dist-macos-x86_64 dist-linux dist-windows dist-list

## rpt-compat for macOS on Apple silicon.
dist-macos-arm64:
	$(CROSS_CARGO) build --release -p rpt-compat --target aarch64-apple-darwin

## rpt-compat for macOS on Intel.
dist-macos-x86_64:
	$(CROSS_CARGO) build --release -p rpt-compat --target x86_64-apple-darwin

## rpt-compat for Linux x86_64, static musl, saved data only (no C toolchain needed).
dist-linux:
	$(CROSS_CARGO) build --release -p rpt-compat --no-default-features \
	  --target x86_64-unknown-linux-musl

## rpt-compat for Linux x86_64 WITH Oracle — needs a musl C cross-compiler (brew install musl-cross).
dist-linux-oracle:
	$(CROSS_CARGO) build --release -p rpt-compat --target x86_64-unknown-linux-musl

## rpt-compat for Windows x86_64 (mingw cross-build).
dist-windows:
	$(CROSS_CARGO) build --release -p rpt-compat --target x86_64-pc-windows-gnu

## Show what the cross builds produced.
dist-list:
	@find $(CROSS_DIR) -name 'rpt-compat' -o -name 'rpt-compat.exe' \
	  | grep release | sort | while read -r f; do \
	      printf '%10s  %s\n' "$$(du -h "$$f" | cut -f1)" "$$f"; \
	    done
