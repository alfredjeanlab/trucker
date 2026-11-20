//! Candidates, each of which stands for the entry of an own crate by naming its outputs where its compile left them.
//!
//! A rebuild of an own crate records a candidate instead of storing its outputs, so an edit pays for neither the scan
//! for baked paths nor the clones, and the store does not grow with it. The record holds the hash and mode of each
//! output. A miss of the entry clones the outputs into staging, or copies them where the store cannot clone. It
//! promotes the clones only where each has the recorded hash and mode and the scan for baked paths passes.
//!
//! The clone is checked and not the output, because no other process writes the clone while cargo, a linker or an
//! editor may still change the output. Timestamps and links do not decide anything, because cargo sets back the mtime
//! of a check's `.rmeta` and hard-links each output that it uplifts once rustc ends.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;
use std::{fs, process, thread};

use crate::store::{self, Files, HASHES, RECORD, STDERR};
use crate::unit::Unit;

#[derive(Default)]
pub struct Candidate {
    variant: String,
    /// The worktree, separate target directory and slot, whose paths an output must not contain.
    roots: Vec<String>,
    /// The size, mode and hash of each output other than dep-info, and its path.
    outputs: Vec<(u64, u32, String, PathBuf)>,
    /// The name and contents of the entry's dep-info, stderr and record, as an entry stores them.
    files: Vec<(String, Vec<u8>)>,
}

/// The file of the candidate of `key` whose outputs lie in `out_dir`.
///
/// A key has one candidate per output directory.
fn path_of(entries: &Path, key: &str, out_dir: &Path) -> PathBuf {
    let hash = blake3::hash(out_dir.as_os_str().as_encoded_bytes()).to_hex();
    entries.join(format!("{key}.cand")).join(&hash[..16])
}

/// The hex hash of each of `paths` in order, and whether any of them contains the bytes of one of `texts`, as
/// [`scan`] finds them.
pub fn digests(paths: &[PathBuf], texts: &[&str]) -> Option<(Vec<String>, bool)> {
    let (hashes, found) = scan(paths, texts)?;
    Some((Vec::from_iter(hashes.iter().map(|hash| hash.to_hex().to_string())), found))
}

/// The hash of each of `paths` in order, and whether any of them contains the bytes of one of `texts`.
///
/// Every core works from one read of each file, and the result is `None` where a file cannot be read. A mebibyte of a
/// file is hashed as a BLAKE3 subtree and scanned while it is still in the cache, so the one large output of a crate
/// that links is hashed on every core too. Every file is open at once.
pub fn scan(paths: &[PathBuf], texts: &[&str]) -> Option<(Vec<blake3::Hash>, bool)> {
    use blake3::hazmat::{
        ChainingValue, HasherExt, Mode, merge_subtrees_non_root, merge_subtrees_root,
    };
    const SEGMENT: usize = 1 << 20;
    let finders = Vec::from_iter(texts.iter().map(memchr::memmem::Finder::new));
    let overlap = texts.iter().map(|text| text.len().saturating_sub(1)).max().unwrap_or(0);
    let open =
        |path| fs::File::open(path).and_then(|file| Ok((file.metadata()?.len() as usize, file)));
    let files = paths.iter().map(|path| open(path).ok()).collect::<Option<Vec<_>>>()?;
    let segments = files.iter().enumerate().flat_map(|(file, (length, _))| {
        (0..(*length).max(1)).step_by(SEGMENT).map(move |start| (file, start))
    });
    let segments = Vec::from_iter(segments);
    // A file of one segment is hashed whole. A segment of a longer file is a subtree, and BLAKE3 requires its offset to
    // be a multiple of its length. A segment is read and not mapped, because the faults of a map that every core reads
    // take a lock in turn.
    let digest = |(file, start): (usize, usize), buffer: &mut Vec<u8>| {
        use std::os::unix::fs::FileExt;
        let (length, file) = &files[file];
        let (from, end) = (start.saturating_sub(overlap), (*length).min(start + SEGMENT));
        buffer.resize(end - from, 0);
        file.read_exact_at(buffer, from as u64).ok()?;
        let found = finders.iter().any(|finder| finder.find(buffer).is_some());
        let (mut hasher, bytes) = (blake3::Hasher::new(), &buffer[start - from..]);
        if *length <= SEGMENT {
            return Some((Err(hasher.update(bytes).finalize()), found));
        }
        Some((Ok(hasher.set_input_offset(start as u64).update(bytes).finalize_non_root()), found))
    };
    let next = AtomicUsize::new(0);
    let worker = || {
        let (mut digests, mut buffer) = (Vec::new(), Vec::new());
        while let Some(&segment) = segments.get(next.fetch_add(1, Ordering::Relaxed)) {
            digests.push((segment, digest(segment, &mut buffer)?));
        }
        Some(digests)
    };
    let workers = thread::available_parallelism().map_or(1, usize::from).min(segments.len());
    let mut digests = thread::scope(|scope| {
        let threads = Vec::from_iter((1..workers).map(|_| scope.spawn(worker)));
        let mut digests = worker()?;
        for thread in threads {
            digests.extend(thread.join().ok()??);
        }
        Some(digests)
    })?;
    digests.sort_by_key(|(segment, _)| *segment);
    // The left subtree of BLAKE3 holds the largest power of two of segments that leaves the right one some input.
    fn halves(values: &[ChainingValue]) -> (&[ChainingValue], &[ChainingValue]) {
        values.split_at(1 << (usize::BITS - 1 - (values.len() - 1).leading_zeros()))
    }
    fn merge(values: &[ChainingValue]) -> ChainingValue {
        let [value] = values else {
            let (left, right) = halves(values);
            return merge_subtrees_non_root(&merge(left), &merge(right), Mode::Hash);
        };
        *value
    }
    let found = digests.iter().any(|(_, (_, found))| *found);
    let mut hashes = Vec::new();
    for file in 0..files.len() {
        let parts = digests.iter().filter(|((at, _), _)| *at == file).map(|(_, (part, _))| part);
        let hash = match Vec::from_iter(parts)[..] {
            [Err(hash)] => *hash,
            ref values @ [_, _, ..] => {
                let values = values.iter().map(|value| value.ok()).collect::<Option<Vec<_>>>()?;
                let (left, right) = halves(&values);
                merge_subtrees_root(&merge(left), &merge(right), Mode::Hash)
            }
            _ => return None,
        };
        hashes.push(hash);
    }
    Some((hashes, found))
}

