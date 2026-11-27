//! The bytes that the files under a path take on disk, which their removal would free where nothing else holds them.
//!
//! A block that several files share is a [`Piece`] of the [`Usage`] of each, counted once by whoever sums them.
//!   - A file with several links is a [`Key::Inode`], which holds as many files as its links.
//!   - On APFS, a file that shares all its blocks with clones is a [`Key::Clone`], which holds as many as its clones.
//!   - On XFS and btrfs, an extent that FIEMAP flags as shared is a [`Key::Extent`], whose holders are not known.
//!
//! A clone that was partly written shares the rest of its blocks with its source, which only XFS and btrfs report. On
//! APFS it counts all of its blocks, because the private size of a file can lag behind a write.

use std::fs;
use std::path::Path;

/// What shares a [`Piece`] with other files, which no two pieces of different blocks have.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    Inode(u64),
    Clone(u64),
    Extent(u64),
}

/// Blocks that other files may share, which take `bytes` on disk once whatever the number of files that hold them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Piece {
    pub key: Key,
    /// The number of files that hold the blocks, where the filesystem tells it.
    pub holders: Option<u64>,
    pub bytes: u64,
}

/// The bytes that only the files under a path hold, and the pieces that they may share with other files.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub own: u64,
    pub shared: Vec<Piece>,
}

impl Piece {
    /// The piece as one word without spaces or tabs, which [`Piece::parse`] reads back.
    pub fn word(&self) -> String {
        let (kind, key) = match self.key {
            Key::Inode(key) => ('i', key),
            Key::Clone(key) => ('c', key),
            Key::Extent(key) => ('e', key),
        };
        let holders = self.holders.map_or_else(|| "-".to_string(), |holders| holders.to_string());
        format!("{kind}{key}:{holders}:{}", self.bytes)
    }

    pub fn parse(word: &str) -> Option<Piece> {
        let mut fields = word.split(':');
        let (key, holders, bytes) = (fields.next()?, fields.next()?, fields.next()?);
        let number = key.get(1..)?.parse().ok()?;
        let key = match key.get(..1)? {
            "i" => Key::Inode(number),
            "c" => Key::Clone(number),
            "e" => Key::Extent(number),
            _ => return None,
        };
        let holders = if holders == "-" { None } else { Some(holders.parse().ok()?) };
        Some(Piece { key, holders, bytes: bytes.parse().ok()? })
    }

    /// Whether the files that `held` counts hold all of the blocks, so that their removal would free them.
    ///
    /// An extent without a count of holders is held elsewhere too where only one file is known to hold it, because
    /// FIEMAP flags it as shared. Where two files hold it, it counts as theirs alone.
    pub fn freed_by(&self, held: u64) -> bool {
        self.holders.map_or(held >= 2, |holders| held >= holders)
    }
}

/// The usage of the file or the files under the directory at `path`, without following a symlink.
pub fn measure(path: &Path) -> Usage {
    let mut usage = Usage::default();
    platform::measure(path, &mut usage);
    usage
}

/// The bytes that `file` takes on disk, which exceed its length where it is smaller than a block.
///
/// A memo of 32 bytes takes a block of 4 KiB on APFS, ext4 and XFS. A filesystem that keeps a small file in its
/// metadata, as btrfs does, allocates no block to it, so the length is the least that a file counts.
#[cfg(unix)]
pub fn allocated(file: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    file.len().max(file.blocks() * 512)
}

/// The length of `file`, because NTFS keeps a small file within its record in the master file table.
#[cfg(windows)]
pub fn allocated(file: &fs::Metadata) -> u64 {
    file.len()
}

#[cfg(not(target_os = "macos"))]
fn children(directory: &Path) -> Vec<std::path::PathBuf> {
    Vec::from_iter(
        fs::read_dir(directory).into_iter().flatten().flatten().map(|entry| entry.path()),
    )
}

/// The usage that a file of `links` links, of the identity `inode`, adds where `alone` is its usage as one link.
fn linked(usage: &mut Usage, inode: u64, links: u64, alone: u64) {
    if links > 1 {
        usage.shared.push(Piece { key: Key::Inode(inode), holders: Some(links), bytes: alone });
    } else {
        usage.own += alone;
    }
}

#[cfg(target_os = "macos")]
mod platform {
    //! The walk of APFS, which asks a directory for the clone ids and clone counts of its files in bulk.

    use std::ffi::CString;
    use std::fs;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::Path;

    use super::{Key, Piece, Usage};

    /// The count of the full clones of a file, itself included, which libc does not define.
    const ATTR_CMNEXT_CLONE_REFCNT: u32 = 0x0000_1000;
    const VREG: u32 = 1;
    const VDIR: u32 = 2;

    /// What one record of the attributes in [`attributes`] says about a file.
    struct Record {
        name: Vec<u8>,
        kind: u32,
        inode: u64,
        links: u64,
        allocated: u64,
        /// The clone id and clone count, where the filesystem reports them.
        clone: Option<(u64, u64)>,
    }

