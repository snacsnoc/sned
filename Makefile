# sned - Build System
# Usage: make, make release, make full, make test, make check, make clean

.PHONY: all release full test check clean

# All language packs + terminal
ALL_FEATURES := terminal,lang-rust,lang-javascript,lang-python,lang-typescript,lang-go,lang-c,lang-cpp,lang-c-sharp,lang-ruby,lang-java,lang-php,lang-swift
JOBS         := $(shell nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)

# Set PATH for Zig if available (used by terminal feature)
export PATH := $(shell if [ -x /opt/homebrew/opt/zig@0.15/bin/zig ]; then echo /opt/homebrew/opt/zig@0.15/bin:$$PATH; else echo $$PATH; fi)

all:
	cargo build -j$(JOBS)

release:
	cargo build --release -j$(JOBS)

full:
	cargo build --features "$(ALL_FEATURES)" -j$(JOBS)

test:
	cargo test --features "$(ALL_FEATURES)" -j$(JOBS)

check:
	cargo check --features "$(ALL_FEATURES)" -j$(JOBS)

clean:
	cargo clean
