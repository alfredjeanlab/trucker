#!/bin/bash
# The Linux path, run inside a local VM whose root is ext4: `limactl shell <vm> bash tests/linux.sh`.
#
# Worktrees and the store live on /work, an XFS filesystem in a file on the root disk, where files clone. The unit
# tests run there, then two copies of one workspace, each its own git worktree, are built with a shared store. At the
# end the store is put on another filesystem, which must say so once, then on ext4 with two workspaces, which cannot
# clone and so must store by hard link, restore by copy and carry a session by hard link.
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
rm -rf /work/tmp /work/e2e /work/cache /work/concurrent "$HOME"/e2e-ext4* "$HOME/cache-ext4"
mkdir -p /work/tmp /work/e2e

export CARGO_TARGET_DIR=/work/trucker-target
(cd "$here" && TMPDIR=/work/tmp cargo test --release --quiet 2> /dev/null)
(cd "$here" && cargo build --release --quiet)
wrapper=$CARGO_TARGET_DIR/release/trucker
unset CARGO_TARGET_DIR

workspace() {
    mkdir -p "$1/src" "$1/.git"
    printf '[package]\nname = "e2e"\nversion = "0.1.0"\nedition = "2021"\n[dependencies]\n' > "$1/Cargo.toml"
    printf 'serde = { version = "1", features = ["derive"] }\nanyhow = "1"\nlibc = "0.2"\n' >> "$1/Cargo.toml"
    printf 'pub fn pid() -> i32 {\n    unsafe { libc::getpid() }\n}\n' > "$1/src/lib.rs"
    printf '#[derive(serde::Serialize)]\nstruct Pid(i32);\n' > "$1/src/main.rs"
    printf 'fn main() -> anyhow::Result<()> {\n    let _ = Pid(e2e::pid());\n    Ok(())\n}\n' >> "$1/src/main.rs"
}
build() { (cd "$1" && XDG_CACHE_HOME=$2 RUSTC_WRAPPER=$wrapper cargo build 2>&1); }
# A compile that a build script starts as a probe, such as anyhow's, is logged as `-` and runs only where the script
# runs, so it is left out of the counts.
outcomes() { awk '$2 != "-"' "$1/trucker/log" | cut -d' ' -f4- | grep -c "^$2" || true; }
scripts() { cut -d' ' -f2,4- "$1/trucker/log" | grep -c "/build-script $2" || true; }
expect() { [ "$2" = "$3" ] || { echo "FAILED: $1 is $2, expected $3"; exit 1; }; }

workspace /work/e2e/first
build /work/e2e/first /work/cache > /dev/null
cp /work/e2e/first/Cargo.lock /work/e2e/second.lock
misses=$(outcomes /work/cache miss)
runs=$(scripts /work/cache miss)
expect "hits in the first build" "$(outcomes /work/cache hit)" 0
[ "$runs" -gt 0 ] || { echo "FAILED: no build-script run was stored"; exit 1; }

workspace /work/e2e/second
mv /work/e2e/second.lock /work/e2e/second/Cargo.lock
build /work/e2e/second /work/cache > /dev/null
expect "misses after the second build" "$(outcomes /work/cache miss)" "$misses"
expect "hits in the second build" "$(outcomes /work/cache hit)" "$misses"
expect "build-script runs replayed in the second build" "$(scripts /work/cache hit)" "$runs"
expect "build-script runs uncached" "$(scripts /work/cache passthrough)" 0
[ "$(grep -cE ' e2e [0-9]* hit( promoted)?$' /work/cache/trucker/log)" -gt 0 ] || { echo "FAILED: the own crate missed"; exit 1; }
expect "crates compiled by a repeated build" "$(build /work/e2e/second /work/cache | grep -c Compiling || true)" 0
/work/e2e/second/target/debug/e2e
inode() { stat -c %i "$(ls "$1"/target/debug/build/libc/*/out/liblibc-*.rlib)"; }
[ "$(inode /work/e2e/first)" != "$(inode /work/e2e/second)" ] || { echo "FAILED: the outputs share an inode"; exit 1; }

# Two builds at once against an empty store compile each key once.
for name in fourth fifth; do workspace /work/e2e/$name && cp /work/e2e/first/Cargo.lock /work/e2e/$name/; done
build /work/e2e/fourth /work/concurrent > /dev/null & build /work/e2e/fifth /work/concurrent > /dev/null
wait
expect "misses of two concurrent builds" "$(outcomes /work/concurrent miss)" "$misses"

