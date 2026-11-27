//! A `RUSTC_WRAPPER` that keeps the outputs of third-party and own crates in a store shared by every worktree.
//!
//! A crate of the registry, of a git dependency or of a git worktree is compiled once per key and cloned into each
//! later build. A key is as sound as cargo's own rebuild checks, which miss a file or variable that a proc macro or
//! build script reads without declaring it, and also covers the files of the package of an own crate. An invocation
//! whose outputs cannot be predicted from a key runs rustc uncached, and one by a rustc older than 1.100 runs it
//! untouched. The wrapper also stands in for each third-party build script that it compiles, and replays the runs of
//! the script.

mod duplicates;
mod gc;
#[cfg(unix)]
mod lazy;
mod missed;
mod script;
#[cfg(unix)]
mod session;
mod store;
#[cfg(test)]
#[cfg(unix)]
mod tests;
mod unit;
mod usage;
#[cfg(windows)]
mod windows;
#[cfg(any(windows, test))]
mod windows_scan;

#[cfg(windows)]
use windows::{lazy, session};

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::io::{BufRead, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio, exit};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use std::{env, fs, io};

use unit::Unit;

fn main() {
    script::dispatch();
    session::read();
    session::link();
    let args = Vec::from_iter(env::args_os().skip(1));
    let Some((rustc, rest)) = args.split_first() else { usage() };
    let text = rest.iter().map(|arg| arg.to_str().map(str::to_string)).collect::<Option<Vec<_>>>();
    match (rustc.to_str(), text) {
        (Some("--version" | "-V"), Some(rest)) if rest.is_empty() => print!("{}", version()),
        (Some("stats"), Some(rest)) => {
            print!("{}", stats(&rest, &store_directory()).unwrap_or_else(|| usage()))
        }
        (Some("missed"), Some(rest)) => {
            print!("{}", missed::report(&rest, &store_directory()).unwrap_or_else(|| usage()))
        }
        (Some("gc"), Some(rest)) if rest.is_empty() || rest == ["--all"] => {
            print!("{}", gc::run(&store_directory(), !rest.is_empty(), true))
        }
        // The run that a miss starts, which leaves the store to a run already going.
        (Some("gc"), Some(rest)) if rest == ["--if-idle"] => {
            print!("{}", gc::run(&store_directory(), false, false))
        }
        (Some(word), _) if !is_program(word) => usage(),
        (Some(rustc), Some(rest)) => {
            exit(wrap(rustc, &rest, &store_directory(), unit::cargo_home()))
        }
        _ => exit(replace(Command::new(rustc).args(rest), rustc.as_ref())),
    }
}

/// Whether `word` can name the rustc that cargo passes, as a path or as a program on `PATH`.
///
/// Any other word is a mistyped subcommand, such as `trucker status`, which would otherwise run as a rustc that cannot
/// be found.
fn is_program(word: &str) -> bool {
    let on_path = |directory: PathBuf| {
        let program = directory.join(word);
        program.is_file() || (cfg!(windows) && program.with_extension("exe").is_file())
    };
    word.contains(['/', std::path::MAIN_SEPARATOR])
        || env::var_os("PATH").is_some_and(|path| env::split_paths(&path).any(on_path))
}

fn usage() -> ! {
    eprintln!("usage: RUSTC_WRAPPER=trucker cargo build");
    eprintln!(
        "       trucker stats [--since <seconds since 1970, or an age such as 90s, 15m, 2h or 3d>]"
    );
    eprintln!(
        "       trucker missed [--since <a time as stats takes it>] [--duplicates | <crate>]"
    );
    eprintln!("       trucker gc [--all]");
    eprintln!("       trucker --version");
    exit(2)
}

/// The version of trucker and the start of the hash of its binary, which tells apart two builds of one version.
fn version() -> String {
    let hash = env::current_exe().and_then(|wrapper| unit::file_hash(&wrapper));
    let hash = hash.map_or_else(|_| "unknown".to_string(), |hash| hash.to_hex()[..12].to_string());
    format!("trucker {} ({hash})\n", env!("CARGO_PKG_VERSION"))
}

