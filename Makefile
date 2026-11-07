BIN := gz
PREFIX := $(HOME)/.local/bin

.PHONY: build release install-local check fmt test clean

build:
	cargo build

release:
	cargo build --release

install-local: release
	mkdir -p $(PREFIX)
	@# Replace the file rather than overwrite it: macOS kills a binary whose
	@# code-signature cache no longer matches its contents.
	rm -f $(PREFIX)/$(BIN)
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
