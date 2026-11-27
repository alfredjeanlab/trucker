<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/trucker-banner-dark.svg">
  <img src="docs/trucker-banner-light.svg" alt="trucker: hauls crates to your worktrees" width="440" height="96">
</picture>

Trucker is a `RUSTC_WRAPPER` that shares compiled crates between every worktree and clone on a machine.

A crate that has been compiled once is restored into each later build by cloning its files,
so a fresh worktree builds in a fraction of the time and uses much less disk space.

## Requirements

  - Rust and cargo 1.100 or later. Trucker runs an older rustc untouched.
  - macOS, Linux or Windows, as [supported platforms](#supported-platforms) says.
  - The store and the target directories on one filesystem.  
    The store is `~/.cache/trucker` or `$XDG_CACHE_HOME/trucker`.  

## Installation

```
brew install alfredjeanlab/tap/trucker
cargo install trucker
```

Binaries for Linux and macOS are on the [releases page](https://github.com/alfredjeanlab/trucker/releases).

Then name it in the `.cargo/config.toml` of a repository, or of your home directory:

```toml
[build]
rustc-wrapper = "trucker"
```

## Usage

```
cargo build                     # build through trucker, once configured

RUSTC_WRAPPER= cargo build      # build without trucker

trucker gc                      # removes entries that no build has used lately
trucker gc --all                # empties the cache, but keeps what a running build holds
trucker stats                   # cache hits, misses and build timers
trucker missed                  # the causes of misses, ranked by the time that they and their dependents took
trucker missed <crate>          # why the newest miss of a crate missed
trucker missed --duplicates     # the crates that workspaces compile again for other features or versions
trucker --version               # the version, and a hash of the binary that tells two builds apart
```

`trucker stats` and `trucker missed` take `--since` with a time in seconds since 1970, or an age such as `90s`, `15m`,
`2h` or `3d`. A miss is explained against the last compile of the crate with the same arguments in the last few hours.

### What `trucker stats` reports

`trucker stats` prints a line for each outcome in the log, with its count, the total time that trucker took for it, its
code and a short description. The time is that to restore for a hit, and to compile or run the build script otherwise.
The share of hits is the hits divided by the hits and misses, so it leaves out what could not be cached.

A rebuild of your own crate, as after an edit, is cached lazily. Trucker records where its outputs lie in the target
folder that built them, and copies them into the store only once another build asks for them. Most rebuilds are never
asked for again, so an edit pays for no copy and the store does not grow with it.

| Code | What happened | Next build with the same inputs | What you can do |
| --- | --- | --- | --- |
| `hit` | The outputs were restored from the store, and rustc or the build script did not run. | Hits. | Nothing. |
| `hit waited` | Another build was compiling the same inputs, so this one waited for it and restored its result. | Hits. | Nothing. |
| `hit promoted` | An earlier rebuild of your crate had left its outputs in its target folder. Trucker copied them into the store, checked them against the hashes recorded at the compile, and restored them. | Hits. | Nothing. |
| `miss` | Nothing in the cache matched, so rustc or the build script ran and its result was cached. | Hits. | `trucker missed` says what differed from an earlier compile. |
| `miss session` | Nothing in the cache matched, so rustc ran and its result was cached. Its incremental compile started from the work of an earlier compile, usually in another worktree. | Hits. | `trucker missed` says what differed from an earlier compile. |
| `miss evicted` | A cached copy no longer matched its hash, as when a tool rewrote it through a hard link, so it was removed and the crate was compiled and cached again. | Hits. | Nothing, unless it repeats because a tool such as `strip` writes outputs in place. |
| `passthrough rustc-failed` | rustc exited with an error, so nothing was cached. | Compiles. | Fix the error that cargo shows. |
| `passthrough changed-source` | A source in the worktree changed in the second before the compile or during it, so rustc may have read other contents than trucker hashed. | Compiles. | Nothing, unless a tool writes sources during every build. |
| `passthrough baked-path` | An output of your crate contains the path of its worktree or target folder, as `env!("CARGO_MANIFEST_DIR")` can leave, which a hit would carry into another worktree. | Compiles. | Keep absolute paths out of the outputs, or accept that the crate compiles in each worktree. |
| `passthrough baked-slot` | The incremental compile of your crate ran in a folder of the store that other worktrees continue from, and an output named that folder. The crate was compiled again in the worktree. | Compiles, and no longer runs in such a folder. | Nothing. |
| `passthrough baked-slot-late` | An output of your crate named the folder of the store that it compiled in, after cargo had started its dependents. Trucker failed the build, and the crate no longer compiles in such a folder. | Compiles in the worktree. | Run the build again. |
| `passthrough dep-info` | The list of sources that rustc writes for cargo could not be rewritten to name the worktree, so the crate was compiled again in the worktree. | Compiles. | Nothing. |
| `passthrough unkeyed-input` | The crate or its build script read a file whose changes trucker cannot follow, such as one outside the package, the target folder and CARGO_HOME. | Compiles. | Nothing, or move the file into the package. |
| `passthrough undeclared-input` | Your crate uses the macros of sqlx, which query a database unless `SQLX_OFFLINE` is set and `SQLX_OFFLINE_DIR` is not. | Compiles. | Set `SQLX_OFFLINE=true` and commit the offline data in `.sqlx`. |
| `passthrough unreadable-read` | A file that an earlier compile read, and that decides which cached result fits, could not be read or hashed. | Compiles. | Check the permissions of the files that the crate reads. |
| `passthrough unreadable-input` | A file that the lookup hashes, such as the compiler or a library of a dependency, could not be read. | Compiles. | Check the permissions of the toolchain and the target folder. |
| `passthrough no-source-read` | Your crate read no source in its worktree, as rustc lists them, so trucker cannot tell what its result depends on. | Compiles. | Nothing. |
| `passthrough no-repository` | Your crate does not lie in a git repository, so trucker cannot tell which worktree it belongs to. | Compiles. | Build inside a git repository. |
| `passthrough windows-own-crate` | Your crates compile as with plain cargo on Windows. | Compiles. | Nothing. |
| `passthrough coverage` | Your crate is built with `-C instrument-coverage`, whose paths cargo-llvm-cov matches, so it compiles as with plain rustc. | Compiles. | Nothing. |
| `passthrough incremental` | A crate that is not your own is compiled incrementally, as `-C incremental` in RUSTFLAGS does, which trucker caches only for your crates. | Compiles. | Set it through the profile or CARGO_INCREMENTAL instead. |
| `passthrough argument-file` | rustc read its arguments from an `@file`, which trucker does not read. | Compiles. | Nothing. |
| `passthrough explicit-output` | The arguments name the output file with `-o` rather than cargo's output folder. | Compiles. | Nothing. |
| `passthrough emit` | The arguments ask for outputs other than those of a build, such as assembly, or leave out the list of sources. | Compiles. | Nothing. |
| `passthrough no-crate-name` | The arguments do not name the crate with `--crate-name`. | Compiles. | Nothing. |
| `passthrough no-out-dir` | The arguments do not name an output folder, or a build script runs without `OUT_DIR`. | Compiles. | Nothing. |
| `passthrough no-target-dir` | The output folder does not lie where cargo puts the outputs of a target folder. | Compiles. | Nothing. |
| `passthrough escaped-path` | The path of the target folder, CARGO_HOME or the worktree holds a quote, a control character or, outside Windows, a backslash, which rustc escapes in its messages. | Compiles. | Rename the folder. |
| `passthrough path-spelling` | On Windows, a path can also be spelled another way, as through a `subst` or network drive. | Compiles. | Build from the path of the drive itself. |
| `passthrough no-working-directory` | The current folder could not be read. | Compiles. | Nothing. |
| `passthrough other-filesystem` | The store and the target folder lie on different filesystems, so no file can be cloned between them. | Compiles. | Put both on one filesystem, as [requirements](#requirements) says. |
| `passthrough clone-failed` | The filesystem refused to clone an output into the store. | Compiles. | Check the free space on the disk. |
| `passthrough store-error` | Trucker could not write into the store, as when the disk is full. | Compiles. | Check the free space on the disk and the permissions of the store. |
| `passthrough unremovable-output` | An earlier output in the target folder could not be removed. | Compiles. | Check the permissions of the target folder. |
| `passthrough rustc-not-started` | rustc could not be started. | Compiles. | Check the toolchain. |
| `passthrough no-stand-in` | Trucker could not put its replay in place of a third-party build script, so cargo ran the script itself. | Runs the script. | Check the free space on the disk. |
| `passthrough script-not-started` | A build script could not be started. | Runs the script. | Check the target folder. |
| `passthrough script-failed` | A build script failed. Where trucker had added a prefix map to CFLAGS and CXXFLAGS, the script ran again without it, and nothing was cached. | Runs the script. | Fix the error, where the build fails. |
| `passthrough binary-output` | A build script printed output that is not UTF-8 text. | Runs the script. | Nothing. |
| `passthrough binary-target-path` | A build script left the path of the target folder in a binary file, and a run with its output in a fixed folder of the store failed or was ruled out before. | Runs the script, until gc removes the mark after three days. | Nothing. |
| `passthrough fixed-run-failed` | A build script that leaves its path in a binary failed when it ran with its output in a fixed folder of the store, which keeps that path the same in every worktree. | Runs the script. | Nothing. |
| `passthrough script-escaped` | A build script that ran with its output in a fixed folder of the store wrote outside that folder, or changed a file that the lookup hashes. | Runs the script. | Nothing. |
| `passthrough unmovable-output` | The output of a build script that ran in a fixed folder of the store could not be moved into cargo's output folder. | Runs the script. | Check the permissions of the target folder. |
| `passthrough symlink` | A build script left a symlink in its output folder that names a path outside it and outside the sources of dependencies. | Runs the script. | Nothing. |
| `passthrough windows-fixed` | On Windows, a build script that leaves its path in a binary cannot run in a fixed folder of the store, because that needs a symlink. | Runs the script. | Nothing. |
| `passthrough unnormalized-path` | On Windows, the output or messages of a build script spell the path of the target folder in a way that trucker cannot replace. | Runs the script. | Nothing. |
| `note outside-workspace` | A note after a miss of your crate that is a path dependency outside the workspace root, which cargo names by its absolute path. Such a crate is cached once in each worktree. | Hits in the same worktree. | Move the package under the workspace root. |
| `note unkept-session` | A note that the incremental work of a compile could not be kept for other worktrees, followed by the kind of error. | Hits. | Check the free space on the disk. |

Trucker automatically decides whether to run `trucker gc` after a miss so it rarely needs to be run by hand.
Trucker will use up to 10% of the filesystem, between 16 GiB and 100 GiB,
and always leaves behind at least 10% free space on the disk.
The cap counts the disk that the store adds, so a block that it shares with a target directory does not count.
Once the store comes within a tenth of its cap or of that floor, gc removes entries that no build hit for an hour,
then the least recently used, however recently used, until it is a tenth of its cap clear again.
It keeps only what a running compile holds.

Use `TRUCKER_CACHE_SIZE` to adjust the cache size, in bytes or with a suffix K, M, G or T (e.g. `=200G`).

## When to use Trucker

Use trucker whenever you run builds from **one or more worktrees or checkouts on a single machine**,
as is typical during agentic development with Claude, Codex, Conductor, Zed, etc.

It makes normal builds faster:

- The first build in a new worktree or checkout.
- The first edit (and subsequent edits) to any crate in the workspace.

It helps run more concurrent work on the same machine:

- Disk usage is bounded and garbage collected.
- Concurrent builds compile each shared crate only once.
- Overall system load is minimized to leave room for other work.

It's almost always faster than cargo alone, and never meaningfully slower.

## In CI

Trucker is useful in CI on runners that keep the store and target on the same persistent disk.  
It is not designed for classic GitHub Actions-like runners.

  - **Persistent runners:** try trucker for tools like [Blacksmith](https://docs.blacksmith.sh/blacksmith-caching/dependencies-sticky-disks) or [Namespace](https://namespace.so/docs/architecture/storage/cache-volumes).
  - **GitHub Actions or Forgejo Actions:** use [rust-cache](https://github.com/Swatinem/rust-cache).
  - **GitLab CI:** use a `cache:` of `target/` and `~/.cargo`, keyed on `Cargo.lock`.

See [CI caches](docs/comparisons/vs-ci.md) for a comparison of other CI caching tools.

## Trucker vs alternatives

Times and disk use are relative to plain cargo; lower is better.  
See the [benchmarks](#benchmarks) below.

| | trucker | kache | mr-boxington | shared `target/` | sccache |
| --- | --- | --- | --- | --- | --- |
| Build fresh worktree | 0.05–0.15× | 0.09–0.39× | 0.09–0.73× | 0.53–0.86× | 0.73–1.05× |
| 1st edit in worktree | 0.96–1.18× | 1.15–5.14× | 1.20–3.22× | 0.97–1.03× | 0.93–1.14× |
| 2nd edit in worktree | 0.96–1.08× | 0.93–1.03× | 0.99–1.29× | 0.97–1.03× | 0.91–1.01× |
| Disk per worktree | 0.25–0.34×[^ext4] | 0.17–0.28× | 0.16–0.41× | 0.25–0.29× | ~1× |

Trucker also handles the most common kinds of build steps in Rust projects:

  - **Native code:** C or C++ that a build script compiles with `cc` or CMake, and `cxx` bridges.
  - **Generated code:** sources that a build script writes into `OUT_DIR`, even where it bakes that path into a binary.
  - **Proc macros that read files:** embedded assets, templates, and sqlx's offline data and migrations.
  - **Build info:** the git commit that each worktree embeds, as vergen does.
  - **Libraries loaded at run time:** a `cdylib` such as a pyo3 extension, and a `dylib` such as Bevy's.

All of `cargo check`, `cargo clippy` and `cargo test` share the cache with `cargo build`, so they are fast in a fresh
worktree too, and a restored crate still shows its clippy warnings.

Each target, profile, and flagset is cached separately, so wasm or release builds are also reused.

See also: [sentinel projects](bench/README.md#sentinels) used to test trucker.

## Benchmarks

Each figure is the mean of 3 builds with Rust 1.100.0 in the dev profile, on a 16-core Linux virtual machine with XFS,
offline and pinned to 15 cores, with each tool at its defaults.

The matrix above compares each tool with the plain cargo run alongside it.

The methods, scripts and results are in [bench/](bench/README.md).

### Fresh worktree, warm cache

A new git worktree of the same commit builds, with the cache warm from the cold build of another worktree.

| Project | cargo | trucker | [kache] | [mr-boxington] | [shared `target/`][shared] | [sccache] |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| ripgrep | *6.43s* | **343ms** | 588ms | 592ms | 3.40s | 4.70s |
| cargo | *50.88s* | **3.74s** | 6.71s | 37.27s | 33.20s | 53.26s |
| rust-analyzer | *56.63s* | **8.23s** | 21.99s | 20.95s | 48.92s | 57.52s |
| Zed | *216.81s* | **27.24s** | 40.99s | 144.94s | 133.46s | 204.83s |

### First edit of the deepest crate

The new worktree appends an unused function to the crate that every other own crate depends on, and builds.

| Project | cargo | trucker | [kache] | [mr-boxington] | [shared `target/`][shared] | [sccache] |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| ripgrep | *1.20s* | 1.22s | 2.33s | 2.09s | 1.23s | **1.18s** |
| cargo | *6.35s* | 6.69s | 32.63s | 16.70s | 6.42s | **6.33s** |
| rust-analyzer | *13.47s* | 13.50s | 49.60s | 32.70s | **13.43s** | 13.48s |
| Zed | *42.50s* | 50.07s | 129.92s | 136.85s | **41.22s** | 43.38s |

### First edit of a leaf crate

The new worktree appends an unused function to the binary crate, which nothing depends on, and builds.

| Project | cargo | trucker | [kache] | [mr-boxington] | [shared `target/`][shared] | [sccache] |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| ripgrep | *816ms* | 834ms | 2.22s | 2.15s | 836ms | **755ms** |
| cargo | *1.84s* | 2.10s | 4.14s | 3.89s | 1.83s | **1.79s** |
| rust-analyzer | *1.82s* | **1.76s** | 2.11s | 2.19s | 1.84s | 1.80s |
| Zed | *7.91s* | 9.15s | 12.31s | 10.63s | **7.77s** | 9.04s |

### Disk use by four worktrees

The filesystem's used space, by `df`, after one cold build and three new worktrees.

| Project | cargo | trucker | [kache] | [mr-boxington] | [shared `target/`][shared] | [sccache] |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| ripgrep | *1.5 GB* | 391 MB | 260 MB | **250 MB** | 388 MB | 1.6 GB |
| cargo | *13 GB* | 3.3 GB | **2.3 GB** | 5.3 GB | 3.3 GB | 14 GB |
| rust-analyzer | *18 GB* | 6.1 GB | 5.1 GB | 5.1 GB | **4.8 GB** | 17 GB |
| Zed | *90 GB* | 23 GB | **22 GB** | 37 GB | 26 GB | 93 GB |

### Cold build

One worktree builds with an empty target and an empty cache. mr-boxington turns off cargo's incremental compilation, so
its cold build writes less and its first edits recompile more.

| Project | cargo | trucker | [kache] | [mr-boxington] | [sccache] |
| --- | ---: | ---: | ---: | ---: | ---: |
| ripgrep | *6.43s* | 6.44s | 6.06s | **6.03s** | 7.39s |
| cargo | *50.88s* | 51.87s | 59.03s | **48.34s** | 55.08s |
| rust-analyzer | *56.63s* | 58.00s | 49.83s | **49.34s** | 57.35s |
| Zed | *216.81s* | 221.88s | 216.16s | **197.46s** | 240.45s |

[^ext4]: 1× on a filesystem without reflinks, such as ext4, where each worktree gets a checked copy.

[sccache]: docs/comparisons/vs-sccache.md
[mr-boxington]: docs/comparisons/vs-mr-boxington.md
[kache]: docs/comparisons/vs-kache.md
[shared]: docs/comparisons/vs-cargo.md

The figures were measured on Linux with XFS.

A fresh worktree typically builds in 1/10th the time of a cold build (seconds instead of minutes),
and incremental edit workflows stay as fast as cargo (~1x cargo incremental builds).

## Concurrent builds (throughput)

Four new worktrees of the same commit build at once against a warm store, then each edits a leaf crate, the deepest
crate, and the deepest crate again. Each figure is the time until the last worktree finishes, from one or two runs.

| Project | Step | cargo | [shared `target/`][shared] | trucker |
| --- | --- | ---: | ---: | ---: |
| cargo | Fresh worktree | 102.63s | 34.93s | **3.94s** |
| cargo | Leaf edit | **2.50s** | 3.51s | 3.71s |
| cargo | Deep edit | **7.23s** | 33.58s | 21.19s |
| cargo | Second deep edit | **7.29s** | 33.65s | 7.49s |
| Zed | Fresh worktree | 837.21s | 422.67s | **38.86s** |
| Zed | Leaf edit | **21.34s** | 389.75s | 27.95s |
| Zed | Deep edit | 254.60s | 420.04s | **156.91s** |
| Zed | Second deep edit | **176.48s** | 397.76s | 179.94s |

A shared `target/` makes the worktrees wait for each other on cargo's lock.

With trucker, if several new worktrees edit the same crate at the same time,
some will recompile the whole crate to avoid waiting on each other.
Later edits are as fast as plain cargo.

## Supported platforms

  - **Supported:** macOS on APFS, and Linux on XFS, Btrfs or ext4.
    The tests run on macOS and on Linux with XFS and ext4, and the benchmarks above run on Linux with XFS.
    On a filesystem without reflinks, such as ext4, each build gets a copy that is checked against its hash,
    so a fresh worktree is as fast but takes its full size on disk.
  - **Experimental:** Windows 11, for third-party crates only, and your own crates compile as with plain cargo.
    Files are cloned on a Dev Drive and copied with a hash check on NTFS.
    It has not yet been tested on a Windows machine.
    Inside WSL 2, trucker runs as on Linux, with the repository and the store on its Linux filesystem.

## License

trucker is licensed under either of the [Apache License, Version 2.0](LICENSE-APACHE) or the [MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in trucker, as defined in
the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
