BIN := gz
PREFIX := $(HOME)/.local/bin

.PHONY: build release install-local check fmt test clean packaging

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

# Homebrew formula, Scoop and winget manifests for one published release,
# with its checksums: make packaging VERSION=0.10.0 [PACKAGING_OUT=dir]
PACKAGING_OUT ?= target/packaging
packaging:
	@test -n "$(VERSION)" || { echo "usage: make packaging VERSION=0.10.0"; exit 1; }
	packaging/render.sh $(VERSION) $(PACKAGING_OUT)
