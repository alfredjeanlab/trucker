//! The Windows twins of the Unix system calls that trucker makes, and the stand-ins of what Windows does not support.
//!
//! On Windows only third-party crates and the runs of their build scripts are cached. An own crate runs as plain rustc
//! would, so [`session`] and [`lazy`] never carry a session or record a candidate. A root of a cached crate must be
//! spelled as Windows spells it itself, as [`is_canonical`] checks, because the scans for baked paths rely on it.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};
use std::{env, fs, io, thread};

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_INVALID_FUNCTION, ERROR_LOCK_VIOLATION, ERROR_NOT_SUPPORTED,
    ERROR_SHARING_VIOLATION, HANDLE,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_SPARSE_FILE, FILE_BASIC_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    FILE_WRITE_ATTRIBUTES, FileBasicInfo, GetDiskFreeSpaceExW, GetFileInformationByHandle,
    GetFileInformationByHandleEx, GetShortPathNameW, GetVolumeInformationByHandleW,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DUPLICATE_EXTENTS_DATA, FSCTL_DUPLICATE_EXTENTS_TO_FILE, FSCTL_GET_INTEGRITY_INFORMATION,
    FSCTL_GET_INTEGRITY_INFORMATION_BUFFER, FSCTL_SET_INTEGRITY_INFORMATION,
    FSCTL_SET_INTEGRITY_INFORMATION_BUFFER, FSCTL_SET_SPARSE,
};
use windows_sys::Win32::System::SystemServices::FILE_SUPPORTS_BLOCK_REFCOUNTING;

use crate::unit::Roots;
use crate::windows_scan::Scan;

/// The longest root that a cached crate may have, which keeps the paths of its outputs well under `MAX_PATH`.
const LONGEST_ROOT: usize = 200;

/// The most bytes that one call duplicates, a multiple of every cluster size that stays under the limit of 4 GiB.
const CHUNK: u64 = 1 << 31;

/// The seconds between 1601, where a `FILETIME` starts, and 1970.
const EPOCH_GAP: i64 = 11_644_473_600;

fn wide(path: &Path) -> Vec<u16> {
    Vec::from_iter(path.as_os_str().encode_wide().chain([0]))
}

/// The file or directory at `path`, opened only to read or set its attributes, and a link itself unless `follow`.
fn attributes(path: &Path, access: u32, follow: bool) -> io::Result<fs::File> {
    let link = if follow { 0 } else { FILE_FLAG_OPEN_REPARSE_POINT };
    fs::OpenOptions::new()
        .access_mode(access)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | link)
        .open(path)
}

/// The volume serial, file index, link count and length of the file at `path`, or of a link itself unless `follow`.
#[allow(unsafe_code)]
pub fn information(path: &Path, follow: bool) -> Option<(u32, u64, u32, u64)> {
    let file = attributes(path, FILE_READ_ATTRIBUTES, follow).ok()?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle is open, and `info` is a buffer of the type that the call writes.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) } == 0 {
        return None;
    }
    let index = u64::from(info.nFileIndexHigh) << 32 | u64::from(info.nFileIndexLow);
    let length = u64::from(info.nFileSizeHigh) << 32 | u64::from(info.nFileSizeLow);
    Some((info.dwVolumeSerialNumber, index, info.nNumberOfLinks, length))
}

/// Whether `a` and `b` lie on one volume.
pub fn same_volume(a: &Path, b: &Path) -> bool {
    matches!((information(a, true), information(b, true)), (Some(a), Some(b)) if a.0 == b.0)
}

/// The change time of `path` itself in seconds since 1970, which a copy does not carry over from its source.
#[allow(unsafe_code)]
pub fn change_time(path: &Path) -> Option<u64> {
    let file = attributes(path, FILE_READ_ATTRIBUTES, false).ok()?;
    let mut info = FILE_BASIC_INFO::default();
    let size = size_of::<FILE_BASIC_INFO>() as u32;
    let buffer = (&raw mut info).cast::<c_void>();
    // SAFETY: the handle is open, and `buffer` holds `size` bytes of the type that the class names.
    if unsafe {
        GetFileInformationByHandleEx(file.as_raw_handle() as HANDLE, FileBasicInfo, buffer, size)
    } == 0
    {
        return None;
    }
    u64::try_from(info.ChangeTime / 10_000_000 - EPOCH_GAP).ok()
}

