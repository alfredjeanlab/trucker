//! The entries of the store, which are directories that hold the outputs of one invocation.
//!
//! Outputs are cloned in and out without copying data, or linked in and copied out where the filesystem cannot clone.
//! An entry is written in a directory of its own and renamed into place, so a reader finds it whole or missing.

use std::ffi::c_int;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};
use std::{fs, io, thread};

use crate::unit::{Roots, Unit, file_hash};

/// The file of an entry that holds the stderr of rustc.
///
/// An output cannot have this name.
pub const STDERR: &str = ".stderr";

/// The file of an entry that holds the record of its compile and that [`restore`] skips.
pub const RECORD: &str = ".record";

/// The file of an entry that holds the hash of each linked output.
///
/// [`restore`] checks every copy against it.
pub const HASHES: &str = ".hashes";

/// The time that a process waits for another process to store a key before it compiles the key too.
const PATIENCE: Duration = Duration::from_secs(if cfg!(test) { 3 } else { 600 });

pub fn same_filesystem(store: &Path, out_dir: &Path) -> bool {
    matches!((fs::metadata(store), fs::metadata(out_dir)), (Ok(store), Ok(out_dir)) if store.dev() == out_dir.dev())
}

/// Clones `source` to `destination` as a file of its own that shares the blocks of `source`.
///
/// `destination` must not exist. The clone carries the current time, because cargo rebuilds a crate whose outputs are
/// older than its dependencies.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub fn clone(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    unsafe extern "C" {
        fn clonefile(
            source: *const std::ffi::c_char,
            destination: *const std::ffi::c_char,
            flags: u32,
        ) -> c_int;
    }
    let path = |path: &Path| std::ffi::CString::new(path.as_os_str().as_bytes());
    let (from, to) = (path(source)?, path(destination)?);
    // SAFETY: both pointers are NUL-terminated strings that outlive the call.
    if unsafe { clonefile(from.as_ptr(), to.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    fs::File::open(destination)?.set_modified(std::time::SystemTime::now())
}

/// Clones `source` to `destination` as a file of its own that shares the blocks of `source`.
///
/// `destination` must not exist. Fails on a filesystem without reflinks, such as ext4.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub fn clone(source: &Path, destination: &Path) -> io::Result<()> {
    use std::ffi::c_ulong;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    unsafe extern "C" {
        fn ioctl(descriptor: c_int, request: c_ulong, ...) -> c_int;
    }
    const FICLONE: c_ulong = 0x4004_9409;
    let source = fs::File::open(source)?;
    let mode = source.metadata()?.permissions().mode();
    let clone = fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(destination)?;
    // SAFETY: both descriptors are open files, and FICLONE takes the source's descriptor as its argument.
    if unsafe { ioctl(clone.as_raw_fd(), FICLONE, source.as_raw_fd()) } != 0 {
        let error = io::Error::last_os_error();
        let _ = fs::remove_file(destination);
        return Err(error);
    }
    Ok(())
}

/// How files enter and leave the store.
///
/// They are cloned where the filesystem clones, and otherwise hard-linked in and copied out, as on ext4. A probe
/// decides once per store, and a file in the store remembers the answer. Only a filesystem that refuses a clone as
/// unsupported counts as one that cannot clone.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Files {
    Clones,
    Links,
}

impl Files {
    pub fn probe(store: &Path) -> Files {
        let (clones, links) = (store.join("clones"), store.join("links"));
        if clones.exists() || links.exists() {
            return if clones.exists() { Files::Clones } else { Files::Links };
        }
        let probe = store.join(format!("probe.{}", std::process::id()));
        let copy = probe.with_extension(format!("{}.clone", std::process::id()));
        let cloned = fs::write(&probe, "probe").and_then(|()| clone(&probe, &copy));
        let _ = (fs::remove_file(&probe), fs::remove_file(&copy));
        let unsupported =
            cloned.as_ref().is_err_and(|error| error.kind() == io::ErrorKind::Unsupported);
        if cloned.is_ok() || unsupported {
            let _ = fs::write(if unsupported { links } else { clones }, "");
        }
        if unsupported { Files::Links } else { Files::Clones }
    }

    /// Gives `destination` the contents of `source` without copying them, so a write in place to one reaches the other
    /// where they are linked.
    pub fn share(self, source: &Path, destination: &Path) -> io::Result<()> {
        match self {
            Files::Clones => clone(source, destination),
            Files::Links => fs::hard_link(source, destination),
        }
    }

