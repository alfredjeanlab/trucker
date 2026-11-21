# bench

[README.md](README.md) indexes the runs and gives the sentinel verdicts. This file says how to run the bench and
how to change it. The header of each script gives its usage, its variables and its method, which this file does not
repeat.

## Scripts

  - `setup <root> [<locks>]` installs the tools into `<root>/bin` and fetches the pinned projects and their crates.
  - `run <root> <fs> <out> [project]...` times the scenarios of the README for each tool into `steps.jsonl`.
  - `throughput <root> <fs> <out> [project]...` times new worktrees that build at once into `throughput.jsonl`.
  - `sentinel <root> <fs> <out> <name>...` builds each sentinel with trucker and plain cargo and compares their
    programs, into `sentinels.jsonl`.
  - `report.py <out>[,<out>]... <mode>` prints the tables of any of them, and its docstring lists the modes.
  - `sweep <phase> <root> <fs>` builds and tests the most downloaded crates of crates.io with trucker and plain cargo,
    with `sweep.py` parsing for it. It runs only on request, and its results are not committed.

Every script is for Linux, and takes `<fs>` as the directory on the filesystem under test, which may be `<root>`.

## A campaign

A campaign is one pass of the stages on one machine after a fresh `bench/setup`. Its driver, such as
[drive.sh](runs/linux-16core-xfs/drive.sh), runs each stage after a preflight, and the run's README gives
the commands of every stage. The stages are these:

  - `throughput` times the worktrees that build at once.
  - `sentinels` runs every sentinel.
  - `campaign` times the projects of the README with every tool, with three runs of each scenario.
  - `projects` times the sentinel crates, sqlx and wasm as projects of `bench/run`, in 1a, 1b and 2c.

The conventions are these:

  - Each tool is compared with the plain cargo of its own pass, which `report.py` reads from the same directory, or
    from the directories joined to it by `+`. A rerun of a tool runs plain cargo again beside it.
  - A stage starts on a quiet machine. The preflight waits until the load is below 1 and no cargo, rustc or cache runs,
    removes what an earlier stage left under `<fs>`, drops the page cache and checks the free disk. `BENCH_HOLD` names
    the line of `~/ledger` that holds the cores between the invocations of a driver.
  - A machine has a directory `runs/<host>/`, where the host names the OS, the cores and the filesystem, as in
    `linux-16core-xfs`. Its stages are directories named `<stage>-<sha>`, after the commit of trucker that they
    measured. A new stage, such as a rerun of throughput, adds a directory and a note to the run's README, and a stage
    that a newer one repeats is deleted. Names and docs carry no dates.
  - The run's README gives the exact commands, the machine, the versions and the notes that matter, and names the
    `report.py` commands that print its tables. No doc of `bench/` pastes a table that `report.py` prints.
  - Raw records are committed, as the `.jsonl` files, `environment.*.txt`, `projects.txt` and trucker's store logs,
    checks and outputs in `logs/`. cargo's build logs stay on the machine that ran them.
  - `logs/` holds one store log for each project or sentinel, and a record of trucker names its lines in `store_log`
    and `store_lines`. A record of hyperfine's runs, as of 1a, holds their times in `times`, and no export is kept.
  - A new kind of measurement, such as throughput, is a focused stage with a script and a record of its own. It is
    never a new dimension of the matrix of runs, projects and tools that `bench/run` times.

`cargo` is cargo built against the machine's OpenSSL. `cargo-vendored` is a clone of it built with
`--features vendored-openssl`, whose build script bakes its `OUT_DIR` into the outputs. `report.py geomean` leaves
`cargo-vendored` out, so that cargo is not counted twice.

## Adding a sentinel

A sentinel is a directory `sentinels/<name>/`, and `bench/sentinel <root> <fs> <out> <name>` runs it alone.

  - A crate sentinel is a package whose binary `sentinel-<name>` prints what the sentinel checks. `bench/sentinel`
    edits `src/main.rs` by default. `change()` names another input, and `run_output()` another way to run the program.
  - A script sentinel is `sentinels/<name>/sentinel`, which `bench/sentinel` sources with `$work`, `$name`, `step`,
    `verdict`, `cargo_with` and `fixed_git`. It makes its own repositories and gives its own verdict.
  - A new sentinel joins the list of the sentinels stage and the table of README.md.
  - A crate sentinel can also run as a project. It then joins the crates of `bench/setup` and the `projects-all.txt` of
    the campaign, whose lines `bench/run` reads through `PROJECTS`.

## Adding a project

  - `bench/setup` clones it at a pinned commit with a branch named `bench`, and fetches its crates.
  - `bench/run` and `bench/throughput` name its workspace, its most depended-on crate and its leaf crate, and any
    arguments in `extra`. A line of a `PROJECTS` file does the same for one campaign.
  - `report.py` gives it a name with its version in `projects`.