fn report(rustc: &Path, error: io::Error) -> i32 {
    eprintln!("trucker could not run {}: {error}", rustc.display());
    1
}

/// Replaces this process with `command`, which runs `program`, and returns an exit code only where that fails.
#[cfg(unix)]
fn replace(command: &mut Command, program: &Path) -> i32 {
    report(program, command.exec())
}

/// Runs `command`, which runs `program`, and returns its exit code, because Windows cannot replace a process.
#[cfg(windows)]
fn replace(command: &mut Command, program: &Path) -> i32 {
    windows::run(command).unwrap_or_else(|error| report(program, error))
}

fn store_directory() -> PathBuf {
    env::var_os("XDG_CACHE_HOME").map_or_else(cache_home, PathBuf::from).join("trucker")
}

#[cfg(unix)]
fn cache_home() -> PathBuf {
    PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".cache")
}

#[cfg(windows)]
use windows::cache_home;

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs())
}

/// Runs rustc for one invocation, through `store` where its crate is third-party or own, and returns its exit code.
///
/// Every invocation that [`Unit::model`] reads is compiled with [`Unit::remap`] whether or not it is cached, so the
/// inputs of its dependents agree between worktrees.
fn wrap(rustc: &str, args: &[String], store: &Path, cargo_home: String) -> i32 {
    let started = Instant::now();
    let _ = fs::create_dir_all(store.join("entries"));
    store::mark(store);
    let untouched = || replace(Command::new(rustc).args(args), rustc.as_ref());
    if unit::is_query(args) {
        return untouched();
    }
    if !unit::compiler(rustc, store).is_ok_and(|version| unit::is_supported(&version)) {
        notice(store, "version", "runs rustc untouched, because it needs Rust 1.100 or later");
        return untouched();
    }
    if gc::setting().is_some_and(|setting| gc::bytes(&setting).is_none()) {
        let variable = gc::SIZE_VARIABLE;
        notice(
            store,
            "size",
            &format!("ignores {variable}, which is not a size such as 200G or 1.5T"),
        );
    }
    let source = args.iter().find(|arg| arg.ends_with(".rs") && !arg.starts_with('-'));
    let input = source.and_then(|input| Some(unit::lexical(&env::current_dir().ok()?.join(input))));
    let local = input.filter(|input| !unit::is_third_party(input, &cargo_home));
    // An own crate on Windows runs as plain rustc would, without a remap, a slot or a candidate.
    #[cfg(windows)]
    if local.is_some() {
        let code = replace(Command::new(rustc).args(args), rustc.as_ref());
        log(store, crate_name(args), started, "passthrough windows-own-crate");
        return code;
    }
    let worktree = local.as_deref().and_then(|input| unit::worktree(input, &cargo_home));
    let unit = Unit::model(args, cargo_home, env::var("OUT_DIR").ok(), worktree.clone());
    let remap = unit.as_ref().map_or_else(|_| Vec::new(), Unit::remap);
    let outcome = match unit {
        _ if local.is_some() && worktree.is_none() => Err("no-repository"),
        Ok(unit) => cached(rustc, args, store, &unit, &mut io::stderr()).inspect(|(outcome, _)| {
            if outcome.starts_with("hit") {
                session::read_later(&unit, store);
            }
        }),
        Err(reason) => Err(reason),
    };
    let (outcome, code) = outcome.unwrap_or_else(|reason| {
        let status = Command::new(rustc).args(args).args(&remap).status();
        let code = status.map_or_else(
            |error| report(rustc.as_ref(), error),
            |status| status.code().unwrap_or(1),
        );
        (format!("passthrough {reason}"), code)
    });
    log(store, crate_name(args), started, &outcome);
    if let Some(note) = note(source.map(String::as_str), worktree.is_some(), &outcome) {
        log(store, crate_name(args), started, note);
    }
    if outcome.starts_with("miss") {
        let _ = env::current_exe().map(|wrapper| gc::trigger(store, &wrapper));
    }
    code
}

fn crate_name(args: &[String]) -> &str {
    let name = args.iter().position(|arg| arg == "--crate-name").and_then(|at| args.get(at + 1));
    name.map_or("-", String::as_str)
}

