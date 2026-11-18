//! A `RUSTC_WRAPPER` that keeps the outputs of third-party and own crates in a store shared by every worktree.
//!
//! A crate of the registry, of a git dependency or of a git worktree is compiled once per key and cloned into each
//! later build. A key is as sound as cargo's own rebuild checks, which miss a file or variable that a proc macro or
//! build script reads without declaring it. An invocation whose outputs cannot be predicted from a key runs rustc
//! uncached, and one by a rustc older than 1.100 runs it untouched. The wrapper also stands in for each third-party
//! build script that it compiles, to replay the script's runs.

mod duplicates;
mod gc;
mod script;
mod store;
#[cfg(test)]
mod tests;
mod unit;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio, exit};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use std::{env, fs, io};

use unit::Unit;

fn main() {
    script::dispatch();
    let args = Vec::from_iter(env::args_os().skip(1));
    let Some((rustc, rest)) = args.split_first() else { usage() };
    let text = rest.iter().map(|arg| arg.to_str().map(str::to_string)).collect::<Option<Vec<_>>>();
    match (rustc.to_str(), text) {
        (Some("stats"), Some(rest)) => {
            print!("{}", stats(&rest, &store_directory()).unwrap_or_else(|| usage()))
        }
        (Some("gc"), Some(rest)) if rest.is_empty() => print!("{}", gc::run(&store_directory())),
        (Some(rustc), Some(rest)) => {
            exit(wrap(rustc, &rest, &store_directory(), unit::cargo_home()))
        }
        _ => exit(report(rustc.as_ref(), Command::new(rustc).args(rest).exec())),
    }
}

fn usage() -> ! {
    eprintln!("usage: RUSTC_WRAPPER=trucker cargo build");
    eprintln!(
        "       trucker stats [--duplicates] [--since <seconds since 1970, or an age such as 90s, 15m, 2h or 3d>]"
    );
    eprintln!("       trucker gc");
    exit(2)
}

fn report(rustc: &Path, error: io::Error) -> i32 {
    eprintln!("trucker could not run {}: {error}", rustc.display());
    1
}

fn store_directory() -> PathBuf {
    let home = || PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".cache");
    env::var_os("XDG_CACHE_HOME").map_or_else(home, PathBuf::from).join("trucker")
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs())
}

