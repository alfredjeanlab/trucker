# trucker vs mr-boxington

[mr-boxington](https://mr-boxington.jdx.dev) (`mbx`) shares compiled crates between worktrees and replays build-script runs,
as trucker does.

This page describes mbx 1.30.0 at commit `28a5d50`.
mbx was first released in August 2026.

## Who should pick mr-boxington

mbx suits a team that wants one cache for developers and CI.
The cache can live on a cache server, in object storage or in the GitHub Actions cache
([remote cache](https://github.com/jdx/mr-boxington/blob/28a5d5009e7e77f76cd099100966989ec1b0d71c/docs/remote-cache.md)).

It runs on Windows.
On ext4 mbx hard-links its outputs.
trucker copies them there.

With cargo's incremental compilation off, mbx had the fastest cold build on every project.

trucker suits a developer who edits a worktree as soon as it is made.
It does not change the build command.

## The big differences

**Edits in a new worktree.**
mbx builds without cargo's incremental compilation.
In a new worktree, the first edit of a deep crate took mbx 1.7 to 3.2 times as long as plain cargo.
The second edit took 16 to 24% longer than plain cargo.
trucker carries the incremental session between worktrees.
Its first edit took up to 18% longer than plain cargo, on Zed, and its second 2 to 5%.

**How it runs.**
mbx replaces `cargo build` with `mbx build` or a cargo shim.
trucker is one setting in `.cargo/config.toml`.

## What a fresh worktree reuses

Each cell says whether a fresh worktree or a second checkout reuses the work of an earlier one.

| Build step | trucker | mr-boxington |
| --- | --- | --- |
| Pure Rust | Yes | Yes |
| Build scripts | Third-party runs replayed | Yes, replayed |
| C or C++ through the `cc` crate | Yes, replayed | Yes, replayed |
| A vendored C library that bakes its path, such as `openssl-src` | Yes | Only at the same path |
| Proc macros | Yes | Yes, unless one names a native library |
| A crate that bakes its path | Third-party yes, own crates per worktree | Per checkout by default, with its dependents |
| Path dependencies outside the workspace | Yes, inside a git repository | Unknown |
| A repository with its own `rustc-wrapper` | That wrapper must run trucker | Defers to it and caches nothing |
| Incremental edits | Carried into each worktree | Off |
| ext4, without reflinks | Yes, by verified copies | Yes, by hard links |

## Benchmarks

The figures come from the [README campaign](../../bench/runs/linux-16core-xfs/README.md).
They were measured on Linux and XFS with Rust 1.100.0 in the dev profile.
Each figure is the mean of three runs.
Plain cargo, trucker 71d65a6 and mbx ran in one pass.
Plain cargo has no cache, so its fresh worktree is a cold build.
mbx runs through `mbx build` with its defaults.

**ripgrep.**

| Scenario | Plain cargo | trucker | mbx |
| --- | --- | --- | --- |
| Cold build | 6.43s | 6.44s | 6.03s |
| Fresh worktree | 6.43s | 343ms | 592ms |
| First edit, most depended-on crate | 1.20s | 1.22s | 2.09s |
| Second edit, most depended-on crate | 1.17s | 1.19s | 1.40s |
| First edit, leaf crate | 816ms | 834ms | 2.15s |
| Second edit, leaf crate | 833ms | 799ms | 892ms |
| Disk of four worktrees | 1.5 GB | 391 MB | 250 MB |

**cargo**, built against the system's OpenSSL. The vendored rows compile OpenSSL from source.

| Scenario | Plain cargo | trucker | mbx |
| --- | --- | --- | --- |
| Cold build | 50.88s | 51.87s | 48.34s |
| Cold build, vendored OpenSSL | 74.72s | 76.72s | 74.19s |
| Fresh worktree | 50.88s | 3.74s | 37.27s |
| Fresh worktree, vendored OpenSSL | 74.72s | 3.44s | 67.77s |
| First edit, most depended-on crate | 6.35s | 6.69s | 16.70s |
| Second edit, most depended-on crate | 6.36s | 6.61s | 7.35s |
| First edit, leaf crate | 1.84s | 2.10s | 3.89s |
| Second edit, leaf crate | 1.83s | 1.98s | 2.09s |
| Disk of four worktrees | 13 GB | 3.3 GB | 5.3 GB |

**rust-analyzer.**

| Scenario | Plain cargo | trucker | mbx |
| --- | --- | --- | --- |
| Cold build | 56.63s | 58.00s | 49.34s |
| Fresh worktree | 56.63s | 8.23s | 20.95s |
| First edit, most depended-on crate | 13.47s | 13.50s | 32.70s |
| Second edit, most depended-on crate | 13.55s | 13.75s | 16.68s |
| First edit, leaf crate | 1.82s | 1.76s | 2.19s |
| Second edit, leaf crate | 1.85s | 1.82s | 1.83s |
| Disk of four worktrees | 18 GB | 6.1 GB | 5.1 GB |

**Zed** 1.29.2.

| Scenario | Plain cargo | trucker | mbx |
| --- | --- | --- | --- |
| Cold build | 216.81s | 221.88s | 197.46s |
| Fresh worktree | 216.81s | 27.24s | 144.94s |
| First edit, most depended-on crate | 42.50s | 50.07s | 136.85s |
| Second edit, most depended-on crate | 42.07s | 44.34s | 52.26s |
| First edit, leaf crate | 7.91s | 9.15s | 10.63s |
| Second edit, leaf crate | 8.01s | 8.11s | 10.31s |
| Disk of four worktrees | 90 GB | 23 GB | 37 GB |