/// The size and mode of the regular file at `path`.
fn shape(path: &Path) -> Option<(u64, u32)> {
    let file = fs::symlink_metadata(path).ok().filter(|file| file.file_type().is_file())?;
    Some((file.len(), file.permissions().mode()))
}

impl Candidate {
    fn encode(&self) -> Vec<u8> {
        let mut text = format!("variant {}\n", self.variant);
        text.extend(self.roots.iter().map(|root| format!("root {root}\n")));
        for (size, mode, hash, path) in &self.outputs {
            text += &format!("output {size} {mode} {hash} {}\n", path.display());
        }
        let mut bytes = text.into_bytes();
        for (name, contents) in &self.files {
            bytes.extend(format!("file {} {name}\n", contents.len()).as_bytes());
            bytes.extend(contents);
        }
        bytes
    }

    fn decode(mut bytes: &[u8]) -> Option<Candidate> {
        let mut candidate = Candidate::default();
        while let Some(end) = memchr::memchr(b'\n', bytes) {
            let (tag, rest) = std::str::from_utf8(&bytes[..end]).ok()?.split_once(' ')?;
            bytes = &bytes[end + 1..];
            match tag {
                "variant" => candidate.variant = rest.to_string(),
                "root" => candidate.roots.push(rest.to_string()),
                "output" => {
                    let fields = Vec::from_iter(rest.splitn(4, ' '));
                    let [size, mode, hash, path] = fields[..] else { return None };
                    let (size, mode) = (size.parse().ok()?, mode.parse().ok()?);
                    candidate.outputs.push((size, mode, hash.to_string(), PathBuf::from(path)));
                }
                "file" => {
                    let (length, name) = rest.split_once(' ')?;
                    let length = length.parse().ok()?;
                    candidate.files.push((name.to_string(), bytes.get(..length)?.to_vec()));
                    bytes = &bytes[length..];
                }
                _ => return None,
            }
        }
        let whole = bytes.is_empty() && !candidate.variant.is_empty();
        (whole && !candidate.outputs.is_empty()).then_some(candidate)
    }

    fn read(path: &Path) -> Option<Candidate> {
        Candidate::decode(&fs::read(path).ok()?)
    }

    /// Whether every output is still a regular file of the recorded size and mode.
    ///
    /// The check fails fast where an output is gone or rewritten, but it does not prove anything about the contents.
    fn is_alive(&self) -> bool {
        self.outputs.iter().all(|(size, mode, _, path)| shape(path) == Some((*size, *mode)))
    }