/// The note that follows the outcome of an invocation in the log, which says why an own crate misses in each worktree.
///
/// cargo names the source of a package under the workspace root relative to that root. It names any other path
/// package by its absolute path, which it also hashes into `-C metadata`, so that crate and its dependents get a key of
/// their own in each worktree. The note follows only a miss, which is once per crate and build.
fn note(source: Option<&str>, own: bool, outcome: &str) -> Option<&'static str> {
    let absolute = source.is_some_and(|source| Path::new(source).is_absolute());
    (own && absolute && outcome.starts_with("miss")).then_some("note outside-workspace")
}

/// Appends the outcome of one invocation named `name` to the log in `store`.
fn log(store: &Path, name: &str, started: Instant, outcome: &str) {
    let line = format!("{} {name} {} {outcome}\n", now(), started.elapsed().as_millis());
    let log = fs::OpenOptions::new().create(true).append(true).open(store.join("log"));
    let _ = log.and_then(|mut log| log.write_all(line.as_bytes()));
}

/// Restores the outputs of one invocation from `store`, or runs rustc and stores them, and returns the outcome.
///
/// `cargo` receives the stderr of rustc with no line twice, because cargo marks the metadata of a unit ready at each
/// artifact notification and panics at a second one. Fails with the reason, before rustc has run, where the
/// invocation cannot be cached.
fn cached(
    rustc: &str,
    args: &[String],
    store: &Path,
    unit: &Unit,
    cargo: &mut dyn Write,
) -> Result<(String, i32), &'static str> {
    if !store::same_filesystem(store, &unit.out_dir) {
        let reason = "its store and the target directory are on different filesystems";
        notice(store, "notice", &format!("compiles every crate uncached, because {reason}"));
        return Err("other-filesystem");
    }
    if unit.reads_undeclared() {
        return Err("undeclared-input");
    }
    let (entries, files) = (store.join("entries"), store::Files::probe(store));
    let key = unit.key(rustc, args, store, env::vars_os()).map_err(|_| "unreadable-input")?;
    let (key, parts) = (key.hex, key.parts);
    let variant = |started| {
        let reads = fs::read_to_string(store::reads_of(&entries, &key)).unwrap_or_default();
        unit.variant(&key, &reads, started, &store.join("hashes"))
    };
    let current_entry = || variant(None).ok().map(|name| entries.join(name));
    // A compile that starts now from a source that changed within the last second is not stored.
    let found = variant(Some(SystemTime::now()));
    let settled = found != Err("changed-source");
    let mut entry =
        if settled { found.ok().map(|name| entries.join(name)) } else { current_entry() };
    // A process that finds no entry compiles under the lock of the entry, or of the key where the reads that select an
    // entry are unknown. Every process with the same inputs then restores the bytes that the first one stores, and
    // a process with other sources does not wait. A compile that will not be stored takes no lock.
    let waited = !entry.as_ref().is_some_and(|entry| entry.is_dir());
    let inputs = entry.as_ref().and_then(|entry| entry.file_name()?.to_str()).unwrap_or(&key);
    let _lock = (waited && settled).then(|| store::lock(store, inputs, crate_name(args))).flatten();
    // An own crate is stored as a candidate, which a miss promotes into the entry.
    let lazy = unit.roots.worktree.is_some() && !crate_name(args).starts_with("build_script_");
    let mut promoted = false;
    if waited {
        entry = current_entry();
        if let Some(entry) = entry.as_ref().filter(|entry| lazy && !entry.is_dir()) {
            promoted = lazy::promote(&entries, &key, entry, files);
        }
    }
    // An `.rmeta` stays, because rustc and restore keep an unchanged one so that cargo does not rebuild dependents.
    let metadata = |output: &&PathBuf| output.extension().is_some_and(|kind| kind == "rmeta");
    // An own crate is stored at once where this is its first compile in the target directory, and lazily in a rebuild,
    // whose outputs after an edit no other worktree is likely to want. The kept `.rmeta` does not count as an output.
    let rebuild =
        unit.outputs().is_ok_and(|outputs| outputs.iter().any(|output| !metadata(&output)));
    let lazy = lazy && rebuild;
    let clear = || {
        let outputs = unit.outputs()?;
        outputs.iter().filter(|output| !metadata(output)).try_for_each(fs::remove_file)
    };
    clear().map_err(|_| "unremovable-output")?;
    // A build script is published before its stand-in is installed, so that an entry never contains the stand-in.
    let install = || unit.script().map(|script| script::install(&script, store, files));
    let restored = entry.as_ref().filter(|entry| entry.is_dir()).map(|entry| {
        let restored = store::restore(unit, entry, files, cargo);
        restored.inspect(|()| store::touch(&entries, &key, entry))
    });
    if restored == Some(Ok(())) {
        install();
        let hit = if promoted {
            "hit promoted"
        } else if waited {
            "hit waited"
        } else {
            "hit"
        };
        return Ok((hit.to_string(), 0));
    }
    clear().map_err(|_| "unremovable-output")?;
    let (compiling, started) = (Instant::now(), SystemTime::now());
    let mut slot = session::Slot::take(unit, args, store, files);
    let (mut forwarded, running) = (HashSet::new(), Instant::now());
    let mut compiled = compile(rustc, args, unit, slot.as_ref(), cargo, &mut forwarded)?;
    let ran = running.elapsed();
    // A session that crashes rustc is deleted, and the crate is compiled once more without it. Where the session cannot
    // be deleted, that compile runs in the worktree.
    if slot
        .as_ref()
        .is_some_and(|slot| slot.restored && compiled.code.is_none_or(|code| code == 101))
    {
        if slot.as_mut().is_some_and(|slot| slot.discard().is_err()) {
            slot = None;
        }
        compiled = compile(rustc, args, unit, slot.as_ref(), cargo, &mut forwarded)?;
    }
    let Compiled { code, stderr, held } = compiled;
    let outputs = unit.outputs();
    let (dep_info, scanned): (Vec<_>, Vec<_>) = outputs
        .iter()
        .flatten()
        .cloned()
        .partition(|output| output.extension().is_some_and(|kind| kind == "d"));
    // Each output is read once, for the hash that a candidate records and for the scan for the slot.
    let digests = (code == Some(0) && (lazy || slot.is_some()))
        .then(|| lazy::digests(&scanned, slot.as_ref().map(|slot| slot.path.as_str()).as_slice()));
    // An output that names the slot, or cannot be read, is compiled again in the worktree, because the slot later
    // mirrors another one.
    let baked =
        digests.as_ref().is_some_and(|digests| digests.as_ref().is_none_or(|(_, found)| *found));
    if let Some(slot) = slot.as_mut().filter(|_| baked) {
        slot.bake();
        if forwarded.iter().any(|line| is_notification(line, "metadata")) {
            return Ok(late_bake(unit, crate_name(args), cargo));
        }
        let code = compile(rustc, args, unit, None, cargo, &mut forwarded)?.code.unwrap_or(1);
        return Ok(("passthrough baked-slot".to_string(), code));
    }
    let earlier = std::mem::take(&mut forwarded);
    for line in held.into_iter().flatten() {
        forward(cargo, &earlier, &mut forwarded, line);
    }
    let code = code.unwrap_or(1);
    if let Some(slot) = &slot {
        if localize_dep_info(&dep_info, slot).is_err() {
            // A dep-info that names the slot would make cargo check another worktree's files, so the crate compiles
            // again in the worktree, which writes its own.
            let code = compile(rustc, args, unit, None, cargo, &mut forwarded)?.code.unwrap_or(1);
            return Ok(("passthrough dep-info".to_string(), code));
        }
        if code == 0 {
            slot.time(ran);
            if let Err(error) = slot.keep()
                && !matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::InvalidData)
            {
                log(
                    store,
                    crate_name(args),
                    compiling,
                    &format!("note unkept-session {}", error.kind()),
                );
            }
        }
    }
    let stored = if code == 0 {
        let record = duplicates::record(unit, args, compiling.elapsed().as_millis());
        let slot = slot.as_ref().map(|slot| slot.path.as_str());
        // Outputs that cannot be listed leave no hashes, so that `publish` lists them again and fails.
        let hashes = digests.flatten().filter(|_| outputs.is_ok()).map(|(hashes, _)| hashes);
        let outputs = (&dep_info[..], &scanned[..], hashes);
        match lazy
            .then(|| lazy::record(unit, &entries, &key, (&stderr, &record), outputs, started, slot))
        {
            None | Some(Err("eager")) => {
                store::publish(unit, &entries, &key, &stderr, &record, started, files)
            }
            Some(recorded) => recorded,
        }
    } else {
        Err("rustc-failed")
    };
    if stored.is_ok() {
        install();
        // The memo names the entry that the reads now stored select, so that a later miss of equal parts and sources
        // finds that entry gone.
        let variant =
            current_entry().and_then(|entry| Some(entry.file_name()?.to_str()?.to_string()));
        let miss = missed::Miss::of(unit, &parts, &variant.unwrap_or_default());
        if let Some(note) = miss.note(&unit.stem, &store.join("hashes")) {
            log(store, crate_name(args), compiling, &note);
        }
        miss.remember(&store.join("hashes"));
    } else if stored == Err("clone-failed") {
        notice(store, "notice", "compiles a crate uncached, because its store refused a clone");
    }
    let miss = match restored {
        Some(Err("evicted")) => "miss evicted",
        _ if slot.is_some_and(|slot| slot.restored) => "miss session",
        _ => "miss",
    };
    Ok((stored.map_or_else(|reason| format!("passthrough {reason}"), |()| miss.to_string()), code))
}

