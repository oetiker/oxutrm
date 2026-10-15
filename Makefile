# Every rule caps parallelism at 4. The build machine is shared and this is not
# optional: an uncapped cargo will happily take every core on the box.
#
# Every rule also passes --workspace, and that is NOT decoration. The repo root
# is both a [package] and a [workspace], so a bare `cargo test` selects only the
# root binary and reports "ok" while running none of the six crates' tests. The
# gate would pass with every real test skipped. Do not remove it.
CARGO ?= cargo
JOBS  ?= 4

# `make man` builds man/oxutrm.1 from docs/manual.md.
MAN_NAME = oxutrm
include build/man.mk

.PHONY: all build test lint fmt fmt-check check clean demo

all: check

## Compile the workspace.
build:
	$(CARGO) build --workspace --jobs $(JOBS) --all-targets

## Run every test. Test threads are capped as well as build jobs.
test:
	$(CARGO) test --workspace --jobs $(JOBS) -- --test-threads $(JOBS)

## Clippy, with warnings as errors. This is the gate every task must pass.
lint:
	$(CARGO) clippy --workspace --all-targets --jobs $(JOBS) -- -D warnings

## Rewrite source in rustfmt's style.
fmt:
	$(CARGO) fmt --all

## Fail if anything is unformatted, for CI.
fmt-check:
	$(CARGO) fmt --all -- --check

## The full gate: formatting, lints, then tests.
check: fmt-check lint test

clean:
	$(CARGO) clean

# Where cargo puts the release binary, asked of cargo so CARGO_TARGET_DIR and
# build.target-dir are honoured.
TARGET_DIR ?= $(shell $(CARGO) metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')

## Record docs/demo/oxutrm.webp from demo/oxutrm.toml. Needs ansidrama
## (https://github.com/oetiker/ansidrama). Run by hand; CI does not.
demo:
	$(CARGO) build --release --bin oxutrm --jobs $(JOBS)
	demo/record.sh "$(TARGET_DIR)/release"
