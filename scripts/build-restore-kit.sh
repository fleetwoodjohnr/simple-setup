#!/bin/sh
# Builds static (musl) Linux restore programs, and Windows ones when cargo-xwin is available,
# into target/release/restore-kit/<os>-<arch>/, where the CLI and the desktop app look for
# engines to copy into snapshots. Static binaries run on any Linux distribution, old or new,
# without installing anything.
#   rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
set -eu
cd "$(dirname "$0")/.."
kit=target/release/restore-kit
for arch in x86_64 aarch64; do
  target="$arch-unknown-linux-musl"
  # rust-lld links the static binary, so no cross C toolchain is needed.
  env "CARGO_TARGET_$(echo "$target" | tr 'a-z-' 'A-Z_')_LINKER=rust-lld" \
    cargo build --release -p simple-setup-cli --target "$target"
  mkdir -p "$kit/linux-$arch"
  cp "target/$target/release/simple-setup" "$kit/linux-$arch/simple-setup"
  echo "$kit/linux-$arch/simple-setup"
done
# Windows restore programs, when cargo-xwin is installed (cargo install cargo-xwin;
# rustup target add x86_64-pc-windows-msvc).
if cargo xwin --version >/dev/null 2>&1; then
  for arch in x86_64 aarch64; do
    target="$arch-pc-windows-msvc"
    rustup target list --installed | grep -qx "$target" || continue
    cargo xwin build --release -p simple-setup-cli --target "$target"
    mkdir -p "$kit/windows-$arch"
    cp "target/$target/release/simple-setup.exe" "$kit/windows-$arch/simple-setup.exe"
    echo "$kit/windows-$arch/simple-setup.exe"
  done
fi
