# trucker

README.md says what trucker is, how it is used and configured, and where it runs.

    make check    # fmt, clippy with warnings denied, and every test

The repository builds itself through `.cargo/rustc-wrapper`, which runs the installed trucker where there is one and
rustc alone where there is not. A branch's own binary is tested by naming it in `RUSTC_WRAPPER`. `tests/linux.sh`
runs the Linux path inside a VM on XFS and ext4, and `tests/windows.sh` runs real builds on a Windows runner of CI.

## Commits

  - A commit message is one line, `type(scope): ` and a sentence saying what the change does, with no body and no
    attribution trailer such as `Co-Authored-By`. The type is one of `feat`, `fix`, `perf`, `refactor`, `docs`,
    `test` or `chore`.

## Prose

  - Comments and docs are ordinary English sentences. A doc comment says what the code and names cannot, such as an
    invariant, an edge case or a reason, and has no history.
  - No telegraphic style. A phrase is never joined by a colon to a fragment. Write a sentence.
  - No nominal style. A verb carries the sentence, and a negation goes on the verb.
    Write "the file that a request writes", not "the file a request writes", and "does not name a function" or
    "is not an entry", not "names no function" or "is no entry".
    A relative clause opened by "that" or "which" is ordinary English.
  - No "we", "us" or "our". Say what a thing is, never who reads it or that nothing does.
  - No counts of what the reader can already see. A number that is the constraint stays.
  - A sentence is at most 30 words, with at most one possessive per noun phrase ("its slot's session's file" is a
    chain), and does not repeat "each".
  - Three or more cases, conditions or parts are a bulleted list of short sentences.
  - AGENTS.md holds the design and the rules for changing trucker, and never repeats what README.md says.

### Doc comments

  - The first paragraph is one sentence, and a blank line follows it.
  - A test has no doc comment, because its name is the sentence. A line comment above it may say what the name
    cannot, such as the bug that it guards against.
  - A private item gets no doc where its name and type say it all, and otherwise one sentence. More paragraphs are
    only for an invariant, a failure, safety or ordering, and the doc is at most 8 lines.
  - A `pub` item gets a one-sentence summary, and more only for errors, invariants or safety, in at most 15 lines.
  - A module's `//!` is at most 12 lines that say what the module is and the rules that span its items. A design
    narrative goes in AGENTS.md.
  - A doc never restates another item's doc, and links it as ``[`name`]`` instead.

### Names

  - A name comes from the domain or the type, never from a doc's wording or from how the thing is stored.
  - A local has a short name where its scope makes it clear. A function or field gets a clear name that does not
    repeat its context.
  - A participle names only the result of an operation, as `restored` names what a restore returns. A count, flag,
    guard or state is named by the noun of what it contains, never by a past-tense verb.

## Principles

  - A build gets what a fresh compile would give it, as far as cargo's own checks can tell.
    Where trucker cannot show that, it compiles.
  - cargo stays in charge. trucker answers the compiles that cargo asks for, and never edits what cargo records.
  - A failure in trucker costs time, never a wrong or broken build, and the log says why.
  - trucker works without configuration, and adds no variable, file or flag beyond those that README.md names.
  - A file is shared between worktrees only where no later write can reach it from another.
  - A copy is checked before it is trusted.
  - The store uses less disk than the targets that it saves, and stays within reasonable usage bounds.

