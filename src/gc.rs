//! The removal of the entries and slots that no build has used for a while, in the order of [`Item`], and of what they
//! leave.
//!
//! A hit sets the mtime of the entry and of the `.reads` file of the key, and a compile that takes a slot sets the mtime
//! of its lock, so an mtime is the last use. An entry or a part of a slot is renamed out of reach before it is deleted,
//! so a build that reads it then falls back as on a miss. Removal never follows a symlink.
//!
//! The cap bounds the bytes that the store takes on disk, as [`crate::usage`] measures them. A block that the store
//! shares with a file outside it, as an entry shares with the target directory that it was stored from, does not
//! count. A [`Piece`] that only the store holds counts once, and is freed with the last item that holds it.
//!
//! A remembered measure goes stale where a file outside the store stops sharing its blocks, so each run measures
//! again the [`REFRESH`] share of the items that were measured longest ago.

use std::collections::{BTreeMap, HashMap, HashSet};
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
use crate::usage::{self, Key, Piece, allocated};

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

/// The share of the remembered items that a run measures again, oldest measure first, among those measured over an
/// [`INTERVAL`] before.
///
/// A run that a miss starts comes at most once per [`INTERVAL`], so while builds run, a measure is at most about this
/// many hours old.
const REFRESH: usize = 8;

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
    let (cap, set) = store_cap(store);
    let cut = crate::now().saturating_sub(UNUSED_AGE);
    let Collected { entries, slots, freed, size, next } =
        collect(store, cut, cap, all, || space(store));
    crate::log(
        store,
        "-",
        started,
        &format!("gc {entries} {freed} {size} {slots} {cap} {}", u8::from(set)),
    );
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
    add_bytes(store, length(path));
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
    /// The time in seconds since 1970 at which `bytes` and `shared` were measured.
    measured: u64,
    /// The pieces that the item may share with other files, which `bytes` leaves out.
    shared: Vec<Piece>,
    /// The pool and number of a slot, which is `None` for an entry.
    slot: Option<(PathBuf, String)>,
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
    let stale = stale(&known, now);
    // The remembered measure of an item, unless the item changed or its measure is among the stale.
    let recalled = |path: &Path, stamp: &Option<String>| {
        let relative = path.strip_prefix(store).unwrap_or(path).to_string_lossy();
        let known = known.get(&*relative).filter(|known| Some(&known.stamp) == stamp.as_ref());
        let known = known.filter(|_| !stale.contains(&*relative));
        known.map(|known| (known.bytes, known.measured, known.shared.clone()))
    };
    let fresh = |usage: usage::Usage| (usage.own, now, usage.shared);
    // What lies beside the items, which no run removes by the order of [`Item`].
    let mut beside = Vec::new();
    // What builds leave beside the items, such as records of reads, candidates and lock files, lies in directories of
    // tens of thousands of names, so it is swept and counted once per [`SWEEP`].
    let swept =
        known.get(RECORDS).and_then(|known| Some((known.stamp.parse::<u64>().ok()?, known.bytes)));
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
            let (bytes, measured, shared) =
                recalled(&path, &stamp).unwrap_or_else(|| fresh(usage::measure(&path)));
            let (time, spent) =
                (seconds(metadata.modified()), spent(metadata.created(), metadata.modified(), now));
            let path = path.clone();
            items.push(Item { time, path, bytes, stamp, measured, shared, slot: None, spent });
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
            beside.push(usage::measure(&path));
        }
    }
    let memos = sweep(store, if all { now } else { now.saturating_sub(MEMO_AGE) }, all);
    phase.end("swept the memos");
    let records = match swept {
        Some(swept) => swept,
        None => {
            for lock in files_in_shards(&store.join("locks")).filter(|lock| lock.modified <= cut) {
                if let Some(_lock) = unlocked(&lock.path) {
                    remove(&lock.path);
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
                _ => beside.push(usage::measure(&path)),
            }
        }
        for (number, (lock, newest)) in slots {
            let time = lock.as_ref().map_or(newest, |lock| seconds(lock.modified()));
            let path = pool.join(&number);
            let part = |kind: &str| pool.join(format!("{number}.{kind}"));
            let stamp = lock.map(|lock| nanos(lock.modified()).to_string());
            let measure = || {
                let mut sum = usage::Usage::default();
                for usage in SLOT.iter().map(|kind| usage::measure(&part(kind))) {
                    sum.own += usage.own;
                    sum.shared.extend(usage.shared);
                }
                fresh(sum)
            };
            // A slot changes only under its lock, and a compile that takes the lock sets its mtime, so a size measured
            // under the lock holds until the mtime changes. A size measured while a compile holds the lock may be stale,
            // so it counts toward this run but is not remembered, and [`evict`] takes the lock again anyway. A lock
            // without parts, as `wait.lock` is, guards nothing that gc measures, and a slot without a lock has no stamp.
            let ((bytes, measured, shared), stamp) = match recalled(&path, &stamp) {
                Some(known) => (known, stamp),
                None if time > abandoned && stamp.is_some() && newest > 0 => {
                    match unlocked(&part("lock")) {
                        Some(_lock) => (measure(), stamp),
                        None => (measure(), None),
                    }
                }
                None => (measure(), stamp),
            };
            let slot = Some((pool.clone(), number));
            items.push(Item { time, path, bytes, stamp, measured, shared, slot, spent: false });
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
        beside.push(usage::measure(&path));
    }
    // The file of the sizes does not count, so that a run sees the size that the last one left.
    let files = children(store).into_iter().filter(|path| !path.ends_with(SIZES));
    let files = files.filter_map(|path| fs::symlink_metadata(path).ok());
    rest += files.filter(fs::Metadata::is_file).map(|file| allocated(&file)).sum::<u64>();
    items.sort_by_key(|item| (!item.spent, item.time));
    rest += beside.iter().map(|usage| usage.own).sum::<u64>();
    let beside = beside.iter().flat_map(|usage| usage.shared.iter().map(|piece| (now, piece)));
    let pieces =
        items.iter().flat_map(|item| item.shared.iter().map(|piece| (item.measured, piece)));
    // The holders in the store of each piece, as its newest measure counts them, where the store holds all of it. Such
    // a piece counts until the last of them goes.
    let mut holders = HashMap::<Key, (u64, u64, Piece)>::new();
    for (measured, piece) in pieces.chain(beside) {
        let (held, newest, kept) = holders.entry(piece.key).or_insert((0, measured, *piece));
        *held += 1;
        if measured > *newest {
            (*newest, *kept) = (measured, *piece);
        }
    }
    holders.retain(|_, (held, _, piece)| piece.freed_by(*held));
    let shared = holders.values().map(|(_, _, piece)| piece.bytes).sum::<u64>();
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
        if due && evict(&item) {
            *if item.slot.is_some() { &mut collected.slots } else { &mut collected.entries } += 1;
            let mut freed = item.bytes;
            for piece in &item.shared {
                if let Some((held, _, piece)) = holders.get_mut(&piece.key) {
                    *held -= 1;
                    if *held == 0 {
                        freed += piece.bytes;
                    }
                }
            }
            collected.freed += freed;
            collected.size = collected.size.saturating_sub(freed);
            if free < floor.saturating_add(margin) {
                free = space().1;
            }
            continue;
        }
        if let Some(stamp) = &item.stamp {
            let relative = item.path.strip_prefix(store).unwrap_or(&item.path).to_string_lossy();
            let shared = Vec::from_iter(item.shared.iter().map(Piece::word)).join(" ");
            let _ =
                writeln!(sizes, "{relative}\t{stamp}\t{}\t{}\t{shared}", item.bytes, item.measured);
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

/// Removes `item` unless its lock is taken, and returns whether it did.
///
/// Each part is renamed out of reach first, because a compile uses the snapshot of a slot without a check, so a killed
/// run must not leave part of one. The next run removes what such a run left.
fn evict(item: &Item) -> bool {
    let Some((pool, number)) = &item.slot else {
        let doomed = item.path.with_extension(DOOMED);
        if crate::store::rename(&item.path, &doomed).is_err() {
            return false;
        }
        remove(&doomed);
        return true;
    };
    let lock = pool.join(format!("{number}.lock"));
    let Some(_lock) = unlocked(&lock) else { return false };
    for kind in SLOT {
        let part = pool.join(format!("{number}.{kind}"));
        let doomed = pool.join(format!("{number}.{kind}.{DOOMED}"));
        if crate::store::rename(&part, &doomed).is_ok() {
            remove(&doomed);
        }
    }
    remove(&lock);
    true
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
        for memo in files_in_shards(&directory) {
            if memo.modified <= cut {
                remove(&memo.path);
            } else {
                bytes += memo.allocated;
            }
        }
    }
    bytes
}

/// The files in the shards of `directory` that [`crate::store::shard`] names, and those beside the shards.
///
/// A file beside the shards is left by a store from before them, and nothing reads it, so it ages out. A shard is
/// listed only once the files of the one before it are handled, which keeps a removal within one directory at a time.
fn files_in_shards(directory: &Path) -> impl Iterator<Item = usage::Child> {
    usage::scan(directory).into_iter().flat_map(|child| match child.directory {
        true => usage::scan(&child.path).into_iter().filter(|file| !file.directory).collect(),
        false => vec![child],
    })
}

/// What [`SIZES`] remembers of an item, or of the records under [`RECORDS`].
struct Known {
    /// What the measure holds for, or the time of the last [`SWEEP`] for the records.
    stamp: String,
    bytes: u64,
    measured: u64,
    shared: Vec<Piece>,
}

/// What [`SIZES`] remembers of each item by its path in the store.
///
/// A line of an item has the stamp, the bytes, the time of the measure and the pieces, and the line of [`RECORDS`]
/// only the first two. A line of an item without the time of its measure counted lengths, so it is not remembered.
fn remembered(store: &Path) -> HashMap<String, Known> {
    let text = fs::read_to_string(store.join(SIZES)).unwrap_or_default();
    let line = |line: &str| {
        let mut fields = line.split('\t');
        let (path, stamp) = (fields.next()?.to_string(), fields.next()?.to_string());
        let bytes = fields.next()?.parse().ok()?;
        let measured = match fields.next() {
            Some(time) => time.parse().ok()?,
            None if path == RECORDS => 0,
            None => return None,
        };
        let words = fields.next().unwrap_or_default().split(' ').filter(|word| !word.is_empty());
        let shared = words.map(Piece::parse).collect::<Option<_>>()?;
        Some((path, Known { stamp, bytes, measured, shared }))
    };
    text.lines().filter_map(line).collect()
}

/// The paths of the remembered items that a run at `now` measures again.
///
/// They are the [`REFRESH`] share of the items that were measured longest ago, among those measured over an
/// [`INTERVAL`] before.
fn stale(known: &HashMap<String, Known>, now: u64) -> HashSet<&str> {
    let mut items = Vec::from_iter(known.iter().filter(|(path, _)| *path != RECORDS));
    let share = items.len().div_ceil(REFRESH);
    items.retain(|(_, known)| known.measured.saturating_add(INTERVAL) <= now);
    items.sort_by_key(|(path, known)| (known.measured, *path));
    items.into_iter().take(share).map(|(path, _)| path.as_str()).collect()
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

/// The length of the files under `path`, which is what a new entry may take once nothing else shares its blocks.
fn length(path: &Path) -> u64 {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => children(path).iter().map(|child| length(child)).sum(),
        Ok(metadata) => metadata.len(),
        Err(_) => 0,
    }
}

fn remove(path: &Path) {
    let directory = fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir());
    let _ = if directory { fs::remove_dir_all(path) } else { fs::remove_file(path) };
}