/// The exit code and stderr of one run of rustc, and the lines of stderr that it held back from cargo.
struct Compiled {
    code: Option<i32>,
    stderr: Vec<u8>,
    /// The lines after the dep-info notification of a compile in a slot whose dep-info reads a variable that names the
    /// slot.
    ///
    /// Such a crate is likely to be compiled again. cargo must not start a dependent from the metadata of the slot,
    /// because its hash differs from that of the compile that stands. It is `None` where nothing was held back.
    held: Option<Vec<Vec<u8>>>,
}

/// Runs rustc for `unit`, in `slot` where there is one, and returns its exit code and stderr.
///
/// Each line of stderr goes to `cargo` through [`forward`] as it arrives, so cargo starts the dependents once the
/// metadata is written. A line is skipped where `forwarded` records that an earlier run of the same invocation passed
/// it on, or held back into [`Compiled::held`].
fn compile(
    rustc: &str,
    args: &[String],
    unit: &Unit,
    slot: Option<&session::Slot>,
    cargo: &mut dyn Write,
    forwarded: &mut HashSet<Vec<u8>>,
) -> Result<Compiled, &'static str> {
    let mut command = Command::new(rustc);
    let args = [args, &unit.remap()].concat();
    match slot {
        Some(slot) => command
            .args(slot.args(&args))
            .envs(slot.environment(&args))
            .current_dir(&slot.directory),
        None => command.args(args).current_dir(&unit.directory),
    };
    let mut child = command.stderr(Stdio::piped()).spawn().map_err(|_| "rustc-not-started")?;
    let (mut stderr, mut line, mut held) = (Vec::new(), Vec::new(), None::<Vec<Vec<u8>>>);
    let earlier = std::mem::take(forwarded);
    let mut pipe = child.stderr.take().map(io::BufReader::new);
    while pipe
        .as_mut()
        .and_then(|pipe| pipe.read_until(b'\n', &mut line).ok())
        .is_some_and(|read| read > 0)
    {
        if let Some(slot) = slot {
            line = slot.localized(&String::from_utf8_lossy(&line)).into_bytes();
        }
        stderr.extend_from_slice(&line);
        let dep_info = is_notification(&line, "dep-info");
        match &mut held {
            Some(held) => held.push(std::mem::take(&mut line)),
            None => forward(cargo, &earlier, forwarded, std::mem::take(&mut line)),
        }
        if dep_info && slot.is_some_and(|slot| slot.reads_itself(unit)) {
            held.get_or_insert_default();
        }
    }
    forwarded.extend(earlier);
    Ok(Compiled { code: child.wait().ok().and_then(|status| status.code()), stderr, held })
}

