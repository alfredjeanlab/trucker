//! A `RUSTC_WRAPPER` that keeps the outputs of third-party crates in a store shared by every worktree.
//!
//! A crate of the registry or of a git dependency is compiled once per key and cloned into each later build. Every
//! other invocation runs rustc untouched, and one whose outputs cannot be predicted from a key runs it uncached.

mod store;
#[cfg(test)]
mod tests;
mod unit;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio, exit};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use std::{env, fs, io};

use unit::Unit;

fn main() {
    let args = Vec::from_iter(env::args_os().skip(1));
    let Some((rustc, rest)) = args.split_first() else { usage() };
    let text = rest.iter().map(|arg| arg.to_str().map(str::to_string)).collect::<Option<Vec<_>>>();
    match (rustc.to_str(), text) {
        (Some("stats"), Some(rest)) => stats(&rest),
        (Some(rustc), Some(rest)) => exit(wrap(rustc, &rest)),
        _ => exit(report(rustc.as_ref(), Command::new(rustc).args(rest).exec())),
    }
}

fn usage() -> ! {
    eprintln!("usage: RUSTC_WRAPPER=crate-cache cargo build");
    eprintln!(
        "       crate-cache stats [--since <seconds since 1970, or an age such as 90s, 15m, 2h or 3d>]"
    );
    exit(2)
}

fn report(rustc: &Path, error: io::Error) -> i32 {
    eprintln!("crate-cache could not run {}: {error}", rustc.display());
    1
}

fn store_directory() -> PathBuf {
    let home = || PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".cache");
    env::var_os("XDG_CACHE_HOME").map_or_else(home, PathBuf::from).join("crate-cache")
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs())
}

/// Runs rustc for one invocation, through the store where its crate is third-party, and returns its exit code.
fn wrap(rustc: &str, args: &[String]) -> i32 {
    let started = Instant::now();
    let cargo_home = unit::cargo_home();
    let input =
        args.iter().find(|arg| *arg == "-" || (arg.ends_with(".rs") && !arg.starts_with('-')));
    if input.is_some_and(|input| input != "-" && !unit::is_third_party(input, &cargo_home)) {
        return report(rustc.as_ref(), Command::new(rustc).args(args).exec());
    }
    let store = store_directory();
    let _ = fs::create_dir_all(store.join("entries"));
    let uncached = |reason| {
        let status = Command::new(rustc).args(args).status();
        (
            format!("passthrough {reason}"),
            status.map_or_else(
                |error| report(rustc.as_ref(), error),
                |status| status.code().unwrap_or(1),
            ),
        )
    };
    let (outcome, code) = cached(rustc, args, &store, cargo_home).unwrap_or_else(uncached);
    let name = args.iter().position(|arg| arg == "--crate-name").and_then(|at| args.get(at + 1));
    let line = format!(
        "{} {} {} {outcome}\n",
        now(),
        name.map_or("-", String::as_str),
        started.elapsed().as_millis()
    );
    let log = fs::OpenOptions::new().create(true).append(true).open(store.join("log"));
    let _ = log.and_then(|mut log| log.write_all(line.as_bytes()));
    code
}

/// Restores the outputs of one invocation from `store`, or runs rustc and stores them, and returns the outcome.
///
/// Fails with the reason, before rustc has run, where the invocation cannot be cached.
fn cached(
    rustc: &str,
    args: &[String],
    store: &Path,
    cargo_home: String,
) -> Result<(String, i32), &'static str> {
    let unit = Unit::model(args, cargo_home, env::var("OUT_DIR").ok())?;
    if !store::same_filesystem(store, &unit.out_dir) {
        notice(&unit, "its store and the target directory are on different filesystems");
        return Err("other-filesystem");
    }
    let entries = store.join("entries");
    let key = unit.key(rustc, args, store).map_err(|_| "unreadable-input")?;
    let reads = fs::read_to_string(store::reads_of(&entries, &key)).unwrap_or_default();
    let entry = entries.join(unit.variant(&key, &reads));
    let clear = || unit.outputs().and_then(|outputs| outputs.iter().try_for_each(fs::remove_file));
    clear().map_err(|_| "unremovable-output")?;
    if entry.is_dir() && store::restore(&unit, &entry).is_ok() {
        return Ok(("hit".to_string(), 0));
    }
    clear().map_err(|_| "unremovable-output")?;
    let mut child = Command::new(rustc)
        .args(args)
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
        store::publish(&unit, &entries, &key, &stderr)
    } else {
        Err("rustc-failed")
    };
    if stored == Err("clone-failed") {
        notice(&unit, "the filesystem of its store cannot clone files");
    }
    Ok((
        stored.map_or_else(|reason| format!("passthrough {reason}"), |()| "miss".to_string()),
        code,
    ))
}

/// Says on stderr, once per build, why the third-party crates are compiled uncached.
///
/// A file named after cargo's process, in the target directory, marks the build that was told.
fn notice(unit: &Unit, reason: &str) {
    let marker =
        format!("{}.crate-cache-notice-{}", unit.roots.target, std::os::unix::process::parent_id());
    if fs::File::create_new(marker).is_ok() {
        eprintln!("crate-cache compiles the third-party crates uncached, because {reason}");
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

/// Prints the count and the total time of each outcome in the log, then the share of cached invocations that hit.
fn stats(args: &[String]) {
    let start = match args {
        [] => 0,
        [flag, time] if flag == "--since" => since(time).unwrap_or_else(|| usage()),
        _ => usage(),
    };
    let mut totals = BTreeMap::<String, (u64, u64)>::new();
    for line in fs::read_to_string(store_directory().join("log")).unwrap_or_default().lines() {
        let fields = Vec::from_iter(line.splitn(4, ' '));
        if let [time, _, milliseconds, outcome] = fields[..]
            && time.parse::<u64>().is_ok_and(|time| time >= start)
        {
            let total = totals.entry(outcome.to_string()).or_default();
            *total = (total.0 + 1, total.1 + milliseconds.parse::<u64>().unwrap_or(0));
        }
    }
    for (outcome, (count, milliseconds)) in &totals {
        println!("{count:>7} {:>9.1} s  {outcome}", *milliseconds as f64 / 1000.0);
    }
    let count = |outcome: &str| totals.get(outcome).map_or(0, |total| total.0);
    let cached = count("hit") + count("miss");
    if cached > 0 {
        println!(
            "{:.1}% of the cached invocations hit",
            100.0 * count("hit") as f64 / cached as f64
        );
    }
}
