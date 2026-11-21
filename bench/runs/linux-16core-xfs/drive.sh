#!/usr/bin/env bash
# Runs the named stages one after another, each after a preflight, from a checkout of trucker in ~/trucker-repo:
#
#   drive.sh throughput sentinels ripgrep cargo-vendored cargo rust zed projects projects-others
#
# The preflight waits until the load is below 1 and no cargo, rustc or cache runs, removes what an earlier stage left in
# the filesystem under test, drops the page cache and checks the free disk.
set -uo pipefail
export PATH=$HOME/.cargo/bin:$PATH
root=$HOME/trucker-bench
out=$HOME/out
log=$HOME/benchlogs
mkdir -p "$out" "$log"
cd "$HOME/trucker-repo" || exit 1
all=$PWD/bench/runs/linux-16core-xfs/projects-71d65a6/projects-all.txt
export MBX_OVERRIDE_WRAPPER=1
# A line of the ledger that holds the cores between the invocations of the stages, which they do not wait for.
export BENCH_HOLD=trucker-drive-$$
hold() {
  local rest
  rest=$(grep -v "^$BENCH_HOLD	" "$HOME/ledger" || true)
  printf '%s' "${rest:+$rest$'\n'}" > "$HOME/ledger"
  [ "${1:-}" = release ] && return
  printf '%s\t%s\t%s\n' "$BENCH_HOLD" 15 "$(( $(date +%s) + 4 * 3600 ))" >> "$HOME/ledger"
}
trap 'hold release' EXIT