/// Passes `line` on to `cargo` unless `earlier` shows that an earlier run passed it on, and records it in `forwarded`.
fn forward(
    cargo: &mut dyn Write,
    earlier: &HashSet<Vec<u8>>,
    forwarded: &mut HashSet<Vec<u8>>,
    line: Vec<u8>,
) {
    if !earlier.contains(&line) {
        let _ = cargo.write_all(&line);
        forwarded.insert(line);
    }
}

/// Whether `line` is rustc's notification that the output of the kind `emit` is written.
///
/// rustc writes the dep-info before the metadata, and cargo starts the dependents of a library at the notification of
/// its metadata.
fn is_notification(line: &[u8], emit: &str) -> bool {
    let line = String::from_utf8_lossy(line);
    line.starts_with('{')
        && line.contains("\"artifact\":")
        && line.contains(&format!("\"emit\":\"{emit}\""))
}

/// Fails the compile of `unit` in a slot whose output names the slot, after cargo was told that its metadata was
/// written, and returns the outcome.
///
/// A dependent may already have started from that metadata, and a compile in the worktree would give its library
/// another hash. So the outputs are removed instead, and cargo compiles the crate and then its dependents in the next
/// build. The crate is never carried again.
fn late_bake(unit: &Unit, name: &str, cargo: &mut dyn Write) -> (String, i32) {
    for output in unit.outputs().unwrap_or_default() {
        let _ = fs::remove_file(output);
    }
    let message = format!(
        "trucker failed {name}, whose output names the directory that it was compiled in, after its dependents could \
         start, so the next build compiles it again in the worktree\n"
    );
    let _ = cargo.write_all(message.as_bytes());
    ("passthrough baked-slot-late".to_string(), 1)
}

