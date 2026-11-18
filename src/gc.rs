//! The removal of the entries that no build has used for a while, least recently used first, and of what they leave.
//!
//! A hit sets the mtime of its entry and of its key's `.reads` file to the time of use, so an mtime is the last use.
//! An entry is renamed to a name that no reader looks up before it is deleted, so a build that reads it at that moment
//! fails its restore and falls back as on a miss. Removal never follows a symlink. A size is apparent, the sum of file
//! lengths, and the space freed on disk can be less, because the clones in target directories share blocks with the
//! store.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use std::{env, fs};

use crate::unit::file_hash;

/// The age in seconds after which an entry, a compiler's memo, a notice's marker, a lock file or a log line is
/// removed, which outlasts a weekend.
const UNUSED_AGE: u64 = 3 * 86400;

/// The share in percent of its filesystem that the store may take, enough for the working set of several workspaces.
const CAP_PERCENT: u64 = 5;

/// The smallest cap in bytes, because the working set of one workspace takes several GiB even on a small disk.
const CAP_MINIMUM: u64 = 10 << 30;

/// The share in percent of its filesystem that is kept free, below which builds and the system start to fail.
const FREE_PERCENT: u64 = 10;

/// The age in seconds below which nothing is removed, even over the cap or on a full disk, so a running build keeps
/// what it uses.
///
/// It is also the age at which a staging directory counts as left by a killed process.
const RECENT: u64 = 3600;

/// The interval in seconds between the runs of gc that misses start, so that a build pays for at most one stat.
const INTERVAL: u64 = 3600;

/// What one run of gc removed and the apparent size of the store after it.
#[derive(Debug, PartialEq)]
pub struct Collected {
    pub entries: usize,
    pub freed: u64,
    pub size: u64,
}

/// Runs gc on `store` with the policy's constants, unless another run holds the lock, and logs and describes its work.
pub fn run(store: &Path) -> String {
    let started = Instant::now();
    let lock = fs::File::create(store.join("gc.lock"));
    if !lock.as_ref().is_ok_and(|lock| lock.try_lock().is_ok()) {
        return "another trucker gc is running\n".to_string();
    }
    let total = space(store).0;
    let cap = (total / 100 * CAP_PERCENT).max(CAP_MINIMUM);
    let cut = crate::now().saturating_sub(UNUSED_AGE);
    let Collected { entries, freed, size } = collect(store, cut, cap, || space(store));
    crate::log(store, "-", started, &format!("gc {entries} {freed} {size}"));
    let (total, free) = space(store);
    let over = size > cap || free < total / 100 * FREE_PERCENT;
    let megabytes = |bytes: u64| bytes as f64 / 1e6;
    format!(
        "removed {entries} entries of {:.1} MB, and the store is now {:.1} MB of a {:.1} MB cap{}\n",
        megabytes(freed),
        megabytes(size),
        megabytes(cap),
        if over { ", where the rest was used in the last hour" } else { "" }
    )
}

/// The total size and the free space in bytes of the filesystem of `path`, as an unprivileged user sees them.
///
/// Both are zero where the filesystem cannot be asked.
#[allow(clippy::unnecessary_cast)]
fn space(path: &Path) -> (u64, u64) {
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return (0, 0);
    };
    let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `path` is NUL-terminated, and a call that returns 0 has written `stats`.
    if unsafe { libc::statfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return (0, 0);
    }
    // SAFETY: the call succeeded.
    let stats = unsafe { stats.assume_init() };
    let block = stats.f_bsize as u64;
    (stats.f_blocks as u64 * block, stats.f_bavail as u64 * block)
}

/// Starts `wrapper gc` detached, where the last run that a miss started in `store` is older than [`INTERVAL`].
///
/// Returns whether it started one.
pub fn trigger(store: &Path, wrapper: &Path) -> bool {
    let stamp = store.join("gc.stamp");
    if modified(&stamp).is_some_and(|time| time > crate::now().saturating_sub(INTERVAL)) {
        return false;
    }
    let touched = fs::File::create(&stamp).and_then(|file| file.set_modified(SystemTime::now()));
    let mut command = Command::new(wrapper);
    command.arg("gc").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    touched.is_ok() && command.process_group(0).spawn().is_ok()
}

