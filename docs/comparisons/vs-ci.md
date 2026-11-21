# trucker vs CI caches

trucker keeps its store on the local disk.
It does not have a remote store.
On CI it helps only on a runner that keeps its disk between jobs.
Examples are a self-hosted machine and a hosted runner with a persistent disk.

A runner that starts empty needs one of the caches below.
Any of them can run on CI with trucker on developer machines.
They were not benchmarked here.

## Caches that a fresh runner downloads

**[Swatinem/rust-cache](https://github.com/Swatinem/rust-cache)** is a GitHub Action.
It keeps third-party crates in the GitHub Actions cache.
A job then compiles only the workspace.
It is the default choice on GitHub Actions.

**[sccache](https://github.com/mozilla/sccache) with a remote backend** stores each compile in object storage
or another backend.
Many runners and developers can then share one cache.
[trucker vs sccache](vs-sccache.md) compares the two on one machine.

**[cargo-chef](https://github.com/LukeMathWalker/cargo-chef)** builds only the dependencies, in a Docker layer of their own.
That layer stays cached until the manifests or lockfile change.
cargo-chef is for Docker images.
It is not for local builds.

## Build systems with a remote cache

**Nix with [crane](https://github.com/ipetkov/crane)** builds the dependencies into one derivation.
A Nix binary cache such as Cachix shares it with CI.
It suits a project that already builds with Nix.

**Bazel with [rules_rust](https://github.com/bazelbuild/rules_rust)** and **[Buck2](https://github.com/facebook/buck2)**
build each crate as an action that a remote cache can serve.
They replace cargo with build files for every crate.
They suit a large monorepo in several languages.

Nix and Bazel also sandbox each action.
The sandbox catches a build script or proc macro that reads an undeclared file.
Cargo and trucker do not catch such a read.

## Hosted runners with a persistent cache

On a runner whose disk survives between jobs, a later job finds the store of an earlier one.
trucker needs the store and the target directory on one filesystem.
On ext4 it restores by copy.

Hosted runners such as [Namespace](https://namespace.so), [Blacksmith](https://blacksmith.sh),
[RunsOn](https://runs-on.com) and [WarpBuild](https://www.warpbuild.com) can keep a disk between jobs.
Namespace, RunsOn and [Depot](https://depot.dev) also host an sccache backend.
trucker has not been measured on any of them.

## When to pick one

  - On **GitHub Actions**, use rust-cache.
  - For **many runners, or a team that shares compiles**, use sccache with a bucket.
  - For **a Docker image**, use cargo-chef.
  - For **a project that builds with Nix**, use crane with a binary cache.
  - For **a large monorepo in several languages**, use Bazel or Buck2.
  - On **a self-hosted runner or a persistent disk**, use trucker as on a developer machine.