    /// Writes `source` to `destination` as a file of its own with the current mtime.
    pub fn duplicate(self, source: &Path, destination: &Path) -> io::Result<()> {
        match self {
            Files::Clones => clone(source, destination),
            Files::Links => fs::copy(source, destination).map(drop),
        }
    }

    /// Duplicates `source` as [`Files::duplicate`] does, or copies and verifies it where the stored file has a `hash`.
    fn restore_file(self, source: &Path, destination: &Path, hash: Option<&str>) -> io::Result<()> {
        match hash {
            Some(hash) => copy(source, destination, hash),
            None => self.duplicate(source, destination),
        }
    }

    /// Writes the tree of `from` into `to` through [`Files::duplicate`], so a file of `to` is a snapshot that a write
    /// in place to the other tree never reaches.
    pub fn duplicate_tree(self, from: &Path, to: &Path) -> io::Result<()> {
        fs::create_dir_all(to)?;
        for (path, kind) in tree(from)? {
            let (source, destination) = (from.join(&path), to.join(&path));
            if kind.is_dir() {
                fs::create_dir_all(destination)
            } else {
                self.duplicate(&source, &destination)
            }?;
        }
        Ok(())
    }
}

/// The path relative to `directory` and the type of every entry under it.
///
/// A directory comes before its contents.
pub fn tree(directory: &Path) -> io::Result<Vec<(PathBuf, fs::FileType)>> {
    let (mut found, mut pending) = (Vec::new(), vec![PathBuf::new()]);
    while let Some(relative) = pending.pop() {
        for entry in fs::read_dir(directory.join(&relative))? {
            let entry = entry?;
            let (path, kind) = (relative.join(entry.file_name()), entry.file_type()?);
            if kind.is_dir() {
                pending.push(path.clone());
            }
            found.push((path, kind));
        }
    }
    Ok(found)
}

/// Copies `source` to `destination` as a writable file of its own with the current mtime.
///
/// `destination` must not exist. Fails as invalid data where the bytes do not hash to `expected`, as after a tool wrote
/// a linked output in place.
fn copy(source: &Path, destination: &Path, expected: &str) -> io::Result<()> {
    use std::io::Read;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut source = fs::File::open(source)?;
    let mode = source.metadata()?.permissions().mode() | 0o200;
    let mut copy =
        fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(destination)?;
    let (mut hasher, mut buffer) = (blake3::Hasher::new(), vec![0; 1 << 20]);
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        copy.write_all(&buffer[..read])?;
    }
    if hasher.finalize().to_hex().as_str() != expected {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(())
}

/// Writes the outputs of `unit` and `record` into `staging`, and returns what the crate read that the key does not
/// cover.
///
/// The hash of each linked output is recorded in [`HASHES`], because a tool that writes the worktree's file in place
/// changes the store's too. Fails where an output of an own crate contains the path of its worktree or of a separate
/// target directory. A hit in another worktree would carry that path, as a constant built with `env!` or `concat!` can.
fn stage(
    unit: &Unit,
    staging: &Path,
    stderr: &[u8],
    record: &str,
    files: Files,
) -> Result<String, &'static str> {
    let (mut reads, mut hashes, roots) = (String::new(), String::new(), &unit.roots);
    let own_paths =
        Vec::from_iter(roots.worktree.as_deref().into_iter().chain(roots.separate_target()));
    fs::create_dir_all(staging).map_err(|_| "store-error")?;
    let outputs = unit.outputs().map_err(|_| "store-error")?;
    let is_dep_info = |output: &&PathBuf| output.extension().is_some_and(|kind| kind == "d");
    let scanned = Vec::from_iter(outputs.iter().filter(|output| !is_dep_info(output)));
    if !own_paths.is_empty() && any_contains(&scanned, &own_paths) {
        return Err("baked-path");
    }
    for output in &outputs {
        let stored = staging.join(output.file_name().unwrap_or_default());
        if is_dep_info(&output) {
            let dep_info =
                fs::read_to_string(output).ok().and_then(|text| unit.portable_dep_info(&text));
            let (dep_info, read) = dep_info.ok_or("unkeyed-input")?;
            reads = read;
            fs::write(stored, dep_info).map_err(|_| "store-error")?;
        } else if files == Files::Clones {
            clone(output, &stored).map_err(|_| "clone-failed")?;
        } else {
            files.share(output, &stored).map_err(|_| "store-error")?;
            let hash = file_hash(&stored).map_err(|_| "store-error")?;
            let name = stored.file_name().unwrap_or_default().to_string_lossy();
            hashes.push_str(&format!("{} {name}\n", hash.to_hex()));
        }
    }
    if files == Files::Links {
        fs::write(staging.join(HASHES), hashes).map_err(|_| "store-error")?;
    }
    let stderr = unit.roots.normalized(&String::from_utf8_lossy(stderr));
    fs::write(staging.join(STDERR), stderr).map_err(|_| "store-error")?;
    fs::write(staging.join(RECORD), record).map_err(|_| "store-error").map(|()| reads)
}

