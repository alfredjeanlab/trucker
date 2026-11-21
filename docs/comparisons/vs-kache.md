# trucker vs kache

[kache](https://github.com/kunobi-ninja/kache) is a compiler cache for Rust, C, C++ and CUDA.
It runs a background daemon and can use remote backends.
Like trucker, it shares crates between worktrees.
It also caches binaries and build-script runs.

This page describes kache 2.2.0 at commit `c23fcf4`.
kache was first released in February 2026.

## Who should pick kache

kache suits a team that wants to share a cache through object storage, a shared filesystem or an OCI registry
([remote cache](https://github.com/kunobi-ninja/kache/blob/c23fcf46c3700b9315754ce135695310af521b25/docs/remote-cache/ci.mdx)).
On cargo against the system's OpenSSL, kache built a fresh worktree in 6.7 s, and trucker in 3.7 s.
Every other tool took over 30 s.

It runs on Windows.
On ext4 kache hard-links its outputs.
trucker copies them there.

trucker suits a developer who edits a worktree as soon as it is made.
It is one binary.
It does not need a daemon or configuration.

## The big differences

**Moving parts.**
`kache init` writes cargo's configuration, installs shims for cargo and the C compilers, and starts a daemon.
trucker is one binary that cargo runs.

**Edits in a new worktree.**
By default, kache keeps a separate incremental state for every target directory.
In a new worktree, the first edit of a deep crate took kache 1.9 to 5.1 times as long as plain cargo.
The second edit took about as long as plain cargo.
trucker carries the session of a crate between worktrees.
Its first edit took up to 18% longer than plain cargo, on Zed.

## What a fresh worktree reuses

Each cell says whether a fresh worktree or a second checkout reuses the work of an earlier one.

| Build step | trucker | kache |
| --- | --- | --- |
| Pure Rust | Yes | Yes |
| Build scripts | Third-party runs replayed | Yes on Linux and macOS |
| C or C++ through the `cc` crate | Yes, replayed | Yes on Linux and macOS |
| A vendored C library that bakes its path, such as `openssl-src` | Yes | Only at the same path, unless an opt-in mode is on |
| Proc macros | Yes | Yes on Linux and macOS, Windows unknown |
| A crate that bakes its path | Third-party yes, own crates per worktree | Per build directory, with its dependents |
| Path dependencies outside the workspace | Yes, inside a git repository | Unknown |
| A repository with its own `rustc-wrapper` | That wrapper must run trucker | `kache init` asks before replacing it |
| Incremental edits | Carried into each worktree | Per target directory, can be seeded from another |
| ext4, without reflinks | Yes, by verified copies | Yes, by hard links |

## Benchmarks

The figures come from the [README campaign](../../bench/runs/linux-16core-xfs/README.md).
They were measured on Linux and XFS with Rust 1.100.0 in the dev profile.
Each figure is the mean of three runs.
Plain cargo, trucker 71d65a6 and kache ran in one pass.
Plain cargo has no cache, so its fresh worktree is a cold build.
kache uses only its local store (`KACHE_LOCAL_ONLY=1`) and the daemon that it starts itself.
`kache init` was not run.

**ripgrep.**

| Scenario | Plain cargo | trucker | kache |
| --- | --- | --- | --- |
| Cold build | 6.43s | 6.44s | 6.06s |
| Fresh worktree | 6.43s | 343ms | 588ms |
| First edit, most depended-on crate | 1.20s | 1.22s | 2.33s |
| Second edit, most depended-on crate | 1.17s | 1.19s | 1.20s |
| First edit, leaf crate | 816ms | 834ms | 2.22s |
| Second edit, leaf crate | 833ms | 799ms | 778ms |
| Disk of four worktrees | 1.5 GB | 391 MB | 260 MB |

**cargo**, built against the system's OpenSSL. The vendored rows compile OpenSSL from source.

| Scenario | Plain cargo | trucker | kache |
| --- | --- | --- | --- |
| Cold build | 50.88s | 51.87s | 59.03s |
| Cold build, vendored OpenSSL | 74.72s | 76.72s | 100.47s |
| Fresh worktree | 50.88s | 3.74s | 6.71s |
| Fresh worktree, vendored OpenSSL | 74.72s | 3.44s | 33.71s |
| First edit, most depended-on crate | 6.35s | 6.69s | 32.63s |
| Second edit, most depended-on crate | 6.36s | 6.61s | 6.36s |
| First edit, leaf crate | 1.84s | 2.10s | 4.14s |
| Second edit, leaf crate | 1.83s | 1.98s | 1.85s |
| Disk of four worktrees | 13 GB | 3.3 GB | 2.3 GB |

**rust-analyzer.**

| Scenario | Plain cargo | trucker | kache |
| --- | --- | --- | --- |
| Cold build | 56.63s | 58.00s | 49.83s |
| Fresh worktree | 56.63s | 8.23s | 21.99s |
| First edit, most depended-on crate | 13.47s | 13.50s | 49.60s |
| Second edit, most depended-on crate | 13.55s | 13.75s | 13.92s |
| First edit, leaf crate | 1.82s | 1.76s | 2.11s |
| Second edit, leaf crate | 1.85s | 1.82s | 1.73s |
| Disk of four worktrees | 18 GB | 6.1 GB | 5.1 GB |

**Zed** 1.29.2.

| Scenario | Plain cargo | trucker | kache |
| --- | --- | --- | --- |
| Cold build | 216.81s | 221.88s | 216.16s |
| Fresh worktree | 216.81s | 27.24s | 40.99s |
| First edit, most depended-on crate | 42.50s | 50.07s | 129.92s |
| Second edit, most depended-on crate | 42.07s | 44.34s | 42.87s |
| First edit, leaf crate | 7.91s | 9.15s | 12.31s |
| Second edit, leaf crate | 8.01s | 8.11s | 7.97s |
| Disk of four worktrees | 90 GB | 23 GB | 22 GB |