# gc keeps what the builds used. Entries unused for four days go, and the next build compiles them again. A miss
# started a gc in the background, which may still hold the lock.
collect() {
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        out=$(XDG_CACHE_HOME=/work/cache "$wrapper" gc)
        [[ $out == removed* ]] && break
        sleep 1
    done
    echo "$out" | cut -d' ' -f2
}
expect "entries removed by gc after two builds" "$(collect)" 0
find /work/cache/trucker/entries /work/cache/trucker/scripts -mindepth 1 -maxdepth 1 -exec touch -h -d '4 days ago' {} +
removed=$(collect)
[ "$removed" -gt 0 ] || { echo "FAILED: gc removed no entry unused for four days"; exit 1; }
workspace /work/e2e/sixth
cp /work/e2e/first/Cargo.lock /work/e2e/sixth/
build /work/e2e/sixth /work/cache > /dev/null
expect "misses after gc" "$(outcomes /work/cache miss)" "$((2 * misses))"
/work/e2e/sixth/target/debug/e2e

# The first edit to the own crate in a fresh workspace starts from the session that another workspace left in a slot.
reused() { grep -c ' e2e [0-9]* miss session$' /work/cache/trucker/log || true; }
workspace /work/e2e/seventh
cp /work/e2e/first/Cargo.lock /work/e2e/seventh/
build /work/e2e/seventh /work/cache > /dev/null
before=$(reused)
echo 'pub fn edited() {}' >> /work/e2e/seventh/src/lib.rs
# A compile that starts within a second of an edit is logged as `passthrough changed-source` in place of its outcome.
sleep 1.1
build /work/e2e/seventh /work/cache > /dev/null
[ "$(reused)" -gt "$before" ] || { echo "FAILED: the edit did not start from a stored session"; exit 1; }
/work/e2e/seventh/target/debug/e2e

# The store on ext4 while the target is on XFS, then both on ext4, which cannot clone.
workspace /work/e2e/third
cp /work/e2e/first/Cargo.lock /work/e2e/third/
said=$(build /work/e2e/third "$HOME/cache-ext4" | grep -c "different filesystems" || true)
expect "notices of the other filesystem" "$said" 1
expect "misses with the store on another filesystem" "$(outcomes "$HOME/cache-ext4" miss)" 0
rm -rf "$HOME/cache-ext4"
for name in e2e-ext4 e2e-ext4-second; do
    workspace "$HOME/$name"
    cp /work/e2e/first/Cargo.lock "$HOME/$name/"
    expect "notices on ext4" "$(build "$HOME/$name" "$HOME/cache-ext4" | grep -c trucker || true)" 0
    "$HOME/$name/target/debug/e2e"
done
expect "misses on ext4" "$(outcomes "$HOME/cache-ext4" miss)" "$misses"
expect "hits on ext4" "$(outcomes "$HOME/cache-ext4" hit)" "$misses"
echo 'pub fn edited() {}' >> "$HOME/e2e-ext4-second/src/lib.rs" && sleep 1.1
build "$HOME/e2e-ext4-second" "$HOME/cache-ext4" > /dev/null
[ "$(grep -c ' e2e [0-9]* miss session$' "$HOME/cache-ext4/trucker/log")" -gt 0 ] || { echo "FAILED: no session on ext4"; exit 1; }
"$HOME/e2e-ext4-second/target/debug/e2e"
expect "crates compiled by a repeated build on ext4" "$(build "$HOME/e2e-ext4" "$HOME/cache-ext4" | grep -c Compiling || true)" 0
# strip rewrites a binary with several links in place, and so the stored one, which the next restore must refuse.
strip "$HOME/e2e-ext4/target/debug/e2e"
workspace "$HOME/e2e-ext4-third" && cp /work/e2e/first/Cargo.lock "$HOME/e2e-ext4-third/" && sleep 1.1
build "$HOME/e2e-ext4-third" "$HOME/cache-ext4" > /dev/null
file "$HOME/e2e-ext4-third/target/debug/e2e" | grep -q 'not stripped' || { echo "FAILED: a stripped binary was restored"; exit 1; }
echo "linux: passed, with $misses entries stored and restored, $runs of them build-script runs"
