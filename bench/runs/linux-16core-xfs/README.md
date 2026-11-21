# Linux, 16 cores, XFS

This is the campaign of the README, measured with trucker 71d65a6 after a fresh setup of the machine.

The stages ran from a checkout of trucker in `~/trucker-repo`. The run used `bench/install`, `bench/fetch` and a
`setup.sh` of its own, which `bench/setup` now replaces, so these commands repeat it:

    bench/setup ~/trucker-bench bench/runs/linux-16core-xfs/projects-71d65a6/locks
    bench/runs/linux-16core-xfs/drive.sh throughput sentinels ripgrep cargo-vendored cargo rust zed \
      projects projects-others

[drive.sh](drive.sh) holds the command of each stage, with `MBX_OVERRIDE_WRAPPER=1`, and runs a preflight before
each. `out/<stage>` became `<stage>-71d65a6/`, with
trucker's store logs, checks and outputs in `logs/`, but not cargo's build logs.

The stages kept a slice of trucker's log for each step and a hyperfine export for each cold build. Their records now
name the lines of the whole log and hold the times of the runs, as `bench/run` writes them.

  - A sentinel kept only its slices, so `logs/sentinel.<name>.store.log` joins them in the order of its steps.
  - The first `rustflags-sentinel_one` step of rg-rustflags names no lines, because the later step of that label
    overwrote its slice.

The machine is a KVM virtual machine with 16 vCPUs of an AMD EPYC (Milan) processor and 61 GB of memory, running
Ubuntu 26.04.1 with Linux 7.0.0. The filesystem under test is XFS with reflinks, in a loop file. Every build was pinned
to vCPUs 1 to 15 and ran offline in the dev profile.

    Rust 1.100.0 (0ef1bc386), cargo 1.100.0 (3acc79c0c)
    hyperfine 1.19.0, sccache 0.19.0, mbx 1.30.0, kache 2.2.0

The commit of every project is in `projects.txt` of each stage.

  - `throughput-71d65a6/` has plain cargo, a shared target and trucker with four worktrees, two runs of
    cargo-vendored and one of Zed.
  - `throughput-cargo-71d65a6/` has the same for two runs of cargo, which the README's table shows.
  - `sentinels-71d65a6/` has every sentinel, and each program's output is in `logs/sentinel.<name>.outputs`.
  - `campaign-71d65a6/` has every tool on ripgrep, cargo, cargo-vendored, rust-analyzer and Zed 1.29.2, with three
    runs of each scenario. Plain cargo and trucker also ran 1d and `cargo test --no-run`.
  - `projects-71d65a6/` has the sentinel crates, sqlx and wasm as projects, with three runs and one for rocksdb and
    wasmtime. `projects-all.txt` is its `PROJECTS` file, and `locks/` the lockfiles of the
    crates that `bench/setup` commits.
  - `tests-4b22ff7/` has `cargo test --no-run` on ripgrep with every tool, three runs each, from an earlier setup of
    the machine with the same versions, and trucker between 4b22ff7 and 0a6ae0b.

The tables of the README and of `docs/comparisons/` come from these commands, from the run directory:

    python3 -I ../../report.py campaign-71d65a6 --trucker=71d65a6 readme
    python3 -I ../../report.py campaign-71d65a6 --trucker=71d65a6 summary
    python3 -I ../../report.py campaign-71d65a6 --trucker=71d65a6 disk ripgrep cargo cargo-vendored rust zed
    python3 -I ../../report.py projects-71d65a6,campaign-71d65a6 --trucker=71d65a6 geomean
    python3 -I ../../report.py sentinels-71d65a6 sentinels
    python3 -I ../../report.py throughput-71d65a6 throughput cargo-vendored zed
    python3 -I ../../report.py throughput-71d65a6 throughput-readme
    python3 -I ../../report.py throughput-cargo-71d65a6 throughput-readme cargo
    python3 -I ../../report.py tests-4b22ff7 tests ripgrep

`geomean` takes each project of both directories once, so the campaign's projects have their three runs with every
tool. It leaves out cargo-vendored, which would count cargo twice.

## Notes

  - For each project of the campaign, the other tools ran in one invocation, and plain cargo and trucker in another.
    The order of the two invocations alternates by project, and so does the order of the tools within one.
  - kache ran as a plain `RUSTC_WRAPPER`. `bench/run` now sets it up as `kache init` does. Beside the wrapper, init
    changed kache's times by no more than noise on ripgrep, cargo, cargo-vendored and jemalloc.
  - mbx steps aside when a repository's `.cargo/config.toml` names a `build.rustc-wrapper`, so `MBX_OVERRIDE_WRAPPER=1`
    empties `CARGO_BUILD_RUSTC_WRAPPER` for mbx alone.
  - The pass over the projects built sqlx with `SQLX_OFFLINE=true` and wasm with
    `CARGO_BUILD_TARGET=wasm32-unknown-unknown`. Only trucker and plain cargo built cxx and cmake.
  - trucker's first deep edit of cargo-vendored in throughput took 33.2 s in run 1 and 7.7 s in run 2. In run 1, one
    worktree compiled the `cargo` library from a carried session in 4.6 s, and three compiled it cold in 29.8 to 30.3 s.
    In run 2, each worktree carried the session of its own slot from run 1. cargo shows the same, at 34.7 s in run 1,
    with three cold compiles of 30.1 to 31.9 s, and 7.7 s in run 2.
  - The run named cargo `cargo-system` and cargo-vendored `cargo`. The records and file names follow the later names.
  - Not measured here are release builds, switching branches in one worktree, a toolchain upgrade, a cold page cache,
    eviction from a full cache, and any filesystem other than XFS.