/// Removes the entries of `store` last used at or before `cut`, then the least recently used while it is over `cap`.
///
/// It also removes the least recently used while `space`, the total size and the free space of the filesystem, has
/// less than [`FREE_PERCENT`] free. An entry used in the last [`RECENT`] seconds stays. The cleanups of what entries
/// leave follow the same `cut`.
pub fn collect(store: &Path, cut: u64, cap: u64, space: impl Fn() -> (u64, u64)) -> Collected {
    let older = |path: &Path, cut: u64| modified(path).is_some_and(|time| time <= cut);
    let (pid, recent) = (std::process::id().to_string(), crate::now().saturating_sub(RECENT));
    let mut entries = Vec::new();
    for path in ["entries", "scripts"].iter().flat_map(|name| children(&store.join(name))) {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if !name.contains('.') {
            entries.extend(modified(&path).map(|time| (time, path.clone())));
        } else if older(&path, if name.ends_with(".reads") { cut } else { recent }) {
            remove(&path);
        }
    }
    let current = env::current_exe().and_then(|wrapper| file_hash(&wrapper));
    for path in children(&store.join("stand-in")) {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if current.as_ref().is_ok_and(|current| name != current.to_hex().as_str()) {
            remove(&path);
        }
    }
    let memos = ["rustc", "cc", "notices"].iter().flat_map(|name| children(&store.join(name)));
    for path in memos.filter(|path| older(path, cut)) {
        remove(&path);
    }
    for path in children(&store.join("locks")).iter().filter(|path| older(path, cut)) {
        // A held lock belongs to a compile in progress. gc holds the lock while it removes the file.
        if let Ok(lock) = fs::File::open(path)
            && lock.try_lock().is_ok()
        {
            remove(path);
        }
    }
    let log = store.join("log");
    if let Ok(text) = fs::read_to_string(&log) {
        let time = |line: &str| line.split(' ').next().and_then(|time| time.parse::<u64>().ok());
        let lines = text.lines().filter(|line| time(line).is_some_and(|time| time > cut));
        let staging = log.with_extension(&pid);
        let kept = String::from_iter(lines.map(|line| format!("{line}\n")));
        // A rewrite loses a line that a build appends meanwhile, so the log is rewritten only to drop old lines.
        if kept.len() < text.len() {
            let _ = fs::write(&staging, kept).and_then(|()| fs::rename(&staging, &log));
        }
    }
    entries.sort();
    let mut collected = Collected { entries: 0, freed: 0, size: size(store) };
    for (time, path) in entries {
        let (total, free) = space();
        if time > recent
            || (time > cut && collected.size <= cap && free >= total / 100 * FREE_PERCENT)
        {
            break;
        }
        let doomed = path.with_extension(&pid);
        if fs::rename(&path, &doomed).is_ok() {
            let bytes = size(&doomed);
            remove(&doomed);
            collected.entries += 1;
            collected.freed += bytes;
            collected.size = collected.size.saturating_sub(bytes);
        }
    }
    collected
}

/// The mtime of `path` itself, in seconds since 1970.
fn modified(path: &Path) -> Option<u64> {
    let modified = fs::symlink_metadata(path).ok()?.modified().ok()?;
    Some(modified.duration_since(UNIX_EPOCH).ok()?.as_secs())
}

fn children(directory: &Path) -> Vec<PathBuf> {
    Vec::from_iter(
        fs::read_dir(directory).into_iter().flatten().flatten().map(|entry| entry.path()),
    )
}

fn size(path: &Path) -> u64 {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => children(path).iter().map(|child| size(child)).sum(),
        Ok(metadata) => metadata.len(),
        Err(_) => 0,
    }
}

fn remove(path: &Path) {
    let directory = fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir());
    let _ = if directory { fs::remove_dir_all(path) } else { fs::remove_file(path) };
}