/// Rewrites the `dep_info` that rustc wrote in `slot` to name the paths of the worktree.
///
/// Each file is written beside itself and renamed into place, so cargo never reads a part of one.
fn localize_dep_info(dep_info: &[PathBuf], slot: &session::Slot) -> io::Result<()> {
    for path in dep_info {
        let text = match fs::read_to_string(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            text => text?,
        };
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let staging = path.with_file_name(format!(".{name}.{}", std::process::id()));
        let written =
            fs::write(&staging, slot.localized(&text)).and_then(|()| fs::rename(&staging, path));
        if written.is_err() {
            let _ = fs::remove_file(&staging);
            return written;
        }
    }
    Ok(())
}

/// Says `message` on stderr once per build and `kind` of notice.
///
/// A file in `store` named after `kind` and the process of cargo marks the build that was told, until gc removes it.
fn notice(store: &Path, kind: &str, message: &str) {
    #[cfg(unix)]
    let parent = std::os::unix::process::parent_id();
    #[cfg(windows)]
    let parent = windows::parent_id();
    let marker = store.join("notices").join(format!("{kind}-{parent}"));
    let _ = fs::create_dir_all(store.join("notices"));
    if fs::File::create_new(marker).is_ok() {
        eprintln!("trucker {message}");
    }
}

/// The time that `--since` names, in seconds since 1970.
fn since(time: &str) -> Option<u64> {
    let ago = |unit: char, seconds: u64| {
        Some(now().saturating_sub(time.strip_suffix(unit)?.parse::<u64>().ok()? * seconds))
    };
    let ages = [('s', 1), ('m', 60), ('h', 3600), ('d', 86400)];
    time.parse().ok().or_else(|| ages.iter().find_map(|(unit, seconds)| ago(*unit, *seconds)))
}

/// The start of the outcomes that `trucker stats` lists before those of the next start, so that hits come first.
///
/// The empty start takes every other outcome, such as a note.
const ORDER: [&str; 4] = ["hit", "miss", "passthrough ", ""];