    fn attributes() -> libc::attrlist {
        libc::attrlist {
            bitmapcount: libc::ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: libc::ATTR_CMN_RETURNED_ATTRS
                | libc::ATTR_CMN_NAME
                | libc::ATTR_CMN_OBJTYPE
                | libc::ATTR_CMN_FILEID,
            volattr: 0,
            dirattr: 0,
            fileattr: libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_DATAALLOCSIZE,
            forkattr: libc::ATTR_CMNEXT_CLONEID | ATTR_CMNEXT_CLONE_REFCNT,
        }
    }

    /// The options of both calls, under which a record holds every attribute, and its mask says which are valid.
    const OPTIONS: u32 =
        libc::FSOPT_NOFOLLOW | libc::FSOPT_PACK_INVAL_ATTRS | libc::FSOPT_ATTR_CMN_EXTENDED;

    /// The record at the start of `bytes`, whose attributes lie in the order of their bits in [`attributes`].
    ///
    /// The offsets follow from that order, with the length and the returned mask first.
    fn record(bytes: &[u8]) -> Option<Record> {
        let u32_at = |at: usize| Some(u32::from_ne_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
        let u64_at = |at: usize| Some(u64::from_ne_bytes(bytes.get(at..at + 8)?.try_into().ok()?));
        // The length, then the returned mask of five groups, then a reference to the name.
        let (file_mask, fork_mask) = (u32_at(16)?, u32_at(20)?);
        let offset = i32::from_ne_bytes(bytes.get(24..28)?.try_into().ok()?);
        let length = usize::try_from(u32_at(28)?).ok()?;
        let start = usize::try_from(24 + offset).ok()?;
        let name = bytes.get(start..start + length)?;
        let name = name.strip_suffix(&[0]).unwrap_or(name).to_vec();
        let (kind, inode) = (u32_at(32)?, u64_at(36)?);
        // A directory has no attributes of a file, which the record leaves out rather than packs.
        if kind == VDIR {
            return Some(Record { name, kind, inode, links: 1, allocated: 0, clone: None });
        }
        let (links, allocated) = (u32_at(44)?, u64_at(48)?);
        let (id, count) = (u64_at(56)?, u32_at(64)?);
        let all = libc::ATTR_CMNEXT_CLONEID | ATTR_CMNEXT_CLONE_REFCNT;
        let clone = (fork_mask & all == all).then_some((id, u64::from(count)));
        let links = if file_mask & libc::ATTR_FILE_LINKCOUNT != 0 { u64::from(links) } else { 1 };
        Some(Record { name, kind, inode, links, allocated, clone })
    }

    fn add(usage: &mut Usage, record: &Record) {
        match record.clone {
            Some((id, clones)) if clones > 1 && record.links <= 1 => {
                let piece =
                    Piece { key: Key::Clone(id), holders: Some(clones), bytes: record.allocated };
                usage.shared.push(piece);
            }
            _ => super::linked(usage, record.inode, record.links, record.allocated),
        }
    }

    pub fn measure(path: &Path, usage: &mut Usage) {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() => directory(path, usage),
            Ok(metadata) => match single(path) {
                Some(record) => add(usage, &record),
                None => super::linked(
                    usage,
                    metadata.ino(),
                    metadata.nlink(),
                    super::allocated(&metadata),
                ),
            },
            Err(_) => {}
        }
    }

