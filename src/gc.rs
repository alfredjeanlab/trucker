//! The removal of the entries and slots that no build has used for a while, in the order of [`Item`], and of what they
//! leave.
//!
//! A hit sets the mtime of the entry and of the `.reads` file of the key, and a compile that takes a slot sets the mtime
//! of its lock, so an mtime is the last use. An entry or a part of a slot is renamed out of reach before it is deleted,
//! so a build that reads it then falls back as on a miss. Removal never follows a symlink.
//!
//! The cap bounds the length of the files of the store. A file beside the entries and slots, such as a memo, counts
//! its [`allocated`] blocks where they exceed its length. A file of an entry counts whole even where a worktree links
//! it too, because the removal of that worktree leaves the file to the store. A file of a slot counts its length
//! divided among its links, and a block that several slot libraries share as [`clones`] counts once. The space reported
//! as freed counts only files with one link, and on disk it can be less, because clones share blocks.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use std::{env, fs, io};

use crate::unit::file_hash;

/// The age in seconds after which an entry, a record of reads, a lock file or a log line is removed.
///
/// It outlasts a weekend.
const UNUSED_AGE: u64 = 3 * 86400;

/// The age in seconds after which a memo or a notice marker is removed, whether or not a build read it since.
///
/// A read does not set the mtime of a memo, and the memo of a library is dead once a build replaces the library. So a
/// longer age mostly keeps dead memos, and a live memo that goes costs one hash of its file.
const MEMO_AGE: u64 = 4 * 3600;

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

/// The age in seconds at which a staging directory or another leftover counts as left by a killed process.
///
/// No compile holds a slot that long, so gc measures an older slot without its lock. Eviction does not wait for an
/// age, because gc takes the lock of a slot before it removes it, and a hit on an entry that gc removes compiles as on
/// a miss.
const ABANDONED_AGE: u64 = 3600;

/// The age in seconds under which an entry that no build has hit is not removed before what builds used.
///
/// Another worktree at the same commit may be about to hit it.
const UNHIT_AGE: u64 = 3600;

/// The share in percent of the cap that a run keeps clear below the cap and above the floor of free space.
///
/// A run that finds the store within that gap evicts down to its far side, so the next run that a miss starts is about
/// that far away, rather than at the next entry stored. The gap is a share of the filesystem where that is smaller.
const MARGIN_PERCENT: u64 = 10;

/// The interval in seconds between the runs of gc that misses start, so that a build pays for at most one stat.
const INTERVAL: u64 = 3600;

/// The bytes that the store may gain before a miss starts gc within [`INTERVAL`], where no run recorded its room.
const ADDED_LIMIT: u64 = GIB;

/// The file of the store that remembers the size of each entry and idle slot, and the last [`SWEEP`].
///
/// An entry never changes once it is renamed into place, and a slot changes only under its lock, so a run measures only
/// what is new or was used since the last one.
const SIZES: &str = "gc.sizes";

/// The name in [`SIZES`] of the time of the last [`SWEEP`] and of the bytes of the records of reads and candidates.
const RECORDS: &str = "records";

/// The interval in seconds between two sweeps of the records of reads, candidates, locks and log.
///
/// It keeps each for at most a day past [`UNUSED_AGE`]. The memos are swept on every run, as [`MEMO_AGE`] is short.
const SWEEP: u64 = 86400;

/// The parts of a slot other than its lock, in the order that gc removes them.
const SLOT: [&str; 6] = ["session", "source", "libraries", "cold", "warm", "slot"];

/// The extension of an entry or part of a slot that gc renamed to remove it.
const DOOMED: &str = "gc";

/// What one run of gc removed and the size of the store after it.
#[derive(Debug, PartialEq)]
pub struct Collected {
    pub entries: usize,
    pub slots: usize,
    pub freed: u64,
    pub size: u64,
    /// The time before which a miss starts no other run, where the store stays over its bounds, or 0.
    ///
    /// Only what a run may not remove keeps the store over its bounds, as a slot that a compile holds does.
    pub next: u64,
}

