#!/bin/bash
# The Linux path, run inside a local VM whose root is ext4: `limactl shell <vm> bash tests/linux.sh`.
#
# Worktrees and the store live on /work, an XFS filesystem in a file on the root disk, where files clone. The unit
# tests run there, then two copies of one workspace are built with a shared store, and at the end the store is put
# where it cannot clone, which must leave the build working and say so once.
set -euo pipefail
source "$HOME/.cargo/env"
here=$(cd "$(dirname "$0")/.." && pwd)

if ! findmnt -n -t xfs /work > /dev/null; then
    sudo truncate -s 12G /var/lib/work.img
    sudo mkfs.xfs -q /var/lib/work.img
    sudo mkdir -p /work
    sudo mount -o loop /var/lib/work.img /work
    sudo chown "$(id -u):$(id -g)" /work
fi
rm -rf /work/tmp /work/e2e /work/cache "$HOME/e2e-ext4" "$HOME/cache-ext4"
mkdir -p /work/tmp /work/e2e

export CARGO_TARGET_DIR=/work/crate-cache-target
(cd "$here" && TMPDIR=/work/tmp cargo test --release --quiet 2> /dev/null)
(cd "$here" && cargo build --release --quiet)
wrapper=$CARGO_TARGET_DIR/release/crate-cache
unset CARGO_TARGET_DIR

workspace() {
    mkdir -p "$1/src"
    printf '[package]\nname = "e2e"\nversion = "0.1.0"\nedition = "2021"\n[dependencies]\n' > "$1/Cargo.toml"
    printf 'serde = { version = "1", features = ["derive"] }\nanyhow = "1"\nlibc = "0.2"\n' >> "$1/Cargo.toml"
    printf '#[derive(serde::Serialize)]\nstruct Pid(i32);\n' > "$1/src/main.rs"
    printf 'fn main() -> anyhow::Result<()> {\n    let _ = Pid(unsafe { libc::getpid() });\n    Ok(())\n}\n' >> "$1/src/main.rs"
}
build() { (cd "$1" && XDG_CACHE_HOME=$2 RUSTC_WRAPPER=$wrapper cargo build 2>&1); }
outcomes() { cut -d' ' -f4- "$1/crate-cache/log" | grep -c "^$2" || true; }
expect() { [ "$2" = "$3" ] || { echo "FAILED: $1 is $2, expected $3"; exit 1; }; }

workspace /work/e2e/first
build /work/e2e/first /work/cache > /dev/null
cp /work/e2e/first/Cargo.lock /work/e2e/second.lock
misses=$(outcomes /work/cache miss)
expect "hits in the first build" "$(outcomes /work/cache hit)" 0

workspace /work/e2e/second
mv /work/e2e/second.lock /work/e2e/second/Cargo.lock
build /work/e2e/second /work/cache > /dev/null
expect "misses after the second build" "$(outcomes /work/cache miss)" "$misses"
expect "hits in the second build" "$(outcomes /work/cache hit)" "$misses"
expect "crates compiled by a repeated build" "$(build /work/e2e/second /work/cache | grep -c Compiling || true)" 0
/work/e2e/second/target/debug/e2e
inode() { stat -c %i "$(ls "$1"/target/debug/build/libc/*/out/liblibc-*.rlib)"; }
[ "$(inode /work/e2e/first)" != "$(inode /work/e2e/second)" ] || { echo "FAILED: the outputs share an inode"; exit 1; }

# The store on ext4 while the target is on XFS, then both on ext4, which cannot clone.
workspace /work/e2e/third
cp /work/e2e/first/Cargo.lock /work/e2e/third/
said=$(build /work/e2e/third "$HOME/cache-ext4" | grep -c "different filesystems" || true)
expect "notices of the other filesystem" "$said" 1
expect "misses with the store on another filesystem" "$(outcomes "$HOME/cache-ext4" miss)" 0
workspace "$HOME/e2e-ext4"
cp /work/e2e/first/Cargo.lock "$HOME/e2e-ext4/"
said=$(build "$HOME/e2e-ext4" "$HOME/cache-ext4" | grep -c "cannot clone" || true)
expect "notices of a filesystem that cannot clone" "$said" 1
"$HOME/e2e-ext4/target/debug/e2e"
echo "linux: passed, with $misses crates stored and restored"
