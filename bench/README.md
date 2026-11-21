# Results

Each run directory holds the raw records of one campaign, with a README that gives the commands, the machine, the
versions and the notes. `report.py` prints the tables from the records, and [AGENTS.md](AGENTS.md) says how to run
the bench.

## Runs

  - [linux-16core-xfs](runs/linux-16core-xfs) is the campaign of the README, with trucker
    71d65a6 on a 16-core Linux virtual machine with XFS.
    - `campaign-71d65a6/` has every tool on ripgrep, cargo, cargo-vendored, rust-analyzer and Zed.
    - `projects-71d65a6/` has the sentinel crates, sqlx and wasm as projects.
    - `sentinels-71d65a6/` has every sentinel.
    - `throughput-71d65a6/` has four worktrees that build at once, and `throughput-cargo-71d65a6/` the same for cargo.
    - `tests-4b22ff7/` has `cargo test --no-run` on ripgrep with every tool, which the campaign ran with trucker and
      plain cargo only.

## Sentinels

A sentinel is a project that is likely to break a build cache. `bench/sentinel` builds it cold with trucker in a
worktree A and with plain cargo in P, then with trucker in a new worktree B. B and P then change the input that the
sentinel names and build again. A sentinel passes where B's program gives P's output before and after the change, and
A's output stays the same.

Every sentinel passes with trucker 71d65a6. A passthrough is a compile that trucker left to rustc, with its reason.

| Sentinel | What it checks | Passthroughs |
| -------- | -------------- | ------------ |
| rustls | aws-lc-rs, whose build script compiles C | – |
| rocksdb | a build script that compiles C++ | – |
| jemalloc | a build script that compiles C | – |
| openssl | the `vendored` feature, whose build script bakes its `OUT_DIR` into the outputs | – |
| cmake | zlib-ng through the cmake crate, which bakes `OUT_DIR` into CMake's cache | – |
| cxx | an own crate whose build script links headers into `OUT_DIR` through cxx-build, with an edit of the C++ | 3 baked-path |
| wasmtime | v50.0.2, whose build scripts generate Cranelift's code, with an edit of the `wasmtime` crate | 8 baked-path, 2 baked-slot |
| prost | a build script that compiles a `.proto` file, which the edit extends | – |
| embed | an asset through rust-embed with `debug-embed` and a template through askama | – |
| vergen | the commit that vergen-gitcl embeds, with a new commit in B and P | – |
| bevy | `dynamic_linking`, whose binary loads a dylib | – |
| pyo3 | an extension module that Python imports | – |
| sqlx | the `.sqlx` file that `query!` reads without declaring it, which B's commit changes | – |
| sqlx-migrate | the migrations that `migrate!` embeds from the workspace root, to which B's commit adds one | – |
| wasm | a workspace with a proc macro and a build script, for wasm32 and natively in one target | – |
| tests | ripgrep's `cargo test --no-run`, every test, and `cargo test --doc` | – |
| rg-clippy | ripgrep's clippy warnings in a new worktree | – |
| rg-coverage | `cargo llvm-cov`, whose report must equal plain cargo's | 12 coverage |
| rg-check | `cargo check` before `cargo build` | – |
| rg-space | a worktree whose path has a space | – |
| rg-thinlto | ThinLTO in the release profile | – |
| rg-rustflags | RUSTFLAGS changed and changed back | – |

`python3 -I bench/report.py bench/runs/linux-16core-xfs/sentinels-71d65a6 sentinels` prints every build
and verdict.

  - An edit of `.sqlx` in place changes nothing for plain cargo or trucker, which is plain cargo's own blind spot.
  - ripgrep's 323 integration tests fail under both, because they look for `rg` beside their own executable.
  - wasmtime compiles two crates again in B as `baked-slot`, because their outputs name the slot of their session.

## Throughput

`bench/throughput` measured cargo and cargo-vendored with two runs and Zed with one, each with four worktrees, for
plain cargo, a shared target and trucker 71d65a6. The run's README holds the commands and the notes.

  - A shared target makes the worktrees wait for each other on cargo's lock, so each step takes about four builds.
  - In Zed, a worktree of the shared target compiles hundreds of crates again at each step.
  - trucker's first deep edit took 34.7 s on cargo and 33.2 s on cargo-vendored in the first run, and 7.7 s on both
    in the second, as the README of the run explains.
  - trucker's second deep edit is within 3% of plain cargo on both projects.
