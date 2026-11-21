#!/bin/bash
# The Windows path, run in Git Bash on a GitHub runner: `bash tests/windows.sh`.
#
# Two copies of one workspace, each with a .git directory, are built with a shared store on the volume of the runner's
# temporary directory. The second restores each third-party crate that the first stored, by a clone on a Dev Drive and
# otherwise by a copy that is checked against its hash. An own crate runs as plain rustc would.
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
work=$(cygpath -u "${RUNNER_TEMP:-$TEMP}")/trucker-e2e
rm -rf "$work"
mkdir -p "$work"

(cd "$here" && cargo build --release --quiet)
wrapper=$(cygpath -w "$here/target/release/trucker.exe")
cache=$(cygpath -w "$work/cache")
log=$work/cache/trucker/log

workspace() {
    mkdir -p "$1/src" "$1/.git"
    printf '[package]\nname = "e2e"\nversion = "0.1.0"\nedition = "2021"\n[dependencies]\n' > "$1/Cargo.toml"
    printf 'serde = { version = "1", features = ["derive"] }\nanyhow = "1"\n' >> "$1/Cargo.toml"
    printf 'pub fn pid() -> u32 {\n    std::process::id()\n}\n' > "$1/src/lib.rs"
    printf '#[derive(serde::Serialize)]\nstruct Pid(u32);\n' > "$1/src/main.rs"
    printf 'fn main() -> anyhow::Result<()> {\n    let _ = Pid(e2e::pid());\n    Ok(())\n}\n' >> "$1/src/main.rs"
}
# A build is incremental, as on a developer's machine, even where CI turns CARGO_INCREMENTAL off.
build() { (cd "$1" && XDG_CACHE_HOME=$cache RUSTC_WRAPPER=$wrapper CARGO_INCREMENTAL=1 cargo build 2>&1); }
# A compile that a build script starts as a probe, such as anyhow's, is logged as `-` and runs only where the script
# runs, so it is left out of the counts.
outcomes() { awk '$2 != "-"' "$log" | cut -d' ' -f4- | tr -d '\r' | grep -c "^$1" || true; }
expect() { [ "$2" = "$3" ] || { echo "FAILED: $1 is $2, expected $3"; cat "$log"; exit 1; }; }

workspace "$work/first"
build "$work/first" > /dev/null
cp "$work/first/Cargo.lock" "$work/second.lock"
misses=$(outcomes miss)
[ "$misses" -gt 0 ] || { echo "FAILED: the first build stored nothing"; cat "$log"; exit 1; }
expect "hits in the first build" "$(outcomes hit)" 0
expect "own crates in the first build" "$(outcomes 'passthrough windows-own-crate')" 2
expect "other passthroughs in the first build" "$(outcomes passthrough)" 2

workspace "$work/second"
mv "$work/second.lock" "$work/second/Cargo.lock"
build "$work/second" > /dev/null
expect "misses after the second build" "$(outcomes miss)" "$misses"
expect "hits in the second build" "$(outcomes hit)" "$misses"
expect "passthroughs after the second build" "$(outcomes passthrough)" 4
expect "crates compiled by a repeated build" "$(build "$work/second" | grep -c Compiling || true)" 0
"$work/second/target/debug/e2e.exe"

# A miss started a gc in the background, which may still hold the lock.
collect() {
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        out=$(XDG_CACHE_HOME=$cache "$wrapper" gc)
        [[ $out == removed* ]] && break
        sleep 1
    done
    echo "$out" | cut -d' ' -f2
}
expect "entries removed by gc after two builds" "$(collect)" 0
echo "windows: passed, with $misses entries stored and restored"