    #[allow(unsafe_code)]
    fn single(path: &Path) -> Option<Record> {
        let path = CString::new(path.as_os_str().as_bytes()).ok()?;
        let (mut list, mut buffer) = (attributes(), [0u8; 1024]);
        // SAFETY: `path` is NUL-terminated, and the call writes at most the length of the buffer that it is given.
        let asked = unsafe {
            libc::getattrlist(
                path.as_ptr(),
                (&raw mut list).cast(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                OPTIONS,
            )
        };
        if asked != 0 {
            return None;
        }
        record(&buffer)
    }

    /// Adds the files under `path`, reading the records of each directory in bulk, and its subdirectories after it.
    #[allow(unsafe_code)]
    fn directory(path: &Path, usage: &mut Usage) {
        let flags = libc::O_DIRECTORY | libc::O_NOFOLLOW;
        let Ok(directory) = fs::OpenOptions::new().read(true).custom_flags(flags).open(path) else {
            return;
        };
        let (mut list, mut buffer) = (attributes(), vec![0u8; 128 * 1024]);
        let mut below = Vec::new();
        loop {
            // SAFETY: the descriptor is an open directory, and the call writes at most the length of the buffer.
            let count = unsafe {
                libc::getattrlistbulk(
                    directory.as_raw_fd(),
                    (&raw mut list).cast(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    u64::from(OPTIONS),
                )
            };
            if count <= 0 {
                break;
            }
            let mut at = 0;
            for _ in 0..count {
                let Some(length) = buffer.get(at..at + 4).and_then(|length| length.try_into().ok())
                else {
                    break;
                };
                let length = u32::from_ne_bytes(length) as usize;
                let record = buffer.get(at..at + length).and_then(record);
                at += length;
                let Some(record) = record else { continue };
                match record.kind {
                    VDIR => below.push(path.join(std::ffi::OsStr::from_bytes(&record.name))),
                    VREG => add(usage, &record),
                    // A symlink takes at most the block of its target's name.
                    _ => usage.own += record.allocated,
                }
            }
        }
        drop(directory);
        for path in below {
            self::directory(&path, usage);
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    //! The walk of Linux, which asks FIEMAP for the shared extents of each file where the filesystem clones.

    use std::fs;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    use super::{Key, Piece, Usage};

    const XFS: i64 = 0x5846_5342;
    const BTRFS: i64 = 0x9123_683E;

    /// `_IOWR('f', 11, struct fiemap)`, which libc does not define.
    const FS_IOC_FIEMAP: u64 = 0xC020_660B;
    const LAST: u32 = 0x1;
    /// The extent has no place on disk yet, as before writeback, or its place is not known.
    const PLACELESS: u32 = 0x2 | 0x4;
    const SHARED: u32 = 0x2000;
    const EXTENTS: usize = 64;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Extent {
        logical: u64,
        physical: u64,
        length: u64,
        reserved: [u64; 2],
        flags: u32,
        padding: [u32; 3],
    }

    #[repr(C)]
    struct Map {
        start: u64,
        length: u64,
        flags: u32,
        mapped: u32,
        count: u32,
        reserved: u32,
        extents: [Extent; EXTENTS],
    }

    pub fn measure(path: &Path, usage: &mut Usage) {
        let clones = clones(path);
        walk(path, clones, usage);
    }

    /// Whether the filesystem of `path` can share blocks between files, as XFS and btrfs do and ext4 does not.
    #[allow(unsafe_code, clippy::unnecessary_cast)]
    fn clones(path: &Path) -> bool {
        let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return false };
        let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: `path` is NUL-terminated, and a call that returns 0 has written `stats`.
        if unsafe { libc::statfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
            return false;
        }
        // SAFETY: the call succeeded.
        let kind = unsafe { stats.assume_init() }.f_type as i64;
        kind == XFS || kind == BTRFS
    }

    fn walk(path: &Path, clones: bool, usage: &mut Usage) {
        let Ok(metadata) = fs::symlink_metadata(path) else { return };
        if metadata.is_dir() {
            for child in super::children(path) {
                walk(&child, clones, usage);
            }
        } else if metadata.is_file() && clones && metadata.nlink() <= 1 && metadata.blocks() > 0 {
            if !extents(path, usage) {
                usage.own += super::allocated(&metadata);
            }
        } else {
            super::linked(usage, metadata.ino(), metadata.nlink(), super::allocated(&metadata));
        }
    }

    /// Adds the extents of the file at `path`, or returns `false` where FIEMAP cannot map it.
    #[allow(unsafe_code, clippy::cast_lossless)]
    fn extents(path: &Path, usage: &mut Usage) -> bool {
        let Ok(file) = fs::File::open(path) else { return false };
        let mut map = Map {
            start: 0,
            length: u64::MAX,
            flags: 0,
            mapped: 0,
            count: EXTENTS as u32,
            reserved: 0,
            extents: [Extent::default(); EXTENTS],
        };
        let mut added = Usage::default();
        loop {
            (map.length, map.mapped) = (u64::MAX - map.start, 0);
            // SAFETY: the descriptor is an open file, and the call writes at most `count` extents into `map`.
            if unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_FIEMAP as _, &raw mut map) } != 0 {
                return false;
            }
            let mapped = &map.extents[..(map.mapped as usize).min(EXTENTS)];
            for extent in mapped {
                if extent.flags & SHARED != 0 && extent.flags & PLACELESS == 0 {
                    let key = Key::Extent(extent.physical);
                    added.shared.push(Piece { key, holders: None, bytes: extent.length });
                } else {
                    added.own += extent.length;
                }
            }
            match mapped.last() {
                Some(last) if last.flags & LAST == 0 => map.start = last.logical + last.length,
                _ => break,
            }
        }
        usage.own += added.own;
        usage.shared.extend(added.shared);
        true
    }
}

#[cfg(windows)]
mod platform {
    //! The walk of Windows, where NTFS does not clone, so only links share blocks.

    use std::fs;
    use std::path::Path;

    use super::Usage;

    pub fn measure(path: &Path, usage: &mut Usage) {
        let Ok(metadata) = fs::symlink_metadata(path) else { return };
        if metadata.is_dir() {
            for child in super::children(path) {
                measure(&child, usage);
            }
            return;
        }
        let (index, links) = crate::windows::information(path, false)
            .map_or((0, 1), |(_, index, links, _)| (index, u64::from(links.max(1))));
        super::linked(usage, index, links, super::allocated(&metadata));
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
mod platform {
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    use super::Usage;

    pub fn measure(path: &Path, usage: &mut Usage) {
        let Ok(metadata) = fs::symlink_metadata(path) else { return };
        if metadata.is_dir() {
            for child in super::children(path) {
                measure(&child, usage);
            }
        } else {
            super::linked(usage, metadata.ino(), metadata.nlink(), super::allocated(&metadata));
        }
    }
}
