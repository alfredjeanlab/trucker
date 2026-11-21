# trucker vs sccache

[sccache](https://github.com/mozilla/sccache) is a compiler cache for Rust, C, C++ and CUDA.
The Mozilla organization has maintained it since 2016.

This page describes sccache 0.19.0.

## Who should pick sccache

sccache is the mature choice for a cache that leaves the machine.
It stores compiles in object storage and [several other backends](https://github.com/mozilla/sccache/blob/v0.19.0/README.md).
CI runners and a team can then share one cache.

It runs on Windows, Linux and macOS, on any filesystem.
It caches C and C++ compiles too.
Where incremental compilation is off, as CI runners often set with `CARGO_INCREMENTAL=0`, sccache caches workspace
libraries as well.

trucker is for many worktrees of one project on one machine.
Cargo takes one `RUSTC_WRAPPER`.
A project can still use sccache on CI and trucker on developer machines.

## The big differences

**A fresh worktree.**
In a dev build sccache caches only third-party libraries.
It skips workspace crates, because cargo compiles them incrementally.
It also skips crates that link, such as binaries and proc macros.
On rust-analyzer a fresh worktree took 58 s with sccache and 8.2 s with trucker.
Plain cargo's cold build took 57 s.

**Disk.**
sccache extracts a full copy into each target directory.
Four rust-analyzer worktrees took 17 GB with sccache and 6.1 GB with trucker.

## What a fresh worktree reuses

Each cell says whether a fresh worktree or a second checkout reuses the work of an earlier one.

| Build step | trucker | sccache |
| --- | --- | --- |
| Pure Rust | Yes | Third-party libraries only |
| Build scripts | Third-party runs replayed | No |
| C or C++ through the `cc` crate | Yes, replayed | Cached, but misses at another path |
| A vendored C library that bakes its path, such as `openssl-src` | Yes | No |
| Proc macros | Yes | No |
| A crate that bakes its path | Third-party yes, own crates per worktree | Misses at another path |
| Path dependencies outside the workspace | Yes, inside a git repository | No |
| A repository with its own `rustc-wrapper` | That wrapper must run trucker | That wrapper must run sccache |
| Incremental edits | Carried into each worktree | Not cached |
| ext4, without reflinks | Yes, by verified copies | Yes, by copies |

## Benchmarks

The figures come from the [README campaign](../../bench/runs/linux-16core-xfs/README.md).
They were measured on Linux and XFS with Rust 1.100.0 in the dev profile.
Each figure is the mean of three runs.
Plain cargo, trucker 71d65a6 and sccache ran in one pass.
Plain cargo has no cache, so its fresh worktree is a cold build.
sccache uses its local disk cache.

**ripgrep.**

| Scenario | Plain cargo | trucker | sccache |
| --- | --- | --- | --- |
| Cold build | 6.43s | 6.44s | 7.39s |
| Fresh worktree | 6.43s | 343ms | 4.70s |
| First edit, most depended-on crate | 1.20s | 1.22s | 1.18s |
| Second edit, most depended-on crate | 1.17s | 1.19s | 1.19s |
| First edit, leaf crate | 816ms | 834ms | 755ms |
| Second edit, leaf crate | 833ms | 799ms | 756ms |
| Disk of four worktrees | 1.5 GB | 391 MB | 1.6 GB |

**cargo**, built against the system's OpenSSL. The vendored rows compile OpenSSL from source.

| Scenario | Plain cargo | trucker | sccache |
| --- | --- | --- | --- |
| Cold build | 50.88s | 51.87s | 55.08s |
| Cold build, vendored OpenSSL | 74.72s | 76.72s | 83.87s |
| Fresh worktree | 50.88s | 3.74s | 53.26s |
| Fresh worktree, vendored OpenSSL | 74.72s | 3.44s | 81.58s |
| First edit, most depended-on crate | 6.35s | 6.69s | 6.33s |
| Second edit, most depended-on crate | 6.36s | 6.61s | 6.34s |
| First edit, leaf crate | 1.84s | 2.10s | 1.79s |
| Second edit, leaf crate | 1.83s | 1.98s | 1.78s |
| Disk of four worktrees | 13 GB | 3.3 GB | 14 GB |

**rust-analyzer.**

| Scenario | Plain cargo | trucker | sccache |
| --- | --- | --- | --- |
| Cold build | 56.63s | 58.00s | 57.35s |
| Fresh worktree | 56.63s | 8.23s | 57.52s |
| First edit, most depended-on crate | 13.47s | 13.50s | 13.48s |
| Second edit, most depended-on crate | 13.55s | 13.75s | 13.50s |
| First edit, leaf crate | 1.82s | 1.76s | 1.80s |
| Second edit, leaf crate | 1.85s | 1.82s | 1.75s |
| Disk of four worktrees | 18 GB | 6.1 GB | 17 GB |