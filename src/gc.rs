//! The removal of the entries that no build has used for a while, least recently used first, and of what they leave.
//!
//! A hit sets the mtime of the entry and of the `.reads` file of the key to the time of use, so an mtime is the last
//! use. Before an entry is deleted, it is renamed to a name that readers do not look up, and a build that reads it at
//! that moment fails its restore and falls back as on a miss. Removal never follows a symlink.
//!
//! The cap bounds the length of the files of the store. An inode counts once, and it counts even where a worktree links
//! it too, because the removal of that worktree leaves the file to the store. The space reported as freed counts only
//! files with one link, and on disk it can be less, because the clones in target directories share blocks with the
//! store.

use std::collections::HashSet;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use std::{env, fs};

use crate::unit::file_hash;

/// The age in seconds after which an entry, a memo, a notice marker, a lock file or a log line is removed.
///
/// It outlasts a weekend.
const UNUSED_AGE: u64 = 3 * 86400;

const GIB: u64 = 1 << 30;

/// The share in percent of its filesystem that the store may take, which holds the working sets of several workspaces.
const CAP_PERCENT: u64 = 10;

/// The smallest cap, so that a small disk does not evict a working set.
///
/// It holds the working set of one large workspace across a change of its dependencies, with headroom. The stores on a
/// benchmark machine took these sizes.
/// - ripgrep took 0.4 to 0.7 GB.
/// - cargo took 2.6 to 3.0 GB.
/// - A large workspace reached about 12 GB across a change of its dependencies.
const CAP_MINIMUM: u64 = 16 * GIB;

/// The largest cap, because a large disk does not justify a cache that takes more than plain cargo would need.
const CAP_MAXIMUM: u64 = 100 * GIB;

/// The variable that sets the cap in place of the computed one where the stores of a machine need more or less.
pub const SIZE_VARIABLE: &str = "TRUCKER_CACHE_SIZE";

/// The share in percent of its filesystem that is kept free, because builds and the system start to fail below it.
const FREE_PERCENT: u64 = 10;

/// The age in seconds below which nothing is removed, even over the cap or on a full disk.
///
/// A running build thus keeps what it uses. It is also the age at which a staging directory counts as left by a killed
/// process.
const RECENT: u64 = 3600;

/// The interval in seconds between the runs of gc that misses start, so that a build pays for at most one stat.
const INTERVAL: u64 = 3600;

/// The bytes that the store may gain before a miss starts gc within [`INTERVAL`], because a busy hour adds tens of GiB.
const ADDED_LIMIT: u64 = GIB;

/// What one run of gc removed and the size of the store after it.
#[derive(Debug, PartialEq)]
pub struct Collected {
    pub entries: usize,
    pub freed: u64,
    pub size: u64,
}