/// The plain description of each outcome that the log can hold, which `trucker stats` prints after its code.
///
/// The table of README.md describes each one more fully. A note of `note unkept-session` ends with the kind of its
/// error, which the code keeps.
const OUTCOMES: &[(&str, &str)] = &[
    ("hit", "restored from cache"),
    ("hit waited", "waited for another build, then restored from cache"),
    ("hit promoted", "cached an earlier rebuild after checking it, then restored"),
    ("miss", "compiled fresh and cached"),
    ("miss session", "compiled from earlier work and cached"),
    ("miss evicted", "compiled and cached again: its cached copy had changed"),
    ("passthrough rustc-failed", "rustc failed, nothing cached"),
    ("passthrough changed-source", "compiled, not cached: a source file just changed"),
    ("passthrough baked-path", "compiled, not cached: output contains this worktree's path"),
    ("passthrough baked-slot", "compiled twice, not cached: output named a cache folder"),
    ("passthrough baked-slot-late", "build failed, run it again: output named a cache folder"),
    ("passthrough dep-info", "compiled twice, not cached: could not rewrite its source list"),
    ("passthrough unkeyed-input", "ran, not cached: read a file that the cache cannot track"),
    ("passthrough undeclared-input", "compiled, not cached: sqlx macros can query a database"),
    ("passthrough unreadable-read", "compiled, not cached: a file that it read is unreadable"),
    ("passthrough unreadable-input", "ran, not cached: an input is unreadable"),
    ("passthrough no-source-read", "compiled, not cached: read no source in its worktree"),
    ("passthrough no-repository", "compiled, not cached: crate is not in a git repository"),
    ("passthrough windows-own-crate", "compiled, not cached: your crates on Windows"),
    ("passthrough coverage", "compiled, not cached: built for code coverage"),
    ("passthrough incremental", "compiled, not cached: a dependency built incrementally"),
    ("passthrough argument-file", "compiled, not cached: arguments come from a file"),
    ("passthrough explicit-output", "compiled, not cached: output file named with -o"),
    ("passthrough emit", "compiled, not cached: asks for outputs that are not cached"),
    ("passthrough no-crate-name", "compiled, not cached: arguments name no crate"),
    ("passthrough no-out-dir", "ran, not cached: no output folder given"),
    ("passthrough no-target-dir", "ran, not cached: output folder is outside cargo's layout"),
    ("passthrough escaped-path", "compiled, not cached: a path has a quote or control character"),
    ("passthrough path-spelling", "ran, not cached: Windows can spell a path two ways"),
    ("passthrough no-working-directory", "ran, not cached: current folder is unreadable"),
    ("passthrough other-filesystem", "ran, not cached: cache is on another disk"),
    ("passthrough clone-failed", "ran, not cached: the disk refused to clone a file"),
    ("passthrough store-error", "ran, not cached: could not write to the cache"),
    ("passthrough unremovable-output", "ran, not cached: could not remove an old output"),
    ("passthrough rustc-not-started", "rustc did not start, nothing cached"),
    ("passthrough no-stand-in", "build script not cached: could not set up its replay"),
    ("passthrough script-not-started", "build script did not start, nothing cached"),
    ("passthrough script-failed", "build script failed, nothing cached"),
    ("passthrough binary-output", "build script ran, not cached: it printed non-text"),
    ("passthrough binary-target-path", "build script ran, not cached: left its path in a binary"),
    ("passthrough fixed-run-failed", "build script ran, not cached: failed in a fixed folder"),
    ("passthrough script-escaped", "build script ran, not cached: wrote outside its folder"),
    ("passthrough unmovable-output", "build script ran, not cached: could not move its output"),
    ("passthrough symlink", "build script ran, not cached: left a link that cannot be cached"),
    ("passthrough windows-fixed", "build script ran, not cached: left its path, on Windows"),
    (
        "passthrough unnormalized-path",
        "build script ran, not cached: a path is spelled another way",
    ),
    (
        "note outside-workspace",
        "note on a miss: crate outside the workspace is cached per worktree",
    ),
    ("note unkept-session", "note on a compile: its work could not be shared with other builds"),
];