/// Runs rustc for one invocation, through `store` where its crate is third-party or own, and returns its exit code.
///
/// Every invocation that [`Unit::model`] reads is compiled with [`Unit::remap`], cached or not, so that the inputs of
/// its dependents agree between worktrees.
fn wrap(rustc: &str, args: &[String], store: &Path, cargo_home: String) -> i32 {
    let started = Instant::now();
    let _ = fs::create_dir_all(store.join("entries"));
    let untouched = || report(rustc.as_ref(), Command::new(rustc).args(args).exec());
    if unit::is_query(args) {
        return untouched();
    }
    if !unit::compiler(rustc, store).is_ok_and(|version| unit::is_supported(&version)) {
        notice(store, "version", "runs rustc untouched, because it needs Rust 1.100 or later");
        return untouched();
    }
    let input = args.iter().find(|arg| arg.ends_with(".rs") && !arg.starts_with('-'));
    let input = input.and_then(|input| Some(unit::lexical(&env::current_dir().ok()?.join(input))));
    let local = input.filter(|input| !unit::is_third_party(input, &cargo_home));
    let worktree = local.as_deref().and_then(|input| unit::worktree(input, &cargo_home));
    let unit = Unit::model(args, cargo_home, env::var("OUT_DIR").ok(), worktree.clone());
    let remap = unit.as_ref().map_or_else(|_| Vec::new(), Unit::remap);
    let outcome = match unit {
        _ if local.is_some() && worktree.is_none() => Err("no-repository"),
        Ok(unit) => cached(rustc, args, store, &unit),
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
    if outcome == "miss" {
        let _ = env::current_exe().map(|wrapper| gc::trigger(store, &wrapper));
    }
    code
}

fn crate_name(args: &[String]) -> &str {
    let name = args.iter().position(|arg| arg == "--crate-name").and_then(|at| args.get(at + 1));
    name.map_or("-", String::as_str)
}

/// Appends the outcome of one invocation named `name` to the log in `store`.
fn log(store: &Path, name: &str, started: Instant, outcome: &str) {
    let line = format!("{} {name} {} {outcome}\n", now(), started.elapsed().as_millis());
    let log = fs::OpenOptions::new().create(true).append(true).open(store.join("log"));
    let _ = log.and_then(|mut log| log.write_all(line.as_bytes()));
}

/// Restores the outputs of one invocation from `store`, or runs rustc and stores them, and returns the outcome.
///
/// Fails with the reason, before rustc has run, where the invocation cannot be cached.
fn cached(
    rustc: &str,
    args: &[String],
    store: &Path,
    unit: &Unit,
) -> Result<(String, i32), &'static str> {
    if !store::same_filesystem(store, &unit.out_dir) {
        let reason = "its store and the target directory are on different filesystems";
        notice(store, "notice", &format!("compiles every crate uncached, because {reason}"));
        return Err("other-filesystem");
    }
    let entries = store.join("entries");
    let key = unit.key(rustc, args, store).map_err(|_| "unreadable-input")?;
    let current_entry = || {
        let reads = fs::read_to_string(store::reads_of(&entries, &key)).unwrap_or_default();
        entries.join(unit.variant(&key, &reads))
    };
    let mut entry = current_entry();
    // A process that finds no entry compiles under the key's lock, so that every worktree stores the same bytes.
    let waited = !entry.is_dir();
    let _lock = waited.then(|| store::lock(store, &key, crate_name(args))).flatten();
    if waited {
        entry = current_entry();
    }
    // An `.rmeta` stays for rustc and restore, which keep an unchanged one, so that cargo does not rebuild dependents.
    let metadata = |output: &&PathBuf| output.extension().is_some_and(|kind| kind == "rmeta");
    let clear = || {
        let outputs = unit.outputs()?;
        outputs.iter().filter(|output| !metadata(output)).try_for_each(fs::remove_file)
    };
    clear().map_err(|_| "unremovable-output")?;
    // A build script is published before its stand-in is installed, so that an entry never contains the stand-in.
    let install = || unit.script().map(|script| script::install(&script, store));
    if entry.is_dir() && store::restore(unit, &entry).is_ok() {
        store::touch(&entries, &key, &entry);
        install();
        return Ok((if waited { "hit waited" } else { "hit" }.to_string(), 0));
    }
    clear().map_err(|_| "unremovable-output")?;
    let compiling = Instant::now();
    let mut child = Command::new(rustc)
        .args(args)
        .args(unit.remap())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| "rustc-not-started")?;
    let (mut stderr, mut chunk) = (Vec::new(), [0; 8192]);
    // Each chunk is passed on as it arrives, so that cargo starts the dependents once the metadata is written.
    while let Some(read) =
        child.stderr.as_mut().and_then(|pipe| pipe.read(&mut chunk).ok()).filter(|read| *read > 0)
    {
        let _ = io::stderr().write_all(&chunk[..read]);
        stderr.extend_from_slice(&chunk[..read]);
    }
    let code = child.wait().ok().and_then(|status| status.code()).unwrap_or(1);
    let stored = if code == 0 {
        let record = duplicates::record(unit, args, compiling.elapsed().as_millis());
        store::publish(unit, &entries, &key, &stderr, &record)
    } else {
        Err("rustc-failed")
    };
    if stored.is_ok() {
        install();
    } else if stored == Err("clone-failed") {
        let reason = "the filesystem of its store cannot clone files";
        notice(store, "notice", &format!("compiles every crate uncached, because {reason}"));
    }
    Ok((
        stored.map_or_else(|reason| format!("passthrough {reason}"), |()| "miss".to_string()),
        code,
    ))
}

/// Says `message` on stderr once per build and `kind` of notice.
///
/// A file in `store` named after `kind` and cargo's process marks the build that was told, until gc removes it.
fn notice(store: &Path, kind: &str, message: &str) {
    let marker =
        store.join("notices").join(format!("{kind}-{}", std::os::unix::process::parent_id()));
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

/// The count and the total time of each outcome in the log of `store`, then the share of cached invocations that hit.
///
/// With `--duplicates` it is the report of [`duplicates::report`] instead. It is `None` where `args` are not valid.
fn stats(args: &[String], store: &Path) -> Option<String> {
    let duplicates = args.first().is_some_and(|arg| arg == "--duplicates");
    let start = match &args[usize::from(duplicates)..] {
        [] => 0,
        [flag, time] if flag == "--since" => since(time)?,
        _ => return None,
    };
    if duplicates {
        return Some(duplicates::report(&store.join("entries"), start));
    }
    let (mut totals, mut text) = (BTreeMap::<String, (u64, u64)>::new(), String::new());
    for line in fs::read_to_string(store.join("log")).unwrap_or_default().lines() {
        let fields = Vec::from_iter(line.splitn(4, ' '));
        if let [time, _, milliseconds, outcome] = fields[..]
            && time.parse::<u64>().is_ok_and(|time| time >= start)
            && !outcome.starts_with("gc ")
        {
            let total = totals.entry(outcome.to_string()).or_default();
            *total = (total.0 + 1, total.1 + milliseconds.parse::<u64>().unwrap_or(0));
        }
    }
    for (outcome, (count, milliseconds)) in &totals {
        let _ = writeln!(text, "{count:>7} {:>9.1} s  {outcome}", *milliseconds as f64 / 1000.0);
    }
    let count = |outcome: &str| totals.get(outcome).map_or(0, |total| total.0);
    let hits = count("hit") + count("hit waited");
    if hits + count("miss") > 0 {
        let share = 100.0 * hits as f64 / (hits + count("miss")) as f64;
        let _ = writeln!(text, "{share:.1}% of the cached invocations hit");
    }
    Some(text)
}