/// Sets the mtime of the file or directory at `path` to now.
///
/// A handle that only reads cannot set a time on Windows, and a directory opens only with backup semantics.
pub fn touch(path: &Path) -> io::Result<()> {
    attributes(path, FILE_WRITE_ATTRIBUTES, true)?.set_modified(SystemTime::now())
}

/// Copies `source` to `destination` with the current mtime, which a copy on Windows would otherwise carry over.
pub fn copy(source: &Path, destination: &Path) -> io::Result<()> {
    fs::copy(source, destination)?;
    touch(destination)
}

/// Renames `from` to `to` as `fs::rename` does, and tries again while another process holds a file open.
///
/// A scanner such as Defender opens a file once it is closed, and a directory then cannot move. The tries wait 150 ms
/// in all, and then the last error stands. A directory at `to` is never replaced, so that rename fails at once.
pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    let (from, to) = (from.as_ref(), to.as_ref());
    let busy = [ERROR_SHARING_VIOLATION, ERROR_ACCESS_DENIED, ERROR_LOCK_VIOLATION]
        .map(|code| Some(code as i32));
    let mut wait = Duration::from_millis(10);
    loop {
        match fs::rename(from, to) {
            Err(error)
                if wait <= Duration::from_millis(80)
                    && busy.contains(&error.raw_os_error())
                    && !to.is_dir() =>
            {
                thread::sleep(wait);
                wait *= 2;
            }
            renamed => return renamed,
        }
    }
}

fn unsupported() -> io::Error {
    io::ErrorKind::Unsupported.into()
}