/// Runs gc on `store` with the constants of this module, after any other run that holds the lock, and logs and
/// describes it.
///
/// Where `all`, it removes every entry and slot that no compile holds, and every memo and marker, whatever their age
/// and the cap. It still leaves the staging of a running build alone, and the lock files and the log follow their age.
pub fn run(store: &Path, all: bool, wait: bool) -> String {
    let lock = fs::create_dir_all(store).and_then(|()| fs::File::create(store.join("gc.lock")));
    let taken = lock.and_then(|lock| match lock.try_lock() {
        Ok(()) => Ok(lock),
        // A run that a miss started leaves the store to the run already going, which would remove what it would.
        Err(fs::TryLockError::WouldBlock) if !wait => {
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        }
        Err(fs::TryLockError::WouldBlock) => {
            eprintln!("trucker gc waits for another run of gc to end");
            let waited = Instant::now();
            let locked = lock.lock().map(|()| lock);
            eprintln!("trucker gc waited {:.1} s", waited.elapsed().as_secs_f64());
            locked
        }
        Err(fs::TryLockError::Error(error)) => Err(error),
    });
    let _lock = match taken {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => return String::new(),
        Err(_) => return format!("trucker gc cannot take its lock in {}\n", store.display()),
    };
    let started = Instant::now();
    let (cap, _) = store_cap(store);
    let cut = crate::now().saturating_sub(UNUSED_AGE);
    let Collected { entries, slots, freed, size, next } =
        collect(store, cut, cap, all, || space(store));
    crate::log(store, "-", started, &format!("gc {entries} {freed} {size} {slots}"));
    // The room is what the store may gain before it is over its bounds, after which a miss starts gc.
    let (total, free) = space(store);
    let room = cap.saturating_sub(size).min(free.saturating_sub(total / 100 * FREE_PERCENT));
    let _ = fs::write(store.join("gc.stamp"), format!("{next} {room}"));
    let megabytes = |bytes: u64| bytes as f64 / 1e6;
    format!(
        "removed {entries} entries and {slots} slots of {:.1} MB in {:.1} s, and the store is now {:.1} MB of a {:.1} MB cap{}\n",
        megabytes(freed),
        started.elapsed().as_secs_f64(),
        megabytes(size),
        megabytes(cap),
        if next > 0 { ", and the rest cannot be removed yet" } else { "" }
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
    add_bytes(store, size(path, false).0);
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
/// the store gained more since it than the room that the last run recorded, or [`ADDED_LIMIT`] without one.
///
/// A run that left the store short of its target holds off the next for [`INTERVAL`], because only what it may not
/// remove, such as a slot that a compile holds, kept the store over its bounds. Returns whether it started one.
pub fn trigger(store: &Path, wrapper: &Path) -> bool {
    // Misses that end at once would each read the stamp before any touched it, so one decides at a time, and the others
    // leave it to that one.
    let Ok(deciding) = fs::File::create(store.join("gc.trigger")) else { return false };
    if deciding.try_lock().is_err() {
        return false;
    }
    let (stamp, added) = (store.join("gc.stamp"), store.join("gc.added"));
    let lines = fs::read_to_string(&added).unwrap_or_default();
    let gained = lines.lines().filter_map(|line| line.parse::<u64>().ok()).sum::<u64>();
    // A run writes the time before which another would remove nothing, or 0, and the room that it left.
    let text = fs::read_to_string(&stamp).unwrap_or_default();
    let mut fields = text.split_whitespace().map(|field| field.parse::<u64>().ok());
    let (next, room) = (fields.next().flatten(), fields.next().flatten());
    if next.is_some_and(|next| crate::now() < next) {
        return false;
    }
    // A store over its cap leaves no room, so the check of the room below would let every miss during a run start
    // another, and each would queue behind it to remove nothing.
    let running = fs::File::create(store.join("gc.lock")).map(|lock| lock.try_lock().is_err());
    if running.unwrap_or(true) {
        return false;
    }
    if gained <= room.unwrap_or(ADDED_LIMIT)
        && modified(&stamp).is_some_and(|time| time > crate::now().saturating_sub(INTERVAL))
    {
        return false;
    }
    let _ = fs::remove_file(&added);
    // The stamp keeps the room of the last run, so a miss during this run does not start another.
    let file = fs::OpenOptions::new().write(true).create(true).truncate(false).open(&stamp);
    let touched = file.and_then(|file| file.set_modified(SystemTime::now()));
    // The run must not inherit the lock, or a miss would find it held until the run had started.
    drop(deciding);
    let mut command = Command::new(wrapper);
    command
        .args(["gc", "--if-idle"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
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

/// An entry or a slot, which gc removes as a whole, the [`spent`] entries first and then least recently used first.
///
/// A slot is never spent, because its session serves the next sources of its crate, while an entry serves only its
/// exact inputs. So an hour without a hit says little about a slot and much about an entry.
struct Item {
    /// The last use in seconds since 1970.
    time: u64,
    path: PathBuf,
    bytes: u64,
    /// What [`SIZES`] remembers `bytes` under, where the item cannot change before that does.
    stamp: Option<String>,
    /// The pool and number of a slot, which is `None` for an entry.
    slot: Option<(PathBuf, String)>,
    /// The libraries that the slot holds as [`clones`], which `bytes` leaves out.
    clones: Vec<(String, u64)>,
    spent: bool,
}

/// Removes what `store` has not used since `cut`, then the entries and slots in the order of [`Item`] while it is near
/// `cap`.
///
/// Near means within [`MARGIN_PERCENT`] of the cap. `space` gives the total size and the free space of the filesystem,
/// and gc also removes the least recently used while less than [`FREE_PERCENT`] and that margin are free. A slot whose lock a compile holds stays, however old. The cleanups of what
/// entries leave follow the same `cut`.
pub fn collect(
    store: &Path,
    cut: u64,
    cap: u64,
    all: bool,
    space: impl Fn() -> (u64, u64),
) -> Collected {
    let older = |path: &Path, cut: u64| modified(path).is_some_and(|time| time <= cut);
    let now = crate::now();
    let abandoned = now.saturating_sub(ABANDONED_AGE);
    // The cut of what only saves time, such as a record of reads, a candidate or a marker.
    let unused = if all { now } else { cut };
    let mut phase = Phase(Instant::now());
    let known = remembered(store);
    let measured = |path: &Path, stamp: &Option<String>| {
        let relative = path.strip_prefix(store).unwrap_or(path).to_string_lossy();
        let known = known.get(&*relative).filter(|(was, _)| Some(was) == stamp.as_ref());
        known.map(|(_, bytes)| *bytes)
    };
    // What builds leave beside the items, such as records of reads, candidates and lock files, lies in directories of
    // tens of thousands of names, so it is swept and counted once per [`SWEEP`].
    let swept =
        known.get(RECORDS).and_then(|(time, bytes)| Some((time.parse::<u64>().ok()?, *bytes)));
    let swept = swept.filter(|(time, _)| !all && *time > now.saturating_sub(SWEEP) && *time <= now);
    let mut records = (now, 0);
    // The bytes of what is not an entry or a slot, which each run lists anyway.
    let mut rest = 0;
    let mut items = Vec::new();
    for path in ["entries", "scripts"].iter().flat_map(|name| children(&store.join(name))) {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if !name.contains('.') {
            let Ok(metadata) = fs::symlink_metadata(&path) else { continue };
            let stamp = Some(stamp(&path, &metadata));
            let bytes = measured(&path, &stamp).unwrap_or_else(|| size(&path, false).0);
            let (time, spent) =
                (seconds(metadata.modified()), spent(metadata.created(), metadata.modified(), now));
            let path = path.clone();
            items.push(Item { time, path, bytes, stamp, slot: None, clones: Vec::new(), spent });
        } else if path.extension() == Some(DOOMED.as_ref()) {
            // Only gc dooms an entry, and this run holds the lock of gc, so the run that doomed it was killed.
            remove(&path);
        } else if name.ends_with(".cand") {
            records.1 += swept.map_or_else(|| candidates(&path, unused, abandoned), |_| 0);
        } else if swept.is_none() || !name.ends_with(".reads") {
            let reads = name.ends_with(".reads");
            match fs::symlink_metadata(&path) {
                Ok(file) if seconds(file.modified()) <= if reads { unused } else { abandoned } => {
                    remove(&path)
                }
                Ok(file) if reads => records.1 += allocated(&file),
                Ok(file) => rest += allocated(&file),
                Err(_) => {}
            }
        }
    }
    phase.end("listed the entries");
    let current = env::current_exe().and_then(|wrapper| file_hash(&wrapper));
    // A name with an extension is the staging copy of a wrapper, which may still be written. Its age is that of its
    // inode, because a copy can carry the mtime of the wrapper that it copies, as a clone on macOS does.
    for path in children(&store.join("stand-in")) {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let stale = if name.contains('.') {
            changed(&path).is_some_and(|time| time <= abandoned)
        } else {
            current.as_ref().is_ok_and(|current| name != current.to_hex().as_str())
        };
        if stale {
            remove(&path);
        } else {
            rest += size(&path, false).0;
        }
    }
    let memos = sweep(store, if all { now } else { now.saturating_sub(MEMO_AGE) }, all);
    phase.end("swept the memos");
    let records = match swept {
        Some(swept) => swept,
        None => {
            for path in files_in_shards(&store.join("locks")).filter(|path| older(path, cut)) {
                if let Some(_lock) = unlocked(&path) {
                    remove(&path);
                }
            }
            trim(&store.join("log"), cut);
            phase.end("swept the records of reads, locks and log");
            records
        }
    };
    // A slot is the lock, directory, record, snapshot, manifest and compile times of one number in a pool, and its last
    // use is the mtime of its lock, which each compile that takes it sets. Its staging leftovers follow the rule of the
    // store.
    let pools = children(&store.join("slots"));
    for pool in &pools {
        let mut slots = BTreeMap::<String, (Option<fs::Metadata>, u64)>::new();
        for path in children(pool) {
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            match name.rsplit_once('.') {
                Some((number, "lock")) => {
                    slots.entry(number.to_string()).or_default().0 =
                        fs::symlink_metadata(&path).ok();
                }
                Some((number, "slot" | "source" | "session" | "libraries" | "cold" | "warm")) => {
                    let newest = &mut slots.entry(number.to_string()).or_default().1;
                    *newest = (*newest).max(modified(&path).unwrap_or(0));
                }
                // Only gc dooms the parts of a slot, under the lock of gc.
                Some((_, DOOMED)) => remove(&path),
                _ if older(&path, abandoned) => remove(&path),
                _ => rest += size(&path, false).0,
            }
        }
        for (number, (lock, newest)) in slots {
            let time = lock.as_ref().map_or(newest, |lock| seconds(lock.modified()));
            let path = pool.join(&number);
            let part = |kind: &str| pool.join(format!("{number}.{kind}"));
            // The size leaves out the clones that the manifest of the libraries lists, so it holds while both stay.
            let listed =
                nanos(fs::symlink_metadata(part("libraries")).and_then(|file| file.modified()));
            let stamp = lock.map(|lock| format!("{}.{listed}", nanos(lock.modified())));
            let clones = clones(&part("libraries"));
            let own =
                |bytes: u64| bytes.saturating_sub(clones.iter().map(|(_, length)| length).sum());
            let measure = || own(SLOT.iter().map(|kind| size(&part(kind), true).0).sum::<u64>());
            // A slot changes only under its lock, and a compile that takes the lock sets its mtime, so a size measured
            // under the lock holds until the mtime changes. A size measured while a compile holds the lock may be stale,
            // so it counts toward this run but is not remembered, and [`evict`] takes the lock again anyway. A lock
            // without parts, as `wait.lock` is, guards nothing that gc measures, and a slot without a lock has no stamp.
            let (bytes, stamp) = match measured(&path, &stamp) {
                Some(bytes) => (bytes, stamp),
                None if time > abandoned && stamp.is_some() && newest > 0 => {
                    match unlocked(&part("lock")) {
                        Some(_lock) => (measure(), stamp),
                        None => (measure(), None),
                    }
                }
                None => (measure(), stamp),
            };
            let slot = Some((pool.clone(), number));
            items.push(Item { time, path, bytes, stamp, slot, clones, spent: false });
        }
    }
    phase.end("listed the slots");
    // A slot of a fixed OUT_DIR and its symlink to a target directory follow its lock. A marker of a key, a package or a
    // baked crate follows the rule of the records of reads.
    for path in children(&store.join("out")) {
        for lock in children(&path).iter().filter(|path| path.extension() == Some("lock".as_ref()))
        {
            if let Some(_lock) = older(lock, unused).then(|| unlocked(lock)).flatten() {
                for kind in ["", "target", "lock"] {
                    remove(&lock.with_extension(kind));
                }
            }
        }
        let _ = fs::remove_dir(&path);
        if older(&path, unused)
            && fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file())
        {
            remove(&path);
        }
        rest += size(&path, false).0;
    }
    // The file of the sizes does not count, so that a run sees the size that the last one left.
    let files = children(store).into_iter().filter(|path| !path.ends_with(SIZES));
    let files = files.filter_map(|path| fs::symlink_metadata(path).ok());
    rest += files.filter(fs::Metadata::is_file).map(|file| allocated(&file)).sum::<u64>();
    items.sort_by_key(|item| (!item.spent, item.time));
    // The slots that hold each block as a clone, and its length, which counts until the last of them goes.
    let mut holders = HashMap::<String, (usize, u64)>::new();
    for (extent, length) in items.iter().flat_map(|item| &item.clones) {
        holders.entry(extent.clone()).or_insert((0, *length)).0 += 1;
    }
    let shared = holders.values().map(|(_, length)| length).sum::<u64>();
    let size = rest + memos + records.1 + shared + items.iter().map(|item| item.bytes).sum::<u64>();
    let mut collected = Collected { entries: 0, slots: 0, freed: 0, size, next: 0 };
    let (total, mut free) = space();
    let floor = total / 100 * FREE_PERCENT;
    // A store over its cap and one just below it would otherwise both leave the next run a room of a few entries.
    let margin = cap.min(total) / 100 * MARGIN_PERCENT;
    let short = |size: u64, free: u64| size > cap - margin || free < floor.saturating_add(margin);
    let mut sizes = format!("{RECORDS}\t{}\t{}\n", records.0, records.1);
    for item in items {
        // An item that a compile holds stays, because [`evict`] does not take a lock that another process holds.
        let due = all || item.time <= cut || short(collected.size, free);
        if let Some(unlinked) = due.then(|| evict(&item)).flatten() {
            *if item.slot.is_some() { &mut collected.slots } else { &mut collected.entries } += 1;
            collected.freed += unlinked;
            collected.size = collected.size.saturating_sub(item.bytes);
            for (extent, _) in &item.clones {
                if let Some((slots, length)) = holders.get_mut(extent) {
                    *slots -= 1;
                    if *slots == 0 {
                        collected.size = collected.size.saturating_sub(*length);
                    }
                }
            }
            if free < floor.saturating_add(margin) {
                free = space().1;
            }
            continue;
        }
        if let Some(stamp) = &item.stamp {
            let relative = item.path.strip_prefix(store).unwrap_or(&item.path).to_string_lossy();
            let _ = writeln!(sizes, "{relative}\t{stamp}\t{}", item.bytes);
        }
    }
    for pool in &pools {
        let _ = fs::remove_dir(pool);
    }
    // Only what this run may not remove keeps the store short, so a run that starts at once would remove little more.
    if short(collected.size, free) {
        collected.next = now + INTERVAL;
    }
    let staging = store.join(format!("{SIZES}.new"));
    let _ =
        fs::write(&staging, sizes).and_then(|()| crate::store::rename(&staging, store.join(SIZES)));
    phase.end(&format!("removed {} entries and {} slots", collected.entries, collected.slots));
    collected
}

/// The clones that the libraries manifest at `path` lists with an extent, each by that extent and with its length.
///
/// Clones that start at one extent share their blocks, so the slots that hold them count their length once. A clone
/// without an extent, or a manifest in another format, counts whole.
fn clones(path: &Path) -> Vec<(String, u64)> {
    let text = fs::read_to_string(path).unwrap_or_default();
    let line = |line: &str| {
        let [_, _, clone, extent] = Vec::from_iter(line.split('\t'))[..] else { return None };
        let length = clone.split(' ').nth(2)?.parse().ok()?;
        (!extent.is_empty()).then(|| (format!("{extent} {length}"), length))
    };
    text.lines().filter_map(line).collect()
}

/// Whether no build has hit an entry of these birth and modification times since it was stored over [`UNHIT_AGE`]
/// before `now`.
///
/// A hit sets the mtime, which staging leaves within a second of the birth time. Where the filesystem reports no birth
/// time, no entry is spent, so the order is that of the last use alone.
pub fn spent(birth: io::Result<SystemTime>, modified: io::Result<SystemTime>, now: u64) -> bool {
    let (Ok(birth), Ok(modified)) = (birth, modified) else { return false };
    let unhit = modified.duration_since(birth).map_or(true, |late| late.as_secs() < 1);
    unhit && seconds(Ok(modified)) <= now.saturating_sub(UNHIT_AGE)
}

/// Removes the candidates in `path` that are old or unable to make their entries, and returns the bytes of the rest.
fn candidates(path: &Path, cut: u64, abandoned: u64) -> u64 {
    let mut bytes = 0;
    for file in children(path) {
        let temporary = file.extension().is_some();
        if modified(&file).is_some_and(|time| time <= if temporary { abandoned } else { cut })
            || (!temporary && !crate::lazy::is_alive(&file))
        {
            remove(&file);
        } else {
            bytes += fs::symlink_metadata(&file).map_or(0, |file| allocated(&file));
        }
    }
    let _ = fs::remove_dir(path);
    bytes
}

/// Drops the lines of `log` that are not newer than `cut`.
///
/// A rewrite loses a line that a build appends meanwhile, so the log is rewritten only to drop old lines.
fn trim(log: &Path, cut: u64) {
    let Ok(text) = fs::read_to_string(log) else { return };
    let time = |line: &str| line.split(' ').next().and_then(|time| time.parse::<u64>().ok());
    let lines = text.lines().filter(|line| time(line).is_some_and(|time| time > cut));
    let kept = String::from_iter(lines.map(|line| format!("{line}\n")));
    if kept.len() < text.len() {
        let staging = log.with_extension("new");
        let _ = fs::write(&staging, kept).and_then(|()| crate::store::rename(&staging, log));
    }
}

/// The start of a phase of a run, whose end goes to stderr, which only `trucker gc` by hand shows.
struct Phase(Instant);

impl Phase {
    fn end(&mut self, what: &str) {
        eprintln!("trucker gc {what} in {:.2} s", self.0.elapsed().as_secs_f64());
        self.0 = Instant::now();
    }
}

/// Removes `item` unless its lock is taken, and returns the bytes of its files that had one link.
///
/// Each part is renamed out of reach first, because a compile uses the snapshot of a slot without a check, so a killed
/// run must not leave part of one. The next run removes what such a run left.
fn evict(item: &Item) -> Option<u64> {
    let Some((pool, number)) = &item.slot else {
        let doomed = item.path.with_extension(DOOMED);
        crate::store::rename(&item.path, &doomed).ok()?;
        let unlinked = size(&doomed, false).1;
        remove(&doomed);
        return Some(unlinked);
    };
    let lock = pool.join(format!("{number}.lock"));
    let _lock = unlocked(&lock)?;
    let mut unlinked = 0;
    for kind in SLOT {
        let part = pool.join(format!("{number}.{kind}"));
        let doomed = pool.join(format!("{number}.{kind}.{DOOMED}"));
        if crate::store::rename(&part, &doomed).is_ok() {
            unlinked += size(&doomed, true).1;
            remove(&doomed);
        }
    }
    remove(&lock);
    Some(unlinked)
}

/// Removes the memos of `store` that were written before `cut`, and returns the bytes of those that stay.
///
/// Where `all`, each directory of memos is renamed out of reach and removed whole, without reading the metadata of
/// each memo. A directory that a killed run doomed is removed first, because only gc dooms one, under its lock.
fn sweep(store: &Path, cut: u64, all: bool) -> u64 {
    let mut bytes = 0;
    for name in ["rustc", "cc", "hashes", "notices"] {
        let (directory, doomed) = (store.join(name), store.join(format!("{name}.{DOOMED}")));
        remove(&doomed);
        if all {
            if crate::store::rename(&directory, &doomed).is_ok() {
                remove(&doomed);
            }
            continue;
        }
        for path in files_in_shards(&directory) {
            match fs::symlink_metadata(&path) {
                Ok(memo) if seconds(memo.modified()) <= cut => remove(&path),
                Ok(memo) => bytes += allocated(&memo),
                Err(_) => {}
            }
        }
    }
    bytes
}

/// The files in the shards of `directory` that [`crate::store::shard`] names, and those beside the shards.
///
/// A file beside the shards is left by a store from before them, and nothing reads it, so it ages out. A shard is
/// listed only once the files of the one before it are handled, which keeps a removal within one directory at a time.
fn files_in_shards(directory: &Path) -> impl Iterator<Item = PathBuf> {
    let entries = fs::read_dir(directory).into_iter().flatten().flatten();
    entries.flat_map(|entry| match entry.file_type() {
        Ok(kind) if kind.is_dir() => children(&entry.path()),
        _ => vec![entry.path()],
    })
}

/// The bytes that `file` takes on disk, which exceed its length where it is smaller than a block.
///
/// A memo of 32 bytes takes a block of 4 KiB on APFS, ext4 and XFS. A filesystem that keeps a small file in its
/// metadata, as btrfs does, allocates no block to it, so the length is the least that a file counts.
#[cfg(unix)]
fn allocated(file: &fs::Metadata) -> u64 {
    file.len().max(file.blocks() * 512)
}

/// The length of `file`, because NTFS keeps a small file within its record in the master file table.
#[cfg(windows)]
fn allocated(file: &fs::Metadata) -> u64 {
    file.len()
}

/// The size of each item that [`SIZES`] remembers by its path in the store, with the stamp that it holds for.
fn remembered(store: &Path) -> HashMap<String, (String, u64)> {
    let text = fs::read_to_string(store.join(SIZES)).unwrap_or_default();
    let line = |line: &str| {
        let mut fields = line.split('\t');
        let (path, stamp) = (fields.next()?.to_string(), fields.next()?.to_string());
        Some((path, (stamp, fields.next()?.parse().ok()?)))
    };
    text.lines().filter_map(line).collect()
}

/// What tells an entry from another that a later build stores under its name, which is its inode and birth time.
#[cfg(unix)]
fn stamp(_: &Path, metadata: &fs::Metadata) -> String {
    format!("{}.{}", metadata.ino(), nanos(metadata.created().or_else(|_| metadata.modified())))
}

#[cfg(windows)]
fn stamp(path: &Path, metadata: &fs::Metadata) -> String {
    let index = crate::windows::information(path, false).map_or(0, |(_, index, ..)| index);
    format!("{index}.{}", nanos(metadata.created()))
}

/// `time` in seconds since 1970, or 0 where it is not known.
fn seconds(time: std::io::Result<SystemTime>) -> u64 {
    time.ok().and_then(|time| time.duration_since(UNIX_EPOCH).ok()).map_or(0, |time| time.as_secs())
}

/// `time` in nanoseconds since 1970, or 0 where it is not known.
fn nanos(time: std::io::Result<SystemTime>) -> u128 {
    time.ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |time| time.as_nanos())
}

/// The lock at `path`, created where it is missing and taken where no other process has taken it.
///
/// gc keeps the lock while it removes what the lock guards. The lock is not truncated, because that would set its
/// mtime, which is the last use of a slot.
fn unlocked(path: &Path) -> Option<fs::File> {
    let lock = fs::OpenOptions::new().write(true).create(true).truncate(false).open(path);
    lock.ok().filter(|lock| lock.try_lock().is_ok())
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

/// The length of the files under `path`, and of those with one link.
///
/// Where `shared`, a file with several links counts its length divided among them, as a library that a slot links
/// counts once across the slots and the worktree that link it.
fn size(path: &Path, shared: bool) -> (u64, u64) {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => children(path).iter().fold((0, 0), |sum, child| {
            let size = size(child, shared);
            (sum.0 + size.0, sum.1 + size.1)
        }),
        Ok(metadata) => {
            #[cfg(unix)]
            let links = metadata.nlink().max(1);
            #[cfg(windows)]
            let links = crate::windows::information(path, false)
                .map_or(1, |(_, _, links, _)| u64::from(links.max(1)));
            let length = metadata.len();
            (if shared { length / links } else { length }, if links > 1 { 0 } else { length })
        }
        _ => (0, 0),
    }
}

fn remove(path: &Path) {
    let directory = fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir());
    let _ = if directory { fs::remove_dir_all(path) } else { fs::remove_file(path) };
}