/// Runs gc on `store` with the constants of this module unless another run holds the lock, and logs and describes it.
pub fn run(store: &Path) -> String {
    let started = Instant::now();
    let lock = fs::File::create(store.join("gc.lock"));
    if !lock.as_ref().is_ok_and(|lock| lock.try_lock().is_ok()) {
        return "another trucker gc is running\n".to_string();
    }
    let (cap, _) = store_cap(store);
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

/// The size in bytes that a store may take on a filesystem of `total` bytes, and whether `setting` set it.
///
/// `setting` is the value of [`SIZE_VARIABLE`], and one that [`bytes`] cannot read leaves the computed cap.
pub fn cap(total: u64, setting: Option<&str>) -> (u64, bool) {
    match setting.and_then(bytes) {
        Some(bytes) => (bytes, true),
        None => ((total / 100 * CAP_PERCENT).clamp(CAP_MINIMUM, CAP_MAXIMUM), false),
    }
}

/// The cap of `store` under the current environment, and whether [`SIZE_VARIABLE`] set it.
pub fn store_cap(store: &Path) -> (u64, bool) {
    cap(space(store).0, setting().as_deref())
}

/// The value of [`SIZE_VARIABLE`], where it is set and not empty.
pub fn setting() -> Option<String> {
    let value = env::var_os(SIZE_VARIABLE)?.to_string_lossy().into_owned();
    (!value.is_empty()).then_some(value)
}

/// The bytes that `text` names as a positive number with an optional binary suffix, such as `200G` or `500000000`.
///
/// The suffix is K, M, G or T in either case, and the number may have a fraction, as `1.5T` does.
pub fn bytes(text: &str) -> Option<u64> {
    let (number, suffix) = text.split_at(text.trim_end_matches(char::is_alphabetic).len());
    let power =
        ["", "k", "m", "g", "t"].iter().position(|unit| suffix.eq_ignore_ascii_case(unit))?;
    let bytes = number.parse::<f64>().ok()? * (1u64 << (10 * power)) as f64;
    (bytes.is_finite() && bytes >= 1.0).then_some(bytes as u64)
}

/// Adds the size of `path`, as the cap counts it, to the bytes that `store` gained since a miss last started gc.
///
/// Each addition is a line appended to `gc.added`, so concurrent builds do not need a lock.
pub fn add(store: &Path, path: &Path) {
    add_bytes(store, size(path, &mut HashSet::new()).0);
}

/// Adds `bytes` to the bytes that `store` gained since a miss last started gc.
pub fn add_bytes(store: &Path, bytes: u64) {
    let file = fs::OpenOptions::new().create(true).append(true).open(store.join("gc.added"));
    let _ = file
        .and_then(|mut file| std::io::Write::write_all(&mut file, format!("{bytes}\n").as_bytes()));
}

/// The total size and the free space in bytes of the filesystem of `path`, as an unprivileged user sees them.
///
/// Both are zero where the filesystem cannot be asked.
#[cfg(unix)]
#[allow(clippy::unnecessary_cast, unsafe_code)]
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

#[cfg(windows)]
use crate::windows::space;

/// Starts `wrapper gc` detached, where the last run that a miss started in `store` is older than [`INTERVAL`] or
/// the store gained more than [`ADDED_LIMIT`] since it.
///
/// Returns whether it started one.
pub fn trigger(store: &Path, wrapper: &Path) -> bool {
    let (stamp, added) = (store.join("gc.stamp"), store.join("gc.added"));
    let lines = fs::read_to_string(&added).unwrap_or_default();
    let gained = lines.lines().filter_map(|line| line.parse::<u64>().ok()).sum::<u64>();
    if gained <= ADDED_LIMIT
        && modified(&stamp).is_some_and(|time| time > crate::now().saturating_sub(INTERVAL))
    {
        return false;
    }
    let _ = fs::remove_file(&added);
    let touched = fs::File::create(&stamp).and_then(|file| file.set_modified(SystemTime::now()));
    let mut command = Command::new(wrapper);
    command.arg("gc").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    touched.is_ok() && detached(&mut command).spawn().is_ok()
}

#[cfg(unix)]
fn detached(command: &mut Command) -> &mut Command {
    command.process_group(0)
}

/// `command` in a process group of its own and without a console, so that Ctrl-C in the build does not reach it.
#[cfg(windows)]
fn detached(command: &mut Command) -> &mut Command {
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
}

/// Removes what `store` has not used since `cut`, then the least recently used while it is over `cap`.
///
/// `space` gives the total size and the free space of the filesystem, and gc also removes the least recently used while
/// less than [`FREE_PERCENT`] is free. An entry used in the last [`RECENT`] seconds stays. The cleanups of what entries
/// leave follow the same `cut`.
pub fn collect(store: &Path, cut: u64, cap: u64, space: impl Fn() -> (u64, u64)) -> Collected {
    let older = |path: &Path, cut: u64| modified(path).is_some_and(|time| time <= cut);
    let (pid, recent) = (std::process::id().to_string(), crate::now().saturating_sub(RECENT));
    let mut entries = Vec::new();
    for path in ["entries", "scripts"].iter().flat_map(|name| children(&store.join(name))) {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if !name.contains('.') {
            entries.extend(modified(&path).map(|time| (time, path.clone())));
        } else if name.ends_with(".cand") {
            // A candidate counts only its file, and is removed once old or unable to make its entry.
            for file in children(&path) {
                let temporary = file.extension().is_some();
                if older(&file, if temporary { recent } else { cut })
                    || (!temporary && !crate::lazy::is_alive(&file))
                {
                    remove(&file);
                }
            }
            let _ = fs::remove_dir(&path);
        } else if older(&path, if name.ends_with(".reads") { cut } else { recent }) {
            remove(&path);
        }
    }
    let current = env::current_exe().and_then(|wrapper| file_hash(&wrapper));
    // A name with an extension is the staging copy of a wrapper, which may still be written. Its age is that of its
    // inode, because a copy can carry the mtime of the wrapper that it copies, as a clone on macOS does.
    for path in children(&store.join("stand-in")) {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let stale = if name.contains('.') {
            changed(&path).is_some_and(|time| time <= recent)
        } else {
            current.as_ref().is_ok_and(|current| name != current.to_hex().as_str())
        };
        if stale {
            remove(&path);
        }
    }
    let memos =
        ["rustc", "cc", "hashes", "notices"].iter().flat_map(|name| children(&store.join(name)));
    for path in memos.filter(|path| older(path, cut)) {
        remove(&path);
    }
    for path in children(&store.join("locks")).iter().filter(|path| older(path, cut)) {
        if let Some(_lock) = unlocked(path) {
            remove(path);
        }
    }
    // The lock, directory, session snapshot and staging leftovers of a slot follow the rules for the locks and staging
    // of the store.
    for pool in children(&store.join("slots")) {
        for path in children(&pool) {
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            match name.rsplit_once('.') {
                Some((number, "lock")) if older(&path, cut) => {
                    if let Some(_lock) = unlocked(&path) {
                        for kind in ["slot", "source", "session", "libraries", "lock"] {
                            remove(&pool.join(format!("{number}.{kind}")));
                        }
                    }
                }
                Some((_, "lock" | "slot" | "source" | "session" | "libraries")) => {}
                _ if older(&path, recent) => remove(&path),
                _ => {}
            }
        }
        let _ = fs::remove_dir(&pool);
    }
    // A slot of a fixed OUT_DIR and its symlink to a target directory follow its lock. A marker of a key, a package or a
    // baked crate follows the rule of the memos.
    for path in children(&store.join("out")) {
        for lock in children(&path).iter().filter(|path| path.extension() == Some("lock".as_ref()))
        {
            if let Some(_lock) = older(lock, cut).then(|| unlocked(lock)).flatten() {
                for kind in ["", "target", "lock"] {
                    remove(&lock.with_extension(kind));
                }
            }
        }
        let _ = fs::remove_dir(&path);
        if older(&path, cut) && fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file())
        {
            remove(&path);
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
            let _ = fs::write(&staging, kept).and_then(|()| crate::store::rename(&staging, &log));
        }
    }
    entries.sort();
    let mut collected =
        Collected { entries: 0, freed: 0, size: size(store, &mut HashSet::new()).0 };
    for (time, path) in entries {
        let (total, free) = space();
        if time > recent
            || (time > cut && collected.size <= cap && free >= total / 100 * FREE_PERCENT)
        {
            continue;
        }
        let doomed = path.with_extension(&pid);
        if crate::store::rename(&path, &doomed).is_ok() {
            let (owned, unlinked) = size(&doomed, &mut HashSet::new());
            remove(&doomed);
            collected.entries += 1;
            collected.freed += unlinked;
            collected.size = collected.size.saturating_sub(owned);
        }
    }
    collected
}