/// The description of `outcome` in [`OUTCOMES`], where it has one.
fn described(outcome: &str) -> Option<&'static str> {
    let matches = |code: &str| {
        outcome == code
            || code.starts_with("note ")
                && outcome.strip_prefix(code).is_some_and(|kind| kind.starts_with(' '))
    };
    OUTCOMES.iter().find(|(code, _)| matches(code)).map(|(_, description)| *description)
}

/// The count, the total time, the code and the description of each outcome in the log of `store`, then the share of
/// the hits and misses that hit, and the cap of the store.
///
/// It is `None` where `args` are not valid.
fn stats(args: &[String], store: &Path) -> Option<String> {
    let start = match args {
        [] => 0,
        [flag, time] if flag == "--since" => since(time)?,
        _ => return None,
    };
    let (mut totals, mut text) = (BTreeMap::<String, (u64, u64)>::new(), String::new());
    // The size that the last gc left, with the cap that it applied where its line records one.
    let mut last = None;
    for line in fs::read_to_string(store.join("log")).unwrap_or_default().lines() {
        let fields = Vec::from_iter(line.splitn(4, ' '));
        if let [_, _, _, outcome] = fields[..]
            && let Some(collected) = outcome.strip_prefix("gc ")
        {
            let numbers =
                Vec::from_iter(collected.split(' ').map(|field| field.parse::<u64>().ok()));
            if let Some(&Some(size)) = numbers.get(2) {
                let applied = match numbers.get(4..6) {
                    Some(&[Some(cap), Some(set)]) => Some((cap, set == 1)),
                    _ => None,
                };
                last = Some((size, applied));
            }
        } else if let [time, _, milliseconds, outcome] = fields[..]
            && time.parse::<u64>().is_ok_and(|time| time >= start)
            && !outcome.starts_with(missed::NOTE)
        {
            let total = totals.entry(outcome.to_string()).or_default();
            *total = (total.0 + 1, total.1 + milliseconds.parse::<u64>().unwrap_or(0));
        }
    }
    let rank = |outcome: &str| ORDER.iter().position(|start| outcome.starts_with(start));
    let mut rows = Vec::from_iter(&totals);
    rows.sort_by(|(a, (_, a_time)), (b, (_, b_time))| {
        rank(a).cmp(&rank(b)).then(b_time.cmp(a_time)).then(a.cmp(b))
    });
    let width = rows.iter().map(|(outcome, _)| outcome.len()).max().unwrap_or(0);
    for (outcome, (count, milliseconds)) in rows {
        let line = described(outcome).map_or_else(
            || outcome.to_string(),
            |description| format!("{outcome:<width$} | {description}"),
        );
        let _ = writeln!(text, "{count:>7} {:>9.1} s  {line}", *milliseconds as f64 / 1000.0);
    }
    if !totals.is_empty() {
        let _ = writeln!(
            text,
            "Each time is the total that trucker took, to restore for a hit and to compile otherwise."
        );
    }
    let count = |outcome: &str| totals.get(outcome).map_or(0, |total| total.0);
    let misses = count("miss") + count("miss session") + count("miss evicted");
    let hits = count("hit") + count("hit waited") + count("hit promoted");
    if hits + misses > 0 {
        let share = 100.0 * hits as f64 / (hits + misses) as f64;
        let _ = writeln!(
            text,
            "{share:.1}% of the compiles that could be cached were restored from cache (hits {hits}, misses {misses})"
        );
    }
    let gigabytes = |bytes: u64| bytes as f64 / 1e9;
    let source = |set: bool| {
        if set { format!("from {}", gc::SIZE_VARIABLE) } else { "from the filesystem".to_string() }
    };
    // A build may set the cap through the environment of cargo, which this process does not see, so the cap of the
    // last gc is the one that applies.
    let _ = match last {
        Some((size, applied)) => {
            let (cap, set) = applied.unwrap_or_else(|| gc::store_cap(store));
            writeln!(
                text,
                "last gc: {:.1} GB of a {:.1} GB cap ({})",
                gigabytes(size),
                gigabytes(cap),
                source(set)
            )
        }
        None => {
            let (cap, set) = gc::store_cap(store);
            writeln!(text, "cap: {:.1} GB ({})", gigabytes(cap), source(set))
        }
    };
    Some(text)
}
