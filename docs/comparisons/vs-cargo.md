# trucker vs what cargo already offers

Cargo can share a target directory or a build directory between worktrees.
A target directory can also be cloned into a new worktree.
These options do not need an install.
They work on every platform and filesystem.

This page describes cargo 1.100.

  - **A shared target directory.** `build.target-dir`, or `CARGO_TARGET_DIR`, points every worktree at one directory.
  - **A shared build directory.** `build.build-dir` shares intermediate outputs.
    Each worktree keeps its own binaries.
  - **A cloned target directory.** `cp -c -R` on APFS or `cp -a --reflink=auto` on Linux copies another worktree's
    target directory with its file times.
    Cargo then finds it fresh.

## Who should use them

A shared target suits a few worktrees that seldom build at once.
On Windows, a shared target also shares your own crates.
trucker caches only third-party crates there, and its Windows support is experimental.
On ext4 a shared target takes the least disk.
trucker copies its outputs into every worktree on ext4.

A cloned target suits an occasional new worktree.

A shared build directory suits worktrees that need binaries of their own.
It costs some disk against a shared target.

## The big differences

**Worktrees take turns.**
Cargo locks a target or build directory for a whole build.
A second worktree waits until the first finishes.
trucker locks a crate only while it compiles.

**A fresh worktree.**
A shared or cloned target compiles the worktree's own crates again.
With a shared target, that took about half as long as a cold build on ripgrep, and 86% as long on rust-analyzer.
trucker restores the own crates too.
A fresh rust-analyzer worktree builds in 8.2 s with trucker and 49 s with a shared target.

**Growth.**
A cloned target stops sharing once it is made.
A shared directory grows without limit.
trucker keeps sharing.
It removes what no build has used for three days.

A cache shared across workspaces is a
[2026 cargo project goal](https://rust-lang.github.io/rust-project-goals/2026/cargo-cross-workspace-cache.html).

## What a fresh worktree reuses

Each cell says whether a fresh worktree reuses the work of an earlier one.

| Build step | trucker | Shared target or build dir | Cloned target |
| --- | --- | --- | --- |
| Pure Rust | Yes | Third-party crates only | Third-party crates only |
| Build scripts | Third-party runs replayed | Yes | Yes |
| C or C++ through the `cc` crate | Yes, replayed | Yes | Yes |
| A vendored C library that bakes its path, such as `openssl-src` | Yes | Yes | Yes |
| Proc macros | Yes | Yes | Yes |
| A repository with its own `rustc-wrapper` | That wrapper must run trucker | Yes | Yes |
| Incremental edits | Carried into each worktree | One shared state | Copied with the target |
| ext4, without reflinks | Yes, by verified copies | Yes | Yes, at full size |

## Settings for faster edits

These help with or without a cache.

  - **`lto = "off"`** in a dev profile with `opt-level` above 0.
    The default value `false` still runs ThinLTO between a crate's codegen units on every rebuild.
  - **`debug = "line-tables-only"`** keeps the line information that backtraces need and drops the rest.
  - **`[profile.dev.package."*"] opt-level = 2`** optimizes third-party crates.
    They compile only once.
  - **lld or [mold](https://github.com/rui314/mold)** as the linker.
    rustc links with lld by default only on `x86_64-unknown-linux-gnu`.

## Benchmarks

The figures come from the [README campaign](../../bench/runs/linux-16core-xfs/README.md).
They were measured on Linux and XFS with Rust 1.100.0 in the dev profile.
Each figure is the mean of three runs.
Plain cargo, trucker 71d65a6 and the shared target ran in one pass.
Plain cargo has no cache, so its fresh worktree is a cold build.
A shared build directory and a cloned target were not measured.

**ripgrep.**

| Scenario | Plain cargo | trucker | Shared target |
| --- | --- | --- | --- |
| Cold build | 6.43s | 6.44s | —[^first] |
| Fresh worktree | 6.43s | 343ms | 3.40s |
| First edit, most depended-on crate | 1.20s | 1.22s | 1.23s |
| Second edit, most depended-on crate | 1.17s | 1.19s | 1.20s |
| First edit, leaf crate | 816ms | 834ms | 836ms |
| Second edit, leaf crate | 833ms | 799ms | 831ms |
| Disk of four worktrees | 1.5 GB | 391 MB | 388 MB |

**cargo**, built against the system's OpenSSL. The vendored rows compile OpenSSL from source.

| Scenario | Plain cargo | trucker | Shared target |
| --- | --- | --- | --- |
| Cold build | 50.88s | 51.87s | —[^first] |
| Cold build, vendored OpenSSL | 74.72s | 76.72s | —[^first] |
| Fresh worktree | 50.88s | 3.74s | 33.20s |
| Fresh worktree, vendored OpenSSL | 74.72s | 3.44s | 33.24s |
| First edit, most depended-on crate | 6.35s | 6.69s | 6.42s |
| Second edit, most depended-on crate | 6.36s | 6.61s | 6.26s |
| First edit, leaf crate | 1.84s | 2.10s | 1.83s |
| Second edit, leaf crate | 1.83s | 1.98s | 1.84s |
| Disk of four worktrees | 13 GB | 3.3 GB | 3.3 GB |

**rust-analyzer.**

| Scenario | Plain cargo | trucker | Shared target |
| --- | --- | --- | --- |
| Cold build | 56.63s | 58.00s | —[^first] |
| Fresh worktree | 56.63s | 8.23s | 48.92s |
| First edit, most depended-on crate | 13.47s | 13.50s | 13.43s |
| Second edit, most depended-on crate | 13.55s | 13.75s | 13.23s |
| First edit, leaf crate | 1.82s | 1.76s | 1.84s |
| Second edit, leaf crate | 1.85s | 1.82s | 1.86s |
| Disk of four worktrees | 18 GB | 6.1 GB | 4.8 GB |

[^first]: A shared target directory is empty before its first build.
    That first build is plain cargo's cold build.
