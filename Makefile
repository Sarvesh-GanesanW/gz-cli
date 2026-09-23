BIN := gz
PREFIX := $(HOME)/.local/bin

.PHONY: build release install-local check fmt test clean

build:
	cargo build

release:
	cargo build --release

install-local: release
	mkdir -p $(PREFIX)
	cp target/release/$(BIN) $(PREFIX)/$(BIN)
	@echo "installed: $(PREFIX)/$(BIN)"

check:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

fmt:
	cargo fmt

test:
	cargo test

clean:
	cargo clean