/// Whether any file of `paths` contains the bytes of any of `texts` or cannot be read.
///
/// The scan runs on every core, because it delays every edit that recompiles the crate. The debug info of an
/// executable can take hundreds of megabytes, and a crate has hundreds of object files.
pub fn any_contains(paths: &[impl AsRef<Path> + Sync], texts: &[&str]) -> bool {
    let finders = Vec::from_iter(texts.iter().map(memchr::memmem::Finder::new));
    let (next, found) = (AtomicUsize::new(0), AtomicBool::new(false));
    let worker = || {
        while let Some(path) = paths.get(next.fetch_add(1, Ordering::Relaxed)) {
            if found.load(Ordering::Relaxed) {
                return;
            }
            let contains = |bytes: &[u8]| finders.iter().any(|text| text.find(bytes).is_some());
            let path = path.as_ref();
            let hit = match map(path) {
                Some(map) => contains(&map),
                None => fs::read(path).map_or(true, |bytes| contains(&bytes)),
            };
            if hit {
                found.store(true, Ordering::Relaxed);
            }
        }
    };
    let workers = thread::available_parallelism().map_or(1, usize::from).min(paths.len());
    thread::scope(|scope| {
        for _ in 1..workers {
            scope.spawn(worker);
        }
        worker();
    });
    found.into_inner()
}

/// A map of the file at `path`, or `None` where it is too small for a map to be cheaper than a read, or cannot be
/// mapped.
#[allow(unsafe_code)]
pub fn map(path: &Path) -> Option<memmap2::Mmap> {
    let file = fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() < 1 << 16 {
        return None;
    }
    // SAFETY: the file is an output of a process that has exited, so nothing writes it while it is mapped.
    unsafe { memmap2::Mmap::map(&file) }.ok()
}

/// Stores the outputs of `unit`, rustc's `stderr` and the `record` of the compile under `key` in `entries`.
///
/// `started` is the start of the compile, which [`Unit::variant`] checks the sources against.
///
/// Fails with the reason where it cannot store them. Where another process stored the entry first, that entry stays.
pub fn publish(
    unit: &Unit,
    entries: &Path,
    key: &str,
    stderr: &[u8],
    record: &str,
    started: SystemTime,
    files: Files,
) -> Result<(), &'static str> {
    let staging = entries.join(format!("{key}.{}", std::process::id()));
    let stored = stage(unit, &staging, stderr, record, files).and_then(|reads| {
        let entry = entries.join(unit.variant(key, &reads, Some(started))?);
        commit(entries, key, &reads, &staging, &entry)
    });
    let _ = fs::remove_dir_all(&staging);
    stored
}

/// Records `reads` for `key` in `entries`, then renames `staging` to `entry`, unless another process stored it first.
pub fn commit(
    entries: &Path,
    key: &str,
    reads: &str,
    staging: &Path,
    entry: &Path,
) -> Result<(), &'static str> {
    let recorded = reads.is_empty() || fs::write(reads_of(entries, key), reads).is_ok();
    let renamed = recorded && fs::rename(staging, entry).is_ok();
    if let Some(store) = entries.parent().filter(|_| renamed) {
        crate::gc::add(store, entry);
    }
    if renamed || entry.is_dir() { Ok(()) } else { Err("store-error") }
}

/// The file of what a compile of `key` read that the key does not cover.
///
/// Its values select the entry.
pub fn reads_of(entries: &Path, key: &str) -> std::path::PathBuf {
    entries.join(format!("{key}.reads"))
}

