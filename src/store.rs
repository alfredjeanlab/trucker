//! The store's entries, each a directory of the outputs of one invocation, cloned in and out without copying data.
//!
//! An entry is written in a directory of its own and renamed into place, so a reader finds it whole or missing.

use std::ffi::c_int;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, Instant};
use std::{fs, io, thread};

use crate::unit::{Roots, Unit, file_hash};

/// The entry's file of rustc's stderr, whose name no output can have.
const STDERR: &str = ".stderr";

/// The entry's file of the record of its compile, which restore skips.
pub const RECORD: &str = ".record";

/// The time that a process waits for another process to store a key before it compiles the key too.
const PATIENCE: Duration = Duration::from_secs(if cfg!(test) { 3 } else { 600 });

pub fn same_filesystem(store: &Path, out_dir: &Path) -> bool {
    matches!((fs::metadata(store), fs::metadata(out_dir)), (Ok(store), Ok(out_dir)) if store.dev() == out_dir.dev())
}

/// Clones `source` to `destination`, which must not exist, as a file of its own that shares the blocks of `source`.
///
/// The clone carries the current time, because cargo rebuilds a crate whose outputs are older than its dependencies.
#[cfg(target_os = "macos")]
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

/// Clones `source` to `destination`, which must not exist, as a file of its own that shares the blocks of `source`.
///
/// Fails on a filesystem without reflinks, such as ext4.
#[cfg(target_os = "linux")]
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

/// Writes the outputs of `unit` and `record` into `staging`, and returns what the crate read that the key does not cover.
///
/// Fails where an output of an own crate contains the path of its worktree or of a separate target directory, which a
/// hit in another worktree would carry, as one does that a constant built with `env!` or `concat!` names.
fn stage(unit: &Unit, staging: &Path, stderr: &[u8], record: &str) -> Result<String, &'static str> {
    let (mut reads, roots) = (String::new(), &unit.roots);
    let own_paths = roots.worktree.as_deref().into_iter().chain(roots.separate_target());
    fs::create_dir_all(staging).map_err(|_| "store-error")?;
    for output in unit.outputs().map_err(|_| "store-error")? {
        let stored = staging.join(output.file_name().unwrap_or_default());
        if output.extension().is_some_and(|kind| kind == "d") {
            let dep_info =
                fs::read_to_string(&output).ok().and_then(|text| unit.portable_dep_info(&text));
            let (dep_info, read) = dep_info.ok_or("unkeyed-input")?;
            reads = read;
            fs::write(stored, dep_info).map_err(|_| "store-error")?;
        } else if own_paths.clone().any(|root| contains(&output, root)) {
            return Err("baked-path");
        } else {
            clone(&output, &stored).map_err(|_| "clone-failed")?;
        }
    }
    let stderr = unit.roots.normalized(&String::from_utf8_lossy(stderr));
    fs::write(staging.join(STDERR), stderr).map_err(|_| "store-error")?;
    fs::write(staging.join(RECORD), record).map_err(|_| "store-error").map(|()| reads)
}

/// Whether the file at `path` contains the bytes of `text`, or cannot be read.
fn contains(path: &Path, text: &str) -> bool {
    let needle = text.as_bytes();
    fs::read(path).map_or(true, |bytes| bytes.windows(needle.len()).any(|window| window == needle))
}

/// Stores the outputs of `unit`, rustc's `stderr` and the `record` of the compile under `key` in `entries`.
///
/// Fails with the reason that it cannot. Where another process stored the entry first, that entry stays.
pub fn publish(
    unit: &Unit,
    entries: &Path,
    key: &str,
    stderr: &[u8],
    record: &str,
) -> Result<(), &'static str> {
    let staging = entries.join(format!("{key}.{}", std::process::id()));
    let stored = stage(unit, &staging, stderr, record).and_then(|reads| {
        commit(entries, key, &reads, &staging, &entries.join(unit.variant(key, &reads)))
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
    if renamed || entry.is_dir() { Ok(()) } else { Err("store-error") }
}

/// The file of what a compile of `key` read that the key does not cover, whose values select its entry.
pub fn reads_of(entries: &Path, key: &str) -> std::path::PathBuf {
    entries.join(format!("{key}.reads"))
}

/// An exclusive lock on `key` in `store`, which a process takes before it compiles the key, so that others wait.
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

/// Clones the outputs of `entry` into the output directory of `unit`, then replays rustc's stderr, which cargo parses.
///
/// An output equal to the stored one, which only an `.rmeta` can be after the caller's removal, keeps its mtime.
pub fn restore(unit: &Unit, entry: &Path) -> io::Result<()> {
    let mut stderr = String::new();
    for stored in fs::read_dir(entry)? {
        let stored = stored?.path();
        let output = unit.out_dir.join(stored.file_name().unwrap_or_default());
        if stored.ends_with(RECORD) {
            continue;
        } else if stored.ends_with(STDERR) {
            stderr = unit.roots.localized(&fs::read_to_string(&stored)?);
        } else if stored.extension().is_some_and(|kind| kind == "d") {
            fs::write(
                output,
                unit.roots.dep_info(&fs::read_to_string(&stored)?, Roots::localized),
            )?;
        } else if !file_hash(&output)
            .is_ok_and(|kept| file_hash(&stored).is_ok_and(|hash| hash == kept))
        {
            let _ = fs::remove_file(&output);
            clone(&stored, &output)?;
        }
    }
    io::stderr().write_all(stderr.as_bytes())
}