/// The lock at `path`, created where it is missing and taken where no other process has taken it.
///
/// gc keeps the lock while it removes what the lock guards.
fn unlocked(path: &Path) -> Option<fs::File> {
    fs::File::create(path).ok().filter(|lock| lock.try_lock().is_ok())
}

/// The mtime of `path` itself, in seconds since 1970.
fn modified(path: &Path) -> Option<u64> {
    let modified = fs::symlink_metadata(path).ok()?.modified().ok()?;
    Some(modified.duration_since(UNIX_EPOCH).ok()?.as_secs())
}

/// The ctime of `path` itself in seconds since 1970, which a copy does not carry over from its source.
#[cfg(unix)]
fn changed(path: &Path) -> Option<u64> {
    fs::symlink_metadata(path).ok().and_then(|metadata| u64::try_from(metadata.ctime()).ok())
}

#[cfg(windows)]
use crate::windows::change_time as changed;

fn children(directory: &Path) -> Vec<PathBuf> {
    Vec::from_iter(
        fs::read_dir(directory).into_iter().flatten().flatten().map(|entry| entry.path()),
    )
}

/// The length of the files under `path` whose inodes are not in `seen`, and of those with one link.
fn size(path: &Path, seen: &mut HashSet<(u64, u64)>) -> (u64, u64) {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => children(path).iter().fold((0, 0), |sum, child| {
            let size = size(child, seen);
            (sum.0 + size.0, sum.1 + size.1)
        }),
        #[cfg(unix)]
        Ok(metadata) if seen.insert((metadata.dev(), metadata.ino())) => {
            (metadata.len(), if metadata.nlink() > 1 { 0 } else { metadata.len() })
        }
        #[cfg(windows)]
        Ok(_) => match crate::windows::information(path, false) {
            Some((volume, index, links, length)) if seen.insert((u64::from(volume), index)) => {
                (length, if links > 1 { 0 } else { length })
            }
            _ => (0, 0),
        },
        _ => (0, 0),
    }
}

fn remove(path: &Path) {
    let directory = fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir());
    let _ = if directory { fs::remove_dir_all(path) } else { fs::remove_file(path) };
}
