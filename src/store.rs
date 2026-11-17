//! The store's entries, each a directory of the outputs of one invocation, cloned in and out without copying data.
//!
//! An entry is written in a directory of its own and renamed into place, so a reader finds it whole or missing.

use std::ffi::c_int;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::{fs, io};

use crate::unit::Unit;

/// The entry's file of rustc's stderr, whose name no output can have.
const STDERR: &str = ".stderr";

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

/// Writes the outputs of `unit` into `staging`, and returns what the crate read that the key does not cover.
fn stage(unit: &Unit, staging: &Path, stderr: &[u8]) -> Result<String, &'static str> {
    let mut reads = String::new();
    fs::create_dir_all(staging).map_err(|_| "store-error")?;
    for output in unit.outputs().map_err(|_| "store-error")? {
        let stored = staging.join(output.file_name().unwrap_or_default());
        if output.extension().is_some_and(|kind| kind == "d") {
            let dep_info =
                fs::read_to_string(&output).ok().and_then(|text| unit.portable_dep_info(&text));
            let (dep_info, read) = dep_info.ok_or("unkeyed-input")?;
            reads = read;
            fs::write(stored, dep_info).map_err(|_| "store-error")?;
        } else {
            clone(&output, &stored).map_err(|_| "clone-failed")?;
        }
    }
    let stderr = unit.roots.normalized(&String::from_utf8_lossy(stderr));
    fs::write(staging.join(STDERR), stderr).map_err(|_| "store-error").map(|()| reads)
}

/// Stores the outputs of `unit` and rustc's `stderr` under `key` in `entries`, or names the reason that it cannot.
///
/// Where another process stored the entry first, that entry stays.
pub fn publish(unit: &Unit, entries: &Path, key: &str, stderr: &[u8]) -> Result<(), &'static str> {
    let staging = entries.join(format!("{key}.{}", std::process::id()));
    let stored = stage(unit, &staging, stderr).and_then(|reads| {
        let entry = entries.join(unit.variant(key, &reads));
        let recorded = reads.is_empty() || fs::write(reads_of(entries, key), &reads).is_ok();
        let renamed = recorded && fs::rename(&staging, &entry).is_ok();
        if renamed || entry.is_dir() { Ok(()) } else { Err("store-error") }
    });
    let _ = fs::remove_dir_all(&staging);
    stored
}

/// The file of what a compile of `key` read that the key does not cover, whose values select its entry.
pub fn reads_of(entries: &Path, key: &str) -> std::path::PathBuf {
    entries.join(format!("{key}.reads"))
}

/// Clones the outputs of `entry` into the output directory of `unit`, then replays rustc's stderr, which cargo parses.
pub fn restore(unit: &Unit, entry: &Path) -> io::Result<()> {
    let mut stderr = String::new();
    for stored in fs::read_dir(entry)? {
        let stored = stored?.path();
        let output = unit.out_dir.join(stored.file_name().unwrap_or_default());
        if stored.ends_with(STDERR) {
            stderr = unit.roots.localized(&fs::read_to_string(&stored)?);
        } else if stored.extension().is_some_and(|kind| kind == "d") {
            fs::write(output, unit.roots.localized(&fs::read_to_string(&stored)?))?;
        } else {
            clone(&stored, &output)?;
        }
    }
    io::stderr().write_all(stderr.as_bytes())
}
