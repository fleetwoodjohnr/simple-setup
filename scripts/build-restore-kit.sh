#!/bin/sh
# Builds static (musl) Linux restore programs, and Windows ones when cargo-xwin is available,
# into target/release/restore-kit/<os>-<arch>/, where the CLI and the desktop app look for
# engines to copy into snapshots. Static binaries run on any Linux distribution, old or new,
# without installing anything. Targets whose Rust standard library is not installed are
# skipped (with a note); RESTORE_KIT_STRICT=1 makes that an error.
#   rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
#     i686-unknown-linux-musl armv7-unknown-linux-musleabihf riscv64gc-unknown-linux-musl
set -eu
cd "$(dirname "$0")/.."
kit=target/release/restore-kit
installed=$(rustup target list --installed 2>/dev/null || true)
skip() {
  echo "skipped $1: rustup target add $1" >&2
  [ "${RESTORE_KIT_STRICT:-}" != 1 ] || exit 1
}
# <rust target> <Simple Setup architecture name>
for pair in \
  x86_64-unknown-linux-musl:x86_64 \
  aarch64-unknown-linux-musl:aarch64 \
  i686-unknown-linux-musl:i686 \
  armv7-unknown-linux-musleabihf:armv7 \
  riscv64gc-unknown-linux-musl:riscv64; do
  target=${pair%%:*} arch=${pair#*:}
  echo "$installed" | grep -qx "$target" || { skip "$target"; continue; }
  # rust-lld links the static binary, so no cross C toolchain is needed. riscv64 musl links
  # dynamically unless asked for a static binary.
  var=$(echo "$target" | tr 'a-z-' 'A-Z_')
  flags=; [ "$arch" = riscv64 ] && flags="-C target-feature=+crt-static -C link-self-contained=yes"
  env "CARGO_TARGET_${var}_LINKER=rust-lld" "CARGO_TARGET_${var}_RUSTFLAGS=$flags" \
    cargo build --release -p simple-setup-cli --target "$target"
  mkdir -p "$kit/linux-$arch"
  cp "target/$target/release/simple-setup" "$kit/linux-$arch/simple-setup"
  echo "$kit/linux-$arch/simple-setup"
done
# Windows restore programs, when cargo-xwin is installed (cargo install cargo-xwin).
if cargo xwin --version >/dev/null 2>&1; then
  for pair in x86_64-pc-windows-msvc:x86_64 aarch64-pc-windows-msvc:aarch64 i686-pc-windows-msvc:i686; do
    target=${pair%%:*} arch=${pair#*:}
    echo "$installed" | grep -qx "$target" || { skip "$target"; continue; }
    cargo xwin build --release -p simple-setup-cli --target "$target"
    mkdir -p "$kit/windows-$arch"
    cp "target/$target/release/simple-setup.exe" "$kit/windows-$arch/simple-setup.exe"
    echo "$kit/windows-$arch/simple-setup.exe"
  done
fi
