# ai-classification-milter build recipes (just 1.x)
dist := "dist"
bin := "ai-classification-milter"
macos_target := "aarch64-apple-darwin"
linux_target := "x86_64-unknown-linux-musl"

default:
    @just --list

# native debug build
build:
    cargo build

# native release build
build-release:
    cargo build --release

# all unit tests
test:
    cargo test

# pre-push gate: format check + clippy (deny warnings) + tests
check:
    cargo fmt --all -- --check
    cargo clippy --all-targets -- -D warnings
    cargo test

# coverage report (needs cargo-llvm-cov: cargo install cargo-llvm-cov)
cov:
    cargo llvm-cov --summary-only

# format the code
fmt:
    cargo fmt --all

# install as systemd service (root): deploy/install.sh
install: build-linux-amd64
    sudo deploy/install.sh

# install + start + smoke-check
deploy: build-linux-amd64
    sudo deploy/install.sh --start

# one-time setup for the Linux cross build
setup:
    rustup target add {{macos_target}} {{linux_target}}
    cargo install cargo-zigbuild --locked

# macOS/arm64 release binary -> dist/ai-classification-milter-macos-arm64
build-macos-arm:
    mkdir -p {{dist}}
    cargo build --release --target {{macos_target}}
    cp target/{{macos_target}}/release/{{bin}} {{dist}}/{{bin}}-macos-arm64
    @echo "-> {{dist}}/{{bin}}-macos-arm64"

# Linux/amd64 static release binary -> dist/ai-classification-milter-linux-amd64 (via zig)
build-linux-amd64:
    mkdir -p {{dist}}
    command -v cargo-zigbuild >/dev/null || cargo install cargo-zigbuild --locked
    cargo zigbuild --release --target {{linux_target}}
    cp target/{{linux_target}}/release/{{bin}} {{dist}}/{{bin}}-linux-amd64
    @echo "-> {{dist}}/{{bin}}-linux-amd64"

# both release binaries
build-all: build-macos-arm build-linux-amd64
    @ls -la {{dist}}/

clean:
    cargo clean
    rm -rf {{dist}}