preflight() {
  local stage=$1 need=${2:-150} waited=0 clean=0
  {
    echo "== preflight $stage"
    for d in cache config work throughput sentinel sent; do
      if [ -e "$root/$d" ]; then
        echo "removing leftover $root/$d ($(du -sh "$root/$d" 2> /dev/null | cut -f1))"
        chmod -R u+w "$root/$d" 2> /dev/null
        rm -rf "${root:?}/$d"
      fi
    done
    [ -e "$HOME/.cache/trucker" ] && echo "note: ~/.cache/trucker exists ($(du -sh "$HOME/.cache/trucker" | cut -f1))"
    git -C "$root/src" 2> /dev/null true
    for s in "$root"/src/*; do
      local n
      n=$(git -C "$s" worktree list 2> /dev/null | wc -l)
      [ "$n" -gt 1 ] && { echo "pruning $((n - 1)) worktrees of $s"; git -C "$s" worktree prune; }
    done
    while :; do
      local load procs
      load=$(cut -d' ' -f1 /proc/loadavg)
      procs=$(pgrep -a -f 'cargo|rustc|trucker|sccache|mbx|kache' | grep -v -e pgrep -e 'bash -c' || true)
      if [ -z "$procs" ] && awk -v l="$load" 'BEGIN {exit !(l < 1)}'; then
        clean=$((clean + 1))
        [ "$clean" -ge 2 ] && break
      else
        clean=0
        [ $((waited % 300)) -eq 0 ] && echo "waiting: load $load, processes: ${procs:-none}"
      fi
      sleep 10
      waited=$((waited + 10))
    done
    echo "waited ${waited}s; load $(cut -d' ' -f1-3 /proc/loadavg); processes: none"
    sync
    if sudo -n true 2> /dev/null; then echo 3 | sudo -n tee /proc/sys/vm/drop_caches > /dev/null && echo "dropped caches"; else echo "no passwordless sudo"; fi
    local free
    free=$(df -BG --output=avail "$root" | tail -1 | tr -dc 0-9)
    echo "free: ${free} GB on $root; / $(df -h --output=avail / | tail -1)"
    [ "$free" -ge "$need" ] || { echo "not enough disk"; return 1; }
    uptime
  } >> "$log/preflight.log" 2>&1
}

stage() {
  local name=$1
  shift
  hold
  preflight "$name" || { echo "preflight failed for $name" >> "$log/drive.log"; exit 1; }
  echo "== start $name" >> "$log/drive.log"
  "$@" >> "$log/$name.log" 2>&1
  echo "== end $name exit $?" >> "$log/drive.log"
}

tp() {
  RUNS=2 TOOLS="none shared trucker" bench/throughput "$root" "$root" "$out/throughput" cargo-vendored
  RUNS=1 TOOLS="none shared trucker" bench/throughput "$root" "$root" "$out/throughput" zed
}
sentinels() {
  bench/sentinel "$root" "$root/sent" "$out/sentinels" sqlx-migrate sqlx rustls rocksdb jemalloc openssl wasmtime \
    prost embed vergen bevy pyo3 workflows wasm tests cxx cmake
  local status=$?
  if [ "$status" -ne 0 ] || grep -q '"verdict":"fail"' "$out/sentinels/sentinels.jsonl"; then
    echo "SENTINEL FAILURE" >> "$log/drive.log"
    exit 2
  fi
}
# One project of the campaign: the other tools in one invocation, plain cargo and trucker in another.
campaign() {
  local p=$1 order=$2 others=$3 pair=$4
  export RUNS=3 COLD_RUNS=3
  local check=()
  [ "$p" = zed ] && check=(CHECK='./target/debug/zed --version; ./target/debug/zed --help')
  if [ "$order" = pair-first ]; then
    env "${check[@]}" SCENARIOS="1a 1d seq test" TOOLS="$pair" bench/run "$root" "$root" "$out/campaign" "$p"
    preflight "campaign-$p-others" && env "${check[@]}" SCENARIOS="1a seq" TOOLS="$others" bench/run "$root" "$root" "$out/campaign" "$p"
  else
    env "${check[@]}" SCENARIOS="1a seq" TOOLS="$others" bench/run "$root" "$root" "$out/campaign" "$p"
    preflight "campaign-$p-pair" && env "${check[@]}" SCENARIOS="1a 1d seq test" TOOLS="$pair" bench/run "$root" "$root" "$out/campaign" "$p"
  fi
}
projects() {
  local first=$1
  export PROJECTS=$all SCENARIOS="1a seq"
  local i=0 pair
  for p in prost vergen pyo3 embed rustls bevy openssl jemalloc cxx cmake sqlx wasm rocksdb wasmtime; do
    if [ $((i % 2)) -eq 0 ]; then pair="none $first"; else pair="$first none"; fi
    i=$((i + 1))
    local runs=3 env=()
    case $p in rocksdb | wasmtime) runs=1 ;; esac
    [ "$p" = sqlx ] && env=(SQLX_OFFLINE=true)
    [ "$p" = wasm ] && env=(CARGO_BUILD_TARGET=wasm32-unknown-unknown)
    preflight "projects-$p" 50 || return 1
    env "${env[@]}" RUNS=$runs COLD_RUNS=$runs TOOLS="$pair" bench/run "$root" "$root" "$out/projects" "$p"
  done
}
projects_others() {
  export PROJECTS=$all SCENARIOS="1a seq"
  local i=0 tools
  for p in prost vergen pyo3 embed rustls bevy openssl jemalloc sqlx wasm rocksdb wasmtime; do
    if [ $((i % 2)) -eq 0 ]; then tools="sccache mbx kache shared"; else tools="shared kache mbx sccache"; fi
    i=$((i + 1))
    local runs=3 env=()
    case $p in rocksdb | wasmtime) runs=1 ;; esac
    [ "$p" = sqlx ] && env=(SQLX_OFFLINE=true)
    [ "$p" = wasm ] && env=(CARGO_BUILD_TARGET=wasm32-unknown-unknown)
    preflight "projects-others-$p" 50 || return 1
    env "${env[@]}" RUNS=$runs COLD_RUNS=$runs TOOLS="$tools" bench/run "$root" "$root" "$out/projects" "$p"
  done
}

for s in "$@"; do
  case $s in
    throughput) stage throughput tp ;;
    sentinels) stage sentinels sentinels ;;
    ripgrep) stage campaign-ripgrep campaign ripgrep pair-first "sccache mbx kache shared" "none trucker" ;;
    cargo-vendored) stage campaign-cargo-vendored campaign cargo-vendored others-first "shared kache mbx sccache" "trucker none" ;;
    cargo) stage campaign-cargo campaign cargo pair-first "shared kache mbx sccache" "none trucker" ;;
    rust) stage campaign-rust campaign rust others-first "sccache mbx kache shared" "trucker none" ;;
    zed) stage campaign-zed campaign zed pair-first "shared kache mbx sccache" "none trucker" ;;
    projects) stage projects projects trucker ;;
    projects-others) stage projects-others projects_others ;;
  esac
done
echo "== all done" >> "$log/drive.log"