    /// Makes the entry of the candidate in `entries` from duplicates of its outputs, and returns whether it exists.
    ///
    /// Under [`Files::Links`] the duplicates are copies, and their hashes go to [`HASHES`] as in [`store::publish`],
    /// so a restore verifies the entry.
    fn store(&self, entries: &Path, key: &str, files: Files) -> bool {
        let entry = entries.join(&self.variant);
        if entry.is_dir() || !self.is_alive() {
            return entry.is_dir();
        }
        let staging = entries.join(format!("{key}.{}", process::id()));
        let name = |path: &Path| staging.join(path.file_name().unwrap_or_default());
        let staged = Vec::from_iter(self.outputs.iter().map(|(.., path)| name(path)));
        let duplicated = fs::create_dir(&staging).is_ok()
            && self.files.iter().all(|(name, bytes)| fs::write(staging.join(name), bytes).is_ok())
            && self
                .outputs
                .iter()
                .zip(&staged)
                .all(|((.., path), stored)| files.duplicate(path, stored).is_ok());
        let roots = Vec::from_iter(self.roots.iter().map(String::as_str));
        let hashes =
            duplicated.then(|| digests(&staged, &roots)).flatten().filter(|(hashes, found)| {
                !found
                    && hashes
                        .iter()
                        .zip(&self.outputs)
                        .all(|(hash, (.., recorded, _))| hash == recorded)
            });
        let listed = |hashes: &Vec<String>| {
            let names = staged.iter().map(|path| path.file_name().unwrap_or_default());
            let lines = hashes.iter().zip(names);
            String::from_iter(lines.map(|(hash, name)| format!("{hash} {}\n", name.display())))
        };
        let verified = hashes.is_some_and(|(hashes, _)| {
            files == Files::Clones || fs::write(staging.join(HASHES), listed(&hashes)).is_ok()
        }) && staged
            .iter()
            .zip(&self.outputs)
            .all(|(path, (size, mode, ..))| shape(path) == Some((*size, *mode)));
        let renamed = verified && fs::rename(&staging, &entry).is_ok();
        if let Some(store) = entries.parent().filter(|_| renamed) {
            let lengths = self.files.iter().map(|(_, bytes)| bytes.len() as u64);
            crate::gc::add_bytes(
                store,
                self.outputs.iter().map(|(size, ..)| size).sum::<u64>() + lengths.sum::<u64>(),
            );
        }
        let _ = fs::remove_dir_all(&staging);
        renamed || entry.is_dir()
    }
}

/// Records a candidate for the outputs of `unit` under `key`, in place of [`store::publish`].
///
/// The dep-info comes apart from the other outputs, which carry their hashes where they were computed. `slot` is the
/// slot that the compile ran in.
///
/// Fails as `publish` does where the compile cannot be stored. Fails with `eager` where the outputs cannot stand as a
/// candidate, so the caller stores them at once.
pub fn record(
    unit: &Unit,
    entries: &Path,
    key: &str,
    (stderr, record): (&[u8], &str),
    (dep_info, outputs, hashes): (&[PathBuf], &[PathBuf], Option<Vec<String>>),
    started: SystemTime,
    slot: Option<&str>,
) -> Result<(), &'static str> {
    let stderr = unit.roots.normalized(&String::from_utf8_lossy(stderr)).into_bytes();
    let mut files = vec![(STDERR.to_string(), stderr), (RECORD.to_string(), record.into())];
    let mut reads = String::new();
    for path in dep_info {
        let text = fs::read_to_string(path).ok().and_then(|text| unit.portable_dep_info(&text));
        let (text, read) = text.ok_or("unkeyed-input")?;
        reads = read;
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        files.push((name, text.into_bytes()));
    }
    let variant = unit.variant(key, &reads, Some(started), &entries.with_file_name("hashes"))?;
    let printable =
        outputs.iter().all(|output| output.to_str().is_some_and(|text| !text.contains('\n')));
    let shapes = outputs.iter().map(|output| shape(output)).collect::<Option<Vec<_>>>();
    let (Some(shapes), Some(hashes), true) = (shapes, hashes, printable) else {
        return Err("eager");
    };
    let outputs = shapes.into_iter().zip(hashes).zip(outputs);
    let outputs = outputs.map(|(((size, mode), hash), path)| (size, mode, hash, path.clone()));
    let outputs = outputs.collect();
    let roots = unit.roots.worktree.as_deref().into_iter().chain(unit.roots.separate_target());
    let roots = Vec::from_iter(roots.chain(slot).map(str::to_string));
    let candidate = Candidate { variant, roots, outputs, files };
    let path = path_of(entries, key, &unit.out_dir);
    let temporary = path.with_extension(process::id().to_string());
    let recorded = path.parent().is_some_and(|directory| fs::create_dir_all(directory).is_ok())
        && fs::File::create(&temporary)
            .and_then(|mut file| file.write_all(&candidate.encode()))
            .is_ok()
        && (reads.is_empty() || fs::write(store::reads_of(entries, key), &reads).is_ok())
        && fs::rename(&temporary, &path).is_ok();
    let _ = fs::remove_file(&temporary);
    if recorded { Ok(()) } else { Err("eager") }
}

/// Promotes the candidate of `key` that stands for `entry`, under the key's lock, and returns whether `entry` exists.
///
/// The candidate is removed, whether it made the entry or failed.
pub fn promote(entries: &Path, key: &str, entry: &Path, files: Files) -> bool {
    let variant = entry.file_name().unwrap_or_default().to_string_lossy();
    let candidates = fs::read_dir(entries.join(format!("{key}.cand"))).into_iter().flatten();
    for path in
        candidates.flatten().map(|file| file.path()).filter(|path| path.extension().is_none())
    {
        if let Some(candidate) =
            Candidate::read(&path).filter(|candidate| candidate.variant == variant)
        {
            let stored = candidate.store(entries, key, files);
            let _ = fs::remove_file(&path);
            if stored {
                return true;
            }
        }
    }
    false
}

/// Whether the candidate at `path` may still make its entry, as far as [`Candidate::is_alive`] can tell.
pub fn is_alive(path: &Path) -> bool {
    Candidate::read(path).is_some_and(|candidate| candidate.is_alive())
}