/// An exclusive lock on `key` in `store` that a process takes so that others wait while it compiles the key.
///
/// The lock is polled, because a blocking call cannot time out, and the kernel releases it when its process dies.
/// After a minute the wait is said once on stderr. It is `None` where the lock file cannot be opened, or after
/// [`PATIENCE`].
pub fn lock(store: &Path, key: &str, name: &str) -> Option<fs::File> {
    fs::create_dir_all(store.join("locks")).ok()?;
    let file = fs::File::create(store.join("locks").join(key)).ok()?;
    let (started, mut said) = (Instant::now(), false);
    loop {
        match file.try_lock() {
            Ok(()) => return Some(file),
            Err(fs::TryLockError::Error(_)) => return None,
            Err(fs::TryLockError::WouldBlock) => {}
        }
        if started.elapsed() >= PATIENCE {
            return None;
        }
        if !said && started.elapsed() >= Duration::from_secs(60) {
            eprintln!("trucker waits for another build to compile {name}");
            said = true;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Records a use of `entry`, and of the `.reads` file of `key` in `entries`, by setting their mtime to now.
///
/// gc removes a `.reads` file by its mtime, because the name of an entry that it selects does not contain the key.
pub fn touch(entries: &Path, key: &str, entry: &Path) {
    for path in [entry, &reads_of(entries, key)] {
        let _ =
            fs::File::open(path).and_then(|file| file.set_modified(std::time::SystemTime::now()));
    }
}

/// Remembers for `output`, just restored from `stored` in `entry`, the hash of `stored`, which is `expected` where the
/// entry holds linked files.
///
/// The output is remembered although its times are new, because it is whole once renamed into place, and nothing writes
/// it in place afterwards, as rustc replaces an output that trucker removed.
fn remember_restored(entry: &Path, stored: &Path, output: &Path, expected: Option<&str>) {
    let Some(memos) = entry.parent().and_then(Path::parent).map(|store| store.join("hashes"))
    else {
        return;
    };
    let hash = match expected {
        Some(expected) => blake3::Hash::from_hex(expected).ok(),
        None => crate::unit::file_hashes(&[stored.to_path_buf()], &memos)
            .ok()
            .and_then(|hashes| hashes.first().copied()),
    };
    if let (Some(hash), Ok(made)) = (hash, crate::unit::identity(output)) {
        let _ = crate::unit::remember(&memos, &made, hash.as_bytes());
    }
}

/// Restores the outputs of `entry` into the output directory of `unit` and replays to `cargo` the stderr that it
/// parses.
///
/// A file goes through [`Files::restore_file`] under a temporary name, and is renamed into place once it is whole. An
/// entry with [`HASHES`] holds linked files, so every file is copied and verified whatever the marker of the store
/// says. A file that fails is removed from the output directory, the entry is evicted, and the restore fails as
/// `evicted`.
///
/// An output equal to the stored one keeps its mtime. Only an `.rmeta` can be equal, because the caller removes the
/// other outputs. In the worktree that stored a linked entry, that output is the stored file itself, so it must match
/// [`HASHES`].
pub fn restore(
    unit: &Unit,
    entry: &Path,
    files: Files,
    cargo: &mut dyn Write,
) -> Result<(), &'static str> {
    let failed = |_| "unrestorable";
    let (mut stderr, hashes) = (String::new(), fs::read_to_string(entry.join(HASHES)).ok());
    for stored in fs::read_dir(entry).map_err(failed)? {
        let stored = stored.map_err(failed)?.path();
        let name = stored.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let output = unit.out_dir.join(&name);
        if name == RECORD || name == HASHES {
            continue;
        } else if name == STDERR {
            stderr = unit.roots.localized(&fs::read_to_string(&stored).map_err(failed)?);
            continue;
        } else if stored.extension().is_some_and(|kind| kind == "d") {
            let dep_info = fs::read_to_string(&stored).map_err(failed)?;
            fs::write(output, unit.roots.dep_info(&dep_info, Roots::localized)).map_err(failed)?;
            continue;
        }
        let suffix = format!(" {name}");
        let expected = hashes.as_ref().map(|hashes| {
            hashes.lines().find_map(|line| line.strip_suffix(&suffix)).unwrap_or_default()
        });
        let kept = file_hash(&output).is_ok_and(|kept| match expected {
            Some(expected) => kept.to_hex().as_str() == expected,
            None => file_hash(&stored).is_ok_and(|hash| hash == kept),
        });
        if kept {
            continue;
        }
        let _ = fs::remove_file(&output);
        let partial = unit.out_dir.join(format!("{name}.{}.trucker", std::process::id()));
        let restored = files.restore_file(&stored, &partial, expected);
        if let Err(error) = restored.and_then(|()| fs::rename(&partial, &output)) {
            let _ = fs::remove_file(&partial);
            if error.kind() != io::ErrorKind::InvalidData {
                return Err("unrestorable");
            }
            // An entry that a tool changed through a link is removed as gc removes one, and stored again.
            let doomed = entry.with_extension(std::process::id().to_string());
            let _ = fs::rename(entry, &doomed).and_then(|()| fs::remove_dir_all(&doomed));
            return Err("evicted");
        }
        // A dependent hashes each library for its key, and one that was just restored is not settled.
        if name.starts_with("lib") {
            remember_restored(entry, &stored, &output, expected);
        }
    }
    cargo.write_all(stderr.as_bytes()).map_err(failed)
}