/// Sends the control `code` with `input` to the open `file`.
#[allow(unsafe_code)]
fn control<T>(
    file: &fs::File,
    code: u32,
    input: Option<&T>,
    output: Option<&mut T>,
) -> io::Result<()> {
    let size = size_of::<T>() as u32;
    let (input_size, output_size) =
        (if input.is_some() { size } else { 0 }, if output.is_some() { size } else { 0 });
    let input = input.map_or(std::ptr::null(), |input| (input as *const T).cast::<c_void>());
    let output = output.map_or(std::ptr::null_mut(), |output| (output as *mut T).cast::<c_void>());
    let mut returned = 0;
    let handle = file.as_raw_handle() as HANDLE;
    // SAFETY: the handle is open, each buffer is null or holds its stated size, and the call is synchronous.
    let done = unsafe {
        DeviceIoControl(
            handle,
            code,
            input,
            input_size,
            output,
            output_size,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if done != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    let refused = [ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED].map(|code| Some(code as i32));
    Err(if refused.contains(&error.raw_os_error()) { unsupported() } else { error })
}

/// Whether the volume of the open `file` can share blocks between files.
#[allow(unsafe_code)]
fn clones(file: &fs::File) -> bool {
    let mut flags = 0;
    let null = std::ptr::null_mut();
    // SAFETY: the handle is open, and every buffer that is not null is a `u32` that the call writes.
    let asked = unsafe {
        GetVolumeInformationByHandleW(
            file.as_raw_handle() as HANDLE,
            null,
            0,
            null.cast(),
            null.cast(),
            &mut flags,
            null,
            0,
        )
    };
    asked != 0 && flags & FILE_SUPPORTS_BLOCK_REFCOUNTING != 0
}

/// Clones `source` to `destination` with `FSCTL_DUPLICATE_EXTENTS_TO_FILE`, as a file that shares its blocks.
///
/// `destination` must not exist, and gets the current mtime. Fails as unsupported on a volume that cannot clone, such as
/// NTFS or a Dev Drive before Windows 11 24H2. ReFS clones whole clusters, so the clone is sized up to a cluster for the
/// calls and cut back to the length of `source` after them.
pub fn clone(source: &Path, destination: &Path) -> io::Result<()> {
    let from = fs::File::open(source)?;
    if !clones(&from) {
        return Err(unsupported());
    }
    let metadata = from.metadata()?;
    let mut integrity = FSCTL_GET_INTEGRITY_INFORMATION_BUFFER::default();
    control(&from, FSCTL_GET_INTEGRITY_INFORMATION, None, Some(&mut integrity))?;
    let to = fs::OpenOptions::new().read(true).write(true).create_new(true).open(destination)?;
    let cloned = (|| {
        if metadata.file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE != 0 {
            control::<u8>(&to, FSCTL_SET_SPARSE, None, None)?;
        }
        let setting = FSCTL_SET_INTEGRITY_INFORMATION_BUFFER {
            ChecksumAlgorithm: integrity.ChecksumAlgorithm,
            Reserved: 0,
            Flags: integrity.Flags,
        };
        control(&to, FSCTL_SET_INTEGRITY_INFORMATION, Some(&setting), None)?;
        let cluster = u64::from(integrity.ClusterSizeInBytes.max(1));
        let rounded = metadata.len().div_ceil(cluster) * cluster;
        to.set_len(rounded)?;
        let mut offset = 0;
        while offset < rounded {
            let count = (rounded - offset).min(CHUNK);
            let data = DUPLICATE_EXTENTS_DATA {
                FileHandle: from.as_raw_handle() as HANDLE,
                SourceFileOffset: offset as i64,
                TargetFileOffset: offset as i64,
                ByteCount: count as i64,
            };
            control(&to, FSCTL_DUPLICATE_EXTENTS_TO_FILE, Some(&data), None)?;
            offset += count;
        }
        to.set_len(metadata.len())?;
        to.set_modified(SystemTime::now())
    })();
    if cloned.is_err() {
        drop(to);
        let _ = fs::remove_file(destination);
    }
    cloned
}

/// Runs `command` and returns its exit code, because Windows cannot replace a process with another.
///
/// Ctrl-C reaches the child through the console, so this process ignores it and waits for the code of the child. It
/// starts to ignore Ctrl-C only once the child runs, because a child inherits that setting.
#[allow(unsafe_code)]
pub fn run(command: &mut Command) -> io::Result<i32> {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    let mut child = command.spawn()?;
    // SAFETY: a null handler with TRUE only makes this process ignore Ctrl-C.
    unsafe { SetConsoleCtrlHandler(None, 1) };
    Ok(child.wait()?.code().unwrap_or(1))
}

/// The process ID of the parent of this process, or 0 where it cannot be found.
#[allow(unsafe_code)]
pub fn parent_id() -> u32 {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    let id = std::process::id();
    // SAFETY: the snapshot takes no pointers, and its handle is closed below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return 0;
    }
    let mut entry =
        PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
    let mut parent = 0;
    // SAFETY: the snapshot is open, and `entry` is a buffer of the type that the calls write, with its size set.
    let mut found = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while found {
        if entry.th32ProcessID == id {
            parent = entry.th32ParentProcessID;
            break;
        }
        // SAFETY: as for the first call.
        found = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    // SAFETY: the snapshot is open and is not used again.
    unsafe { CloseHandle(snapshot) };
    parent
}

/// The total size and the free space in bytes of the volume of `path`, as this user sees them.
///
/// Both are zero where the volume cannot be asked.
#[allow(unsafe_code)]
pub fn space(path: &Path) -> (u64, u64) {
    let (mut available, mut total) = (0, 0);
    // SAFETY: the path is NUL-terminated, and each pointer that is not null is a `u64` that the call writes.
    let asked = unsafe {
        GetDiskFreeSpaceExW(wide(path).as_ptr(), &mut available, &mut total, std::ptr::null_mut())
    };
    if asked == 0 { (0, 0) } else { (total, available) }
}

/// The short (8.3) spelling of `root`, where its volume gives it one that differs.
#[allow(unsafe_code)]
fn short_spelling(root: &str) -> Option<String> {
    let long = wide(Path::new(root));
    // SAFETY: the path is NUL-terminated, and a null buffer of length 0 asks only for the length.
    let length = unsafe { GetShortPathNameW(long.as_ptr(), std::ptr::null_mut(), 0) };
    let mut short = vec![0u16; usize::try_from(length).ok()?.max(1)];
    // SAFETY: `short` holds the length that the first call asked for.
    let written = unsafe { GetShortPathNameW(long.as_ptr(), short.as_mut_ptr(), length) };
    short.truncate(usize::try_from(written).ok().filter(|written| *written < short.len())?);
    let short = String::from_utf16(&short).ok()?;
    (written != 0 && !short.eq_ignore_ascii_case(root)).then_some(short)
}

/// Whether `root`, which ends in `\`, is spelled as Windows spells it once it resolves every link and drive mapping.
///
/// It must be a local path of a drive letter, in ASCII without a space, `/` or quote, and at most [`LONGEST_ROOT`]
/// long. Any other spelling runs uncached, as that of a `subst` or network drive, a short name or another case does.
///
/// The drive letter alone may be in either case. The root keeps the case that cargo gave it, because the placeholders
/// and the remap match the paths of rustc byte by byte, and the scans leave out the drive.
pub fn is_canonical(root: &str) -> bool {
    let bytes = root.as_bytes();
    let plain = root.len() <= LONGEST_ROOT
        && root.chars().all(|c| c.is_ascii_graphic() && !"/\"*?<>|".contains(c))
        && bytes.len() > 3
        && bytes[0].is_ascii_alphabetic()
        && root[1..].starts_with(":\\")
        && root.ends_with('\\')
        && !root[2..].contains("\\\\");
    let resolved =
        || fs::canonicalize(root).ok()?.to_str()?.strip_prefix(r"\\?\").map(str::to_string);
    let same = |resolved: String| {
        resolved.as_bytes().first().is_some_and(|drive| drive.eq_ignore_ascii_case(&bytes[0]))
            && resolved.get(1..) == Some(root[1..].trim_end_matches('\\'))
    };
    plain && resolved().is_some_and(same)
}

/// Whether a key can cover a crate with `roots` and the keyed `values`, which name paths in its arguments or variables.
///
/// Both roots must be canonical. A key hashes what a value names under the target directory only where the value starts
/// with the root byte for byte, so a value must not name it in another spelling, such as `C:` for `c:` or `/` for `\`.
pub fn is_keyable<'a>(roots: &Roots, values: impl IntoIterator<Item = &'a str>) -> bool {
    let other_spelling = |scan: Scan| {
        values.into_iter().any(|value| scan.contains(roots.normalized(value).as_bytes()))
    };
    is_canonical(&roots.target)
        && is_canonical(&roots.cargo_home)
        && !other_spelling(scan(&[&roots.target]))
}

/// The scan for baked paths that finds `roots` and their short spellings.
pub fn scan(roots: &[&str]) -> Scan {
    let short = Vec::from_iter(roots.iter().filter_map(|root| short_spelling(root)));
    Scan::new(&Vec::from_iter(roots.iter().copied().chain(short.iter().map(String::as_str))))
}

/// Whether `text` holds a spelling of any of `roots`, as [`scan`] finds them.
pub fn contains(text: &str, roots: &[&str]) -> bool {
    scan(roots).contains(text.as_bytes())
}

/// The versions of MSVC and of the Windows SDK that are installed, one per line.
///
/// The `cc` crate finds MSVC through Visual Studio rather than PATH, and each update installs a version of its own, so
/// the key of a build script's run changes with an update.
pub fn toolchains() -> String {
    let mut found = Vec::new();
    let roots =
        ["ProgramFiles", "ProgramFiles(x86)"].map(|name| env::var_os(name).map(PathBuf::from));
    let children = |directory: &Path| {
        Vec::from_iter(
            fs::read_dir(directory).into_iter().flatten().flatten().map(|entry| entry.path()),
        )
    };
    for root in roots.into_iter().flatten() {
        for edition in
            children(&root.join("Microsoft Visual Studio")).iter().flat_map(|year| children(year))
        {
            found.extend(children(&edition.join("VC").join("Tools").join("MSVC")));
        }
        found.extend(children(&root.join("Windows Kits").join("10").join("Lib")));
    }
    found.sort();
    String::from_iter(found.iter().map(|path| format!("{}\n", path.display())))
}

/// `%LOCALAPPDATA%`, which holds the store unless `XDG_CACHE_HOME` names another directory.
///
/// A target directory on another volume, such as a Dev Drive, leaves its crates uncached as `other-filesystem`.
pub fn cache_home() -> PathBuf {
    let home = || env::home_dir().unwrap_or_default().join("AppData").join("Local");
    env::var_os("LOCALAPPDATA").map_or_else(home, PathBuf::from)
}

/// CARGO_HOME with a trailing `\`.
pub fn cargo_home() -> String {
    let home = env::var("CARGO_HOME").ok().or_else(|| {
        let home = env::home_dir()?.join(".cargo");
        home.to_str().map(str::to_string)
    });
    format!("{}\\", home.unwrap_or_default().trim_end_matches(['\\', '/']))
}

/// Carried incremental sessions, which Windows does not support, so no compile ever takes a slot.
pub mod session {
    use std::convert::Infallible;
    use std::ffi::OsString;
    use std::io;
    use std::path::{Path, PathBuf};

    use crate::store::Files;
    use crate::unit::Unit;

    /// A slot, which cannot exist on Windows.
    pub struct Slot {
        pub path: String,
        pub directory: PathBuf,
        pub restored: bool,
        never: Infallible,
    }

    impl Slot {
        pub fn take(_: &Unit, _: &[String], _: &Path, _: Files) -> Option<Slot> {
            None
        }

        pub fn localized(&self, _: &str) -> String {
            match self.never {}
        }

        pub fn args(&self, _: &[String]) -> Vec<String> {
            match self.never {}
        }

        pub fn environment(&self, _: &[String]) -> Vec<(OsString, String)> {
            match self.never {}
        }

        pub fn keep(&self) -> io::Result<()> {
            match self.never {}
        }

        pub fn time(&self, _: std::time::Duration) {
            match self.never {}
        }

        pub fn reads_itself(&self, _: &Unit) -> bool {
            match self.never {}
        }

        pub fn bake(&mut self) {
            match self.never {}
        }

        pub fn discard(&mut self) -> io::Result<()> {
            match self.never {}
        }
    }

    /// Does nothing, because no compile on Windows runs its linker through trucker.
    pub fn link() {}

    /// Does nothing, because no reader is started on Windows.
    pub fn read() {}

    /// Does nothing, because the page cache of a clone on Windows is not measured.
    pub fn read_later(_: &Unit, _: &Path) {}

    /// Whether a compile of an own crate would be carried but for ThinLTO, which never holds without sessions.
    pub fn local_thin_lto(_: &[String]) -> bool {
        false
    }
}

/// Candidates of own crates, which Windows never records, because an own crate runs as plain rustc.
pub mod lazy {
    use std::path::{Path, PathBuf};
    use std::time::SystemTime;

    use crate::store::Files;
    use crate::unit::Unit;

    pub fn digests(_: &[PathBuf], _: &[&str]) -> Option<(Vec<String>, bool)> {
        None
    }

    pub fn record(
        _: &Unit,
        _: &Path,
        _: &str,
        _: (&[u8], &str),
        _: (&[PathBuf], &[PathBuf], Option<Vec<String>>),
        _: SystemTime,
        _: Option<&str>,
    ) -> Result<(), &'static str> {
        Err("eager")
    }

    pub fn promote(_: &Path, _: &str, _: &Path, _: Files) -> bool {
        false
    }

    /// Whether a candidate may still make its entry, which none does on Windows.
    pub fn is_alive(_: &Path) -> bool {
        false
    }
}
