# nirisaver
#
#   make            release build
#   make check      tests, clippy with no warnings, rustfmt
#   make benchmark  ms/frame, allocations, checksum — and the oracle comparison
#   make run        build and launch
#   make install    install to $(PREFIX)/bin

PREFIX           ?= /usr/local
CARGO            ?= cargo
CARGO_TARGET_DIR ?= build/rust
RUNFLAGS         ?=
BIN              := $(CARGO_TARGET_DIR)/release/nirisaver

export CARGO_TARGET_DIR

.PHONY: all benchmark check clean run install

all:
	$(CARGO) build --release
	@echo $(BIN)

# The benchmark verifies the incremental path against a full-frame oracle on
# every frame, so a failure here is a correctness failure, not a slow one.
benchmark:
	$(CARGO) bench --bench render

check:
	$(CARGO) test --all-targets
	$(CARGO) clippy --all-targets -- -D warnings
	$(CARGO) fmt --all --check

clean:
	$(CARGO) clean

run: all
	$(BIN) $(RUNFLAGS)

install: all
	install -Dm755 $(BIN) $(DESTDIR)$(PREFIX)/bin/nirisaver