## Current architecture

  - `main.rs` dispatches the subcommands, and for a compile decides whether it is cached, runs rustc, and logs the
    outcome of each invocation to the store's `log`. A failure runs rustc unchanged, logged as `passthrough <reason>`.
    A panic would fail the build, so clippy denies `unwrap`, `expect` and `panic` outside tests.
  - `unit.rs` reads rustc's arguments into a `Unit` and computes its key. It relies on the layout of cargo 1.100 and
    later, which gives each unit a directory of its own. The paths of the target directory,
    CARGO_HOME and the worktree enter a key through placeholders, so two worktrees of one repository compute the same
    key. What rustc names only in its dep-info, such as a source of an own crate or a file of OUT_DIR, selects among
    the entries of one key. The blind spot, as for cargo, is what a proc macro or build script reads without
    declaring it, such as a variable, a database, or a file outside the walks that [Own crates](#own-crates) describes.
  - `store.rs` owns the entries. An entry is staged in a directory of its own and renamed into place, so a reader
    finds it whole or missing. A miss compiles under the lock of the entry that its inputs select, so a process with
    other sources never waits. A probe finds out once per store whether its filesystem clones, and a copy is renamed
    into place only once its blake3 hash matches. A store on another filesystem than the target directory leaves every
    crate uncached and says so once per build. Memos and locks lie in 256 shards named by the first two digits of their
    hash, because a store holds millions of them, and some filesystems list or search a flat directory of that many
    names slowly.
  - `lazy.rs` records a rebuilt own crate as a candidate, which a later miss promotes into an entry.
  - `session.rs` carries the incremental sessions of own crates between worktrees through slots.
  - `script.rs` is the stand-in that cargo runs in place of a third-party build script, which replays a stored run.
  - `gc.rs` removes what no build has used for three days, then the entries and slots while the store is within a
    tenth of its cap, however recently used. An entry that no build hit and that is over an hour old goes first, and
    the rest go least recently used first. A hit sets the mtime, so an entry whose mtime is within a second of its
    birth time was never hit. Only a slot that a compile holds stays, and a run reads
    remembered sizes rather than walking the store. Staging counts as abandoned once it is an hour old, so
    `trucker gc --all` keeps younger staging, the lock files and the log. It removes each directory of memos whole,
    without reading the metadata of each memo. A memo or another small file beside the items counts the blocks that it
    takes, which can be 128 times its length.
  - `duplicates.rs` is the report of `trucker stats --duplicates`.
  - `windows.rs` holds the Windows twins of the Unix system calls, and `windows_scan.rs` the scan for baked paths on
    Windows, which the tests run everywhere.

### Windows

Each Unix item stays as it is under `#[cfg(unix)]`, and its Windows twin sits beside it under `#[cfg(windows)]`, so a
change for Windows never changes the Unix build.

  - Windows has no slots, carried sessions or candidates, because own crates are not cached there.
  - A root that Windows could spell another way, as through a `subst` or network drive, runs as
    `passthrough path-spelling`.
  - No hash is remembered, because a process can set back the change time of a file on Windows.
  - The tests of `tests.rs` run only on Unix, because their stand-in rustc is a shell script. A test that runs on
    Windows too sits in a module of its own, as those of `windows_scan.rs` do.

### Own crates

An own crate is stored only where every file that rustc read can be hashed, and no source of the worktree changed
during the compile or in the second before it. An output that contains the path of its worktree is not stored either.

A proc macro may read a file without declaring it, as sqlx reads `.sqlx`. So an own crate that can load a proc macro is
also selected by the files of its package, the hidden files of its workspace root, and the directories of its declared
files. The walks skip what version control, editors and tools own, and any nested worktree, package or target
directory.

  - An own crate that loads sqlx-macros compiles uncached unless SQLX_OFFLINE is set, because sqlx otherwise queries a
    database.
  - An own crate built with `-C instrument-coverage` runs as plain rustc would, because a remap breaks the paths that
    cargo-llvm-cov matches.
  - The first compile of a unit in a target directory is stored at once. A rebuild, as after an edit, is stored lazily
    through `lazy.rs`, which checks the hash and mode of each output before promoting it.

### Path dependencies outside the workspace root

cargo hashes the absolute path of a path package outside the workspace root into its `-C metadata` and
`-C extra-filename`, and both pass into every dependent. Such crates are cached per worktree, because no rewrite shares
them soundly.

  - A `-C metadata` without the path changes the StableCrateId, so symbols, TypeIds and dependents differ from what
    cargo's compile gives. A wrapper cannot prove such a value unique, because cargo hashes inputs that rustc never sees.
  - An entry that is keyed without the path and restored under cargo's names carries the StableCrateId of another
    worktree, which a fresh compile would not give.
  - A member outside the root, `package.workspace` and `[patch]` keep the path. From cargo 1.101, `trim-paths` drops it
    from `-C metadata` but not from `-C extra-filename`.

Under `trim-paths`, a non-incremental rlib and rmeta then differ between worktrees only by the string of
`-C extra-filename`, which a substitution of equal length could restore exactly. A linked output cannot be restored so,
because a linker may hash its contents into a build id or `LC_UUID`.

### Build scripts that bake OUT_DIR

A third-party build script that leaves the target directory's path in a binary, as a vendored OpenSSL does, runs again
with OUT_DIR at a fixed path in the store. Its outputs are rewritten through the roots and moved beside cargo's OUT_DIR,
and the fixed directory is removed. A marker sends later misses of the key straight to that run.

  - A script of a package that declares `links` runs at the fixed OUT_DIR first, outside Windows.
  - Where the retry fails, still bakes the path, writes outside its OUT_DIR or changes a file that the key covers, the
    run in cargo's environment is what cargo gets, and the key is marked `never` until gc removes the marker.

### Symlinks in OUT_DIR

A build script may leave symlinks in OUT_DIR, as `cxx-build` does. An entry lists them, and a replay writes them after
the files. A link may name a path in OUT_DIR, or in a registry source or git checkout, which never changes once
unpacked. Every other link is refused as `passthrough symlink`.

### Incremental sessions

rustc reuses a session only where the working directory and the paths in its arguments equal those of the compile that
wrote it. So the incremental compile of an own crate runs in a slot, a directory of the store whose path replaces the
worktree's in the arguments and the environment.

  - Each crate has a pool of slots. A compile locks a free one, preferring the slot that its worktree's session names,
    then the one with the newest session. Where every slot with a session is busy, it waits only as long as a carried
    compile has saved before, and otherwise starts cold.
  - A compile locks its slot after the lock of its inputs, and takes no lock while it holds a slot, so no wait closes a
    cycle.
  - A worktree starts from a clone or copy of the slot's recorded session, used only where each file still has the
    recorded length and mtime. The pool also keeps a snapshot, which nothing writes in place. A session is shared only
    so, because rustc writes the objects of a session in place.
  - A session that crashes rustc is deleted, and the crate is compiled once more without it.
  - An output that names the slot, as one that bakes `env!("CARGO_MANIFEST_DIR")` does, is compiled again in the
    worktree and marked so that its crate is not carried again.
  - cargo receives no line of stderr twice, because it panics at a second artifact notification.
  - A carried compile that links reads its libraries in the slot. Linux hard-links them, so that worktrees share
    their page cache, and macOS clones them. `<number>.libraries` lists the extent of each clone, so gc counts a block
    that slots share once.

A compile is not carried where cargo runs rustc from outside the worktree, where the target directory lies outside it,
or under local ThinLTO, which rewrites the previous session in place.
