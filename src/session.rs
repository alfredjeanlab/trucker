//! The incremental sessions of own crates, carried between worktrees through a pool of slot directories.
//!
//! rustc reuses a session only where the working directory, the remapped prefix and the paths in its arguments equal
//! those of the compile that wrote it. A carried compile therefore runs in a slot, a directory of the store that
//! mirrors the worktree and whose path replaces it in the arguments and environment. A worktree without a session for
//! the slot starts from a clone or copy of the session that the slot recorded, never from a hard link, or else from the
//! snapshot of the pool. A slot that cannot be set up leaves the compile uncarried.
//!
//! rustc deletes a session under an exclusive lock, and writes into it under a shared one. It also empties a session
//! that it cannot use, truncates a hard-linked object in place, and checks only that a work product exists. So a
//! session is taken under a shared lock, and used only where each file keeps its recorded length and mtime, which is
//! the bar of the freshness check of cargo. AGENTS.md gives the whole argument.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};
use std::{env, fs, io};

use crate::store::Files;
use crate::unit::{Unit, identity, identity_of};

/// The file among the sessions of a crate in a worktree that names the slot whose paths they were compiled with.
const SLOT: &str = "slot";

/// The directory in a slot that shares the libraries of a linking compile at their paths in the worktree.
///
/// It stays with the slot between compiles.
pub const LIBRARIES: &str = ".trucker-libraries";

/// The variable that gives the linker and the two paths to this program when it runs as the linker of a carried
/// compile.
const LINKER: &str = "TRUCKER_LINKER";

/// The last value of the codegen option `name` in `args`, empty where it has none.
fn codegen<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let values = args.iter().enumerate().filter_map(|(at, arg)| match arg.as_str() {
        "-C" => args.get(at + 1).map(String::as_str),
        _ => arg.strip_prefix("-C"),
    });
    let mut values = values.filter_map(|value| value.strip_prefix(name));
    let value = values.rfind(|rest| rest.is_empty() || rest.starts_with('='));
    value.map(|rest| rest.trim_start_matches('='))
}

/// Whether rustc runs ThinLTO between the codegen units of one crate, which rewrites the previous session in place.
///
/// It does at an opt-level above 0 without `-C lto` and with more than one codegen unit.
pub fn local_thin_lto(args: &[String]) -> bool {
    let codegen = |name| codegen(args, name);
    let optimized = codegen("opt-level").is_some_and(|level| level != "0");
    optimized && codegen("lto").is_none() && codegen("codegen-units") != Some("1")
}

/// A slot locked for one compile, with the worktree prepared to use its session.
pub struct Slot {
    /// The canonical path of the slot directory.
    pub path: String,
    /// The directory in the slot that rustc runs in, at the same place as cargo's working directory in the worktree.
    pub directory: PathBuf,
    /// That working directory relative to the worktree.
    relative: PathBuf,
    worktree: String,
    pool: PathBuf,
    /// The marker that [`Slot::bake`] writes, which [`baked`] names.
    baked: PathBuf,
    number: usize,
    /// The real directory of the crate's sessions in the worktree, which rustc reaches through the slot.
    sessions: PathBuf,
    /// The name of that directory, which follows `-C incremental` in the arguments.
    subdirectory: String,
    /// Whether the compile starts from a snapshot of the session that the slot recorded.
    pub restored: bool,
    /// Whether the compile starts without a session.
    cold: bool,
    /// Whether the compile links, so that it reads its libraries through [`LIBRARIES`].
    links: bool,
    files: Files,
    _lock: fs::File,
}

impl Slot {
    /// Locks a slot for the compile of `unit` with `args`, or is `None` where its compile cannot be carried.
    ///
    /// The slot is the first of these.
    /// - The slot recorded for the session of the worktree, where it is free.
    /// - The free slot with the newest session.
    /// - One of those that frees up while the compile waits for as long as a carried compile saves, where no other
    ///   compile waits in the pool.
    /// - The lowest free slot.
    /// - A new slot.
    pub fn take(unit: &Unit, args: &[String], store: &Path, files: Files) -> Option<Slot> {
        let worktree = unit.roots.worktree.clone()?;
        let incremental = args.iter().find_map(|arg| {
            arg.strip_prefix("incremental=").or_else(|| arg.strip_prefix("-Cincremental="))
        })?;
        let inside =
            |path: &Path| path.starts_with(&worktree) && !path.to_string_lossy().contains(' ');
        let relative = unit.directory.strip_prefix(&worktree).ok()?.to_path_buf();
        if unit.roots.separate_target().is_some()
            || local_thin_lto(args)
            || !inside(Path::new(incremental))
            || !inside(&unit.out_dir)
        {
            return None;
        }
        let name = crate::crate_name(args);
        let metadata = args.iter().filter(|arg| arg.starts_with("metadata="));
        let identity = blake3::hash(
            format!("{name}\n{}", metadata.map(String::as_str).collect::<String>()).as_bytes(),
        );
        let identity = &identity.to_hex()[..16];
        let store = fs::canonicalize(store).ok()?;
        let baked = baked(&store, args);
        if fs::File::open(&baked).and_then(|file| file.set_modified(SystemTime::now())).is_ok() {
            return None;
        }
        let pool = store.join("slots").join(identity);
        fs::create_dir_all(&pool).ok()?;
        let subdirectory = format!("{name}-trucker.{identity}");
        let sessions = Path::new(incremental).join(&subdirectory);
        let recorded = newest(&sessions)
            .and_then(|_| fs::read_to_string(sessions.join(SLOT)).ok()?.parse::<usize>().ok());
        let (mut kept, mut cold, mut warm) = (Vec::new(), Duration::ZERO, Duration::ZERO);
        for entry in fs::read_dir(&pool).ok()?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(number) = name.strip_suffix(".source").and_then(|n| n.parse::<usize>().ok())
                && let Some(modified) = entry.metadata().ok().and_then(|file| file.modified().ok())
            {
                kept.push((modified, number));
            } else if let Some(time) = [(".cold", &mut cold), (".warm", &mut warm)]
                .into_iter()
                .find_map(|(kind, time)| name.ends_with(kind).then_some(time))
                && let Ok(milliseconds) =
                    fs::read_to_string(entry.path()).map(|text| text.parse::<u64>())
            {
                *time = (*time).max(Duration::from_millis(milliseconds.unwrap_or(0)));
            }
        }
        kept.sort_by(|a, b| b.cmp(a));
        let mut preferred = Vec::from_iter(recorded);
        preferred.extend(kept.iter().map(|(_, number)| *number).filter(|n| Some(*n) != recorded));
        // A compile waits for a slot with a session no longer than a carried compile saves on a cold one, and not at all
        // where the pool has no record of both. Only one compile of a pool waits, because the ones behind it would
        // queue for longer than a cold compile, and each that starts cold in its own slot carries its later edits.
        let saved = if warm.is_zero() { Duration::ZERO } else { cold.saturating_sub(warm) };
        let taken = wait(&pool, &preferred, saved.min(PATIENCE));
        let (number, lock) = taken.or_else(|| choose(&pool, preferred))?;
        let path = pool.join(format!("{number}.slot"));
        let path = path
            .to_str()
            .filter(|path| !path.contains(|c: char| c.is_control() || "\\\" ".contains(c)))?;
        let mut slot = Slot {
            path: path.to_string(),
            directory: Path::new(path).join(&relative),
            relative,
            worktree,
            pool,
            baked,
            number,
            sessions,
            subdirectory,
            restored: false,
            cold: false,
            links: links(args),
            files,
            _lock: lock,
        };
        slot.mirror().ok()?;
        let mut ahead = Vec::new();
        if slot.links {
            ahead = slot.share_libraries(args).ok()?;
        }
        let input = args.iter().find(|arg| arg.ends_with(".rs") && !arg.starts_with('-'))?;
        let mirrored = |path: &str| slot.directory.join(slot.rewrite(path)).exists();
        if !mirrored(input) || !mirrored(&unit.out_dir.to_string_lossy()) {
            return None;
        }
        if recorded != Some(number) {
            slot.restored = slot.restore().is_ok();
            if !slot.restored {
                remove_sessions(&slot.sessions).ok()?;
                slot.cold = true;
            }
            let written = fs::create_dir_all(&slot.sessions)
                .and_then(|()| fs::write(slot.sessions.join(SLOT), number.to_string()));
            written.ok()?;
        }
        // A carried session is a clone, and rustc reads it from the start of the compile, so it is read first.
        if slot.restored {
            let session = crate::store::tree(&slot.sessions).unwrap_or_default().into_iter();
            let files = session.filter(|(_, kind)| kind.is_file());
            ahead.splice(0..0, files.map(|(path, _)| slot.sessions.join(path)));
        }
        read_ahead(ahead);
        Some(slot)
    }

    /// Makes the slot mirror the worktree from its root down to the working directory of cargo.
    ///
    /// The slot holds a real directory for each directory on that path, a symlink to every other entry beside them, and
    /// [`LIBRARIES`] at its root. rustc runs in a real directory, so it does not resolve the path, and the path does
    /// not differ between worktrees. An entry that already is the directory or symlink that it must be stays, so a slot
    /// that mirrors the worktree is only read. An entry that cannot be linked is left out, and the caller checks the
    /// paths of the crate for that.
    fn mirror(&self) -> io::Result<()> {
        let (mut real, mut copy) = (PathBuf::from(&self.worktree), PathBuf::from(&self.path));
        let mut down = self.relative.iter();
        let mut kept = Some(OsString::from(LIBRARIES));
        loop {
            fs::create_dir_all(&copy)?;
            let next = down.next();
            let mut wanted = std::collections::HashMap::new();
            for entry in fs::read_dir(&real)? {
                let entry = entry?;
                if Some(entry.file_name().as_os_str()) != next {
                    wanted.insert(entry.file_name(), entry.path());
                }
            }
            for entry in fs::read_dir(&copy)? {
                let name = entry?.file_name();
                let path = copy.join(&name);
                let directory = fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_dir());
                if directory && (Some(name.as_os_str()) == next || Some(&name) == kept.as_ref()) {
                    continue;
                }
                if wanted
                    .get(&name)
                    .is_some_and(|target| fs::read_link(&path).is_ok_and(|link| link == *target))
                {
                    wanted.remove(&name);
                    continue;
                }
                if directory { fs::remove_dir_all(&path) } else { fs::remove_file(&path) }?;
            }
            for (name, target) in wanted {
                let _ = std::os::unix::fs::symlink(target, copy.join(name));
            }
            let Some(next) = next else { return Ok(()) };
            (real, copy, kept) = (real.join(next), copy.join(next), None);
        }
    }

    /// Makes [`LIBRARIES`] share only the libraries in the worktree directories that `args` search or name with
    /// `--extern`, and returns those that it shared anew.
    ///
    /// AGENTS.md says why a linking session needs them and why a hard link is safe. [`Slot::links_libraries`] says
    /// where they are hard-linked rather than cloned. The libraries stay between compiles, so a replaced library stays
    /// on disk until the next compile that links. A hard link stays while it is the inode of the library, which a write
    /// in place reaches. `<number>.libraries` in the pool lists each clone with the [`identity`] of its library before
    /// the clone and its own after, and a clone stays only where both match again.
    /// A library is listed only where its ctime was over a second old before it was read, because a write in the same
    /// tick of a coarse clock would leave its identity alone. The file is removed while the libraries change, and the
    /// libraries are removed where it is missing.
    /// A clone also stays where its remembered hash matches that of its library, as after a new worktree restores the
    /// libraries, because a new clone is read from disk again.
    fn share_libraries(&self, args: &[String]) -> io::Result<Vec<PathBuf>> {
        let (prefix, started) = (format!("{}/", self.worktree), crate::now() as i64);
        let directories =
            args.iter().enumerate().filter_map(|(at, arg)| match arg.split_once('=') {
                Some(("dependency", directory)) => Some(directory),
                Some((_, path)) if at > 0 && args[at - 1] == "--extern" => {
                    Path::new(path).parent()?.to_str()
                }
                _ => None,
            });
        let mut directories = Vec::from_iter(directories.filter_map(|d| d.strip_prefix(&prefix)));
        directories.sort();
        directories.dedup();
        let libraries = Path::new(&self.path).join(LIBRARIES);
        let manifest = self.pool.join(format!("{}.libraries", self.number));
        let text = fs::read_to_string(&manifest);
        absent(fs::remove_file(&manifest))?;
        let line = |line: &str| match Vec::from_iter(line.split('\t'))[..] {
            [name, library, copy] => Some((PathBuf::from(name), (library.into(), copy.into()))),
            _ => None,
        };
        let kept = text.ok().and_then(|text| text.lines().map(line).collect::<Option<_>>());
        let mut kept: HashMap<PathBuf, (String, String)> = match kept {
            Some(kept) => kept,
            None => absent(fs::remove_dir_all(&libraries)).map(|()| HashMap::new())?,
        };
        let (mut written, mut shared) = (String::new(), Vec::new());
        let record = |name: &Path, file: &fs::Metadata, copy: &Path| -> io::Result<String> {
            Ok(match file.ctime().saturating_add(1) < started {
                true => format!("{}\t{}\t{}\n", name.display(), identity_of(file), identity(copy)?),
                false => String::new(),
            })
        };
        for directory in directories {
            fs::create_dir_all(libraries.join(directory))?;
            let (mut named, mut stale) = (HashSet::new(), Vec::new());
            for entry in fs::read_dir(Path::new(&self.worktree).join(directory))? {
                let path = entry?.path();
                let name = Path::new(directory).join(path.file_name().unwrap_or_default());
                let copy = libraries.join(&name);
                // A file that another compile removes meanwhile is not a library of this one.
                let file = match fs::metadata(&path) {
                    Ok(file) if is_library(&path) => file,
                    Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                    _ => continue,
                };
                let source = identity_of(&file);
                let linked = fs::metadata(&copy).is_ok_and(|copy| {
                    (copy.dev(), copy.ino()) == (file.dev(), file.ino()) && self.links_libraries()
                });
                let recorded = kept.remove(&name);
                if !linked
                    && recorded.is_none_or(|(was, made)| {
                        was != source || identity(&copy).ok() != Some(made)
                    })
                {
                    stale.push((name, path, copy, file));
                    continue;
                }
                written += &record(&name, &file, &copy)?;
                named.insert(copy);
            }
            let same = self.same_contents(&stale);
            for ((name, path, copy, file), same) in stale.into_iter().zip(same) {
                if !same {
                    absent(fs::remove_file(&copy))?;
                    match self.share_library(&file, &path, &copy) {
                        Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                        result => result?,
                    }
                    shared.push(copy.clone());
                }
                written += &record(&name, &file, &copy)?;
                named.insert(copy);
            }
            for entry in fs::read_dir(libraries.join(directory))? {
                let path = entry?.path();
                if !named.contains(&path) {
                    let directory = fs::symlink_metadata(&path).is_ok_and(|file| file.is_dir());
                    if directory { fs::remove_dir_all(&path) } else { fs::remove_file(&path) }?;
                }
            }
        }
        for name in kept.keys() {
            absent(fs::remove_file(libraries.join(name)))?;
        }
        let staging = self.pool.join(format!("{}.libraries.{}", self.number, std::process::id()));
        fs::write(&staging, written).and_then(|()| fs::rename(&staging, &manifest))?;
        Ok(shared)
    }

    /// Whether the slot hard-links its libraries rather than cloning them.
    ///
    /// Linux and a filesystem that cannot clone link them. A clone on Linux does not share the page cache of its
    /// library, so the link of a large binary would read every new library from disk again. macOS clones them.
    fn links_libraries(&self) -> bool {
        cfg!(target_os = "linux") || self.files == Files::Links
    }

    /// Shares `library`, whose metadata before was `file`, at `copy` as [`Slot::links_libraries`] decides.
    fn share_library(&self, file: &fs::Metadata, library: &Path, copy: &Path) -> io::Result<()> {
        if !self.links_libraries() {
            self.files.share(library, copy)?;
            self.remember_clone(file, library, copy);
            return Ok(());
        }
        #[cfg(target_os = "linux")]
        if let Some(memos) = self.memos() {
            return link_remembered(library, copy, &memos);
        }
        fs::hard_link(library, copy)
    }

    /// Remembers for `copy`, a new clone of `library`, the hash that the store remembers for `file`, the metadata of
    /// the library before the clone.
    ///
    /// The library must still have that identity after the clone, so the clone has the contents that were hashed. A
    /// later worktree then keeps the clone without reading it, which [`Slot::same_contents`] would do from disk. The
    /// clone is remembered although its times are new, because the clone is whole once it returns, and nothing writes
    /// it in place afterwards, as rustc and the linker only read it and trucker replaces it by a new clone.
    fn remember_clone(&self, file: &fs::Metadata, library: &Path, copy: &Path) {
        let Some(memos) = self.memos().filter(|_| self.files == Files::Clones) else { return };
        let before = identity_of(file);
        let Some(hash) = crate::unit::recalled(&memos, &before) else { return };
        if identity(library).is_ok_and(|now| now == before)
            && let Ok(made) = identity(copy)
        {
            let _ = crate::unit::remember(&memos, &made, &hash);
        }
    }

    /// The directory of the store that holds remembered hashes.
    fn memos(&self) -> Option<PathBuf> {
        self.pool.parent().and_then(Path::parent).map(|store| store.join("hashes"))
    }

    /// Whether each clone of `stale` has the contents of its library, by hashes remembered in the store.
    ///
    /// It is `false` throughout where the slot links its libraries, and for a clone that cannot be hashed.
    fn same_contents(&self, stale: &[(PathBuf, PathBuf, PathBuf, fs::Metadata)]) -> Vec<bool> {
        let mut same = vec![false; stale.len()];
        let Some(memos) = self.memos().filter(|_| !self.links_libraries()) else { return same };
        let is_file = |(_, (_, _, copy, _)): &(usize, &(_, _, PathBuf, _))| {
            fs::symlink_metadata(copy).is_ok_and(|copy| copy.is_file())
        };
        let candidates = Vec::from_iter(stale.iter().enumerate().filter(is_file).map(|(at, _)| at));
        let paths = candidates.iter().map(|&at| stale[at].1.clone());
        let paths = Vec::from_iter(paths.chain(candidates.iter().map(|&at| stale[at].2.clone())));
        if let Ok(hashes) = crate::unit::file_hashes(&paths, &memos) {
            for (n, &at) in candidates.iter().enumerate() {
                same[at] = hashes[n] == hashes[n + candidates.len()];
            }
        }
        same
    }

    /// Replaces the sessions of the crate in the worktree with the recorded session, or else the snapshot of the pool.
    ///
    /// Both go through [`Files::duplicate_tree`]. A shared recorded session also becomes the snapshot, which is taken
    /// from the shared copy before rustc runs and links its objects into the output directory. The snapshot belongs to
    /// trucker, which only ever duplicates it, so it is used without a check. The lock file that rustc requires of a
    /// session is written last and never shared, so rustc deletes a partial session.
    fn restore(&self) -> io::Result<()> {
        let copy = match self.restore_recorded() {
            Ok(copy) => {
                let _ = self.snapshot(&copy);
                copy
            }
            Err(_) => {
                let (crate_directory, kept) =
                    newest(&self.snapshot_path()).ok_or(io::ErrorKind::NotFound)?;
                remove_sessions(&self.sessions)?;
                let copy =
                    self.sessions.join(crate_directory).join(kept.file_name().unwrap_or_default());
                self.files.duplicate_tree(&kept, &copy)?;
                copy
            }
        };
        let name = copy.file_name().unwrap_or_default().to_string_lossy();
        let lock = format!("{}.lock", name.rsplit_once('-').map_or(&*name, |(stem, _)| stem));
        fs::File::create(copy.with_file_name(lock)).map(drop)
    }

    /// Shares the session that the slot recorded into the sessions of the crate, and returns the path of the copy.
    ///
    /// The session must still be the newest of a worktree whose sessions name the slot. rustc deletes a finalized
    /// session only under an exclusive lock of its lock file, so the session is shared under a shared one. Under a
    /// shared lock, rustc can still empty the session, and an object of the session shares its inode with an output
    /// that rustc writes in place. So the session is used only where each file keeps the length and mtime that
    /// [`Slot::keep`] recorded once it is cloned.
    fn restore_recorded(&self) -> io::Result<PathBuf> {
        let record = fs::read_to_string(self.pool.join(format!("{}.source", self.number)))?;
        let (recorded, listed) = record.split_once('\n').ok_or(io::ErrorKind::InvalidData)?;
        let source = Path::new(recorded).parent().and_then(Path::parent);
        let source = source.ok_or(io::ErrorKind::InvalidData)?;
        if fs::read_to_string(source.join(SLOT))? != self.number.to_string() {
            return Err(io::ErrorKind::NotFound.into());
        }
        let (crate_directory, session) = newest(source).ok_or(io::ErrorKind::NotFound)?;
        if session != Path::new(recorded) {
            return Err(io::ErrorKind::NotFound.into());
        }
        let name = session.file_name().unwrap_or_default().to_string_lossy();
        let lock = format!("{}.lock", name.rsplit_once('-').map_or(&*name, |(stem, _)| stem));
        let held = source.join(&crate_directory).join(&lock);
        // A lock file that rustc removed after it was opened belongs to a deleted session.
        let shared = fs::File::open(&held)?;
        shared.try_lock_shared().map_err(|_| io::ErrorKind::WouldBlock)?;
        if fs::metadata(&held)?.ino() != shared.metadata()?.ino() {
            return Err(io::ErrorKind::NotFound.into());
        }
        remove_sessions(&self.sessions)?;
        let copy = self.sessions.join(crate_directory).join(&*name);
        self.files.duplicate_tree(&session, &copy)?;
        if listing(&session)?.0 != listed {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(copy)
    }

    /// The directory of the pool that holds the snapshot of an earlier session of the slot.
    fn snapshot_path(&self) -> PathBuf {
        self.pool.join(format!("{}.session", self.number))
    }

    /// Replaces the snapshot of the pool with a duplicate of `session` from the worktree, and adds its size to
    /// `gc.added`.
    ///
    /// The snapshot is staged and renamed into place, so a killed process does not leave a partial one. Only the
    /// process that holds the lock of the slot writes or reads it.
    fn snapshot(&self, session: &Path) -> io::Result<()> {
        let (kept, crate_directory) =
            (self.snapshot_path(), session.parent().and_then(Path::file_name));
        let staging = kept.with_extension(format!("session.{}", std::process::id()));
        let destination = staging.join(crate_directory.unwrap_or_default());
        let snapshot = self
            .files
            .duplicate_tree(session, &destination.join(session.file_name().unwrap_or_default()));
        let replaced = snapshot
            .and_then(|()| absent(fs::remove_dir_all(&kept)))
            .and_then(|()| fs::rename(&staging, &kept));
        if replaced.is_err() {
            let _ = fs::remove_dir_all(&staging);
        } else if let Some(store) = self.pool.parent().and_then(Path::parent) {
            crate::gc::add(store, &kept);
        }
        replaced
    }

    /// `text` with each path of the worktree replaced by the same path in the slot.
    pub fn rewrite(&self, text: &str) -> String {
        if text == self.worktree {
            return self.path.clone();
        }
        linked(text, &self.worktree, &self.path)
    }

    /// `text` with each path in the slot replaced by the same path in the worktree, as cargo and the store expect.
    pub fn localized(&self, text: &str) -> String {
        let libraries = format!("{}/{LIBRARIES}", self.path);
        text.replace(&libraries, &self.worktree).replace(&self.path, &self.worktree)
    }

    /// `args` through [`Slot::rewrite`], with the crate's sessions in a directory of their own.
    pub fn args(&self, args: &[String]) -> Vec<String> {
        let incremental =
            |arg: &String| arg.starts_with("incremental=") || arg.starts_with("-Cincremental=");
        let (slot, libraries) =
            (format!("={}/", self.path), format!("={}/{LIBRARIES}/", self.path));
        let arg = |(at, arg): (usize, &String)| {
            let rewritten = self.rewrite(arg);
            let library = at > 0 && args[at - 1] == "--extern" || arg.starts_with("dependency=");
            if incremental(arg) {
                format!("{rewritten}/{}", self.subdirectory)
            } else if self.links && library {
                rewritten.replacen(&slot, &libraries, 1)
            } else {
                rewritten
            }
        };
        let mut rewritten = Vec::from_iter(args.iter().enumerate().map(arg));
        // ld applies only one `-oso_prefix`, and rustc names each library by its real path. So the link runs through
        // [`link`] to name the libraries at their paths in the slot.
        if rewritten.iter().any(|arg| arg.contains("-oso_prefix,"))
            && let Ok(wrapper) = env::current_exe()
        {
            rewritten.extend(["-C".to_string(), format!("linker={}", wrapper.display())]);
        }
        rewritten
    }

    /// The variables of this process whose values name a path of the worktree, rewritten by [`Slot::rewrite`], and
    /// the one that [`link`] reads.
    pub fn environment(&self, args: &[String]) -> Vec<(OsString, String)> {
        let variables =
            env::vars_os().filter_map(|(name, value)| Some((name, value.into_string().ok()?)));
        let rewritten = variables.map(|(name, value)| (name, self.rewrite(&value)));
        let mut environment =
            Vec::from_iter(rewritten.filter(|(_, value)| value.contains(&self.path)));
        let linker = codegen(args, "linker").unwrap_or("cc");
        environment.push((LINKER.into(), format!("{linker}\n{}\n{}", self.worktree, self.path)));
        environment
    }

    /// Records `compile` as the time of a cold compile of the crate where this one started without a session, and
    /// else of a carried one.
    ///
    /// [`Slot::take`] waits for a busy slot with a session for as long as the one saves on the other.
    pub fn time(&self, compile: Duration) {
        let kind = if self.cold { "cold" } else { "warm" };
        let _ = fs::write(
            self.pool.join(format!("{}.{kind}", self.number)),
            compile.as_millis().to_string(),
        );
    }

    /// Records the newest session of the crate in the worktree, with its [`listing`], as the session that the slot
    /// compiled last.
    ///
    /// [`Slot::restore`] shares it into a worktree that has no session for the slot. Where the pool keeps no snapshot,
    /// it takes one of this session, and keeps it only where every file still has its recorded length and mtime
    /// afterwards.
    ///
    /// The session is recorded only where each mtime is older than the record, so a later write in the same tick of a
    /// coarse clock cannot leave an mtime alone.
    pub fn keep(&self) -> io::Result<()> {
        let (_, session) = newest(&self.sessions).ok_or(io::ErrorKind::NotFound)?;
        let staging = self.pool.join(format!("{}.source.{}", self.number, std::process::id()));
        fs::write(&staging, "")?;
        let (listed, latest) = listing(&session)?;
        if latest >= fs::metadata(&staging)?.modified()? {
            return absent(fs::remove_file(&staging)).and(Err(io::ErrorKind::InvalidData.into()));
        }
        let record = [session.as_os_str().as_encoded_bytes(), b"\n", listed.as_bytes()].concat();
        fs::write(&staging, record)?;
        fs::rename(&staging, self.pool.join(format!("{}.source", self.number)))?;
        if !self.snapshot_path().exists()
            && self.snapshot(&session).is_ok()
            && listing(&session).ok().map(|(now, _)| now) != Some(listed)
        {
            absent(fs::remove_dir_all(self.snapshot_path()))?;
        }
        Ok(())
    }

    /// Whether the dep-info of `unit` records a variable whose value names the slot, which predicts that an output
    /// names it too.
    ///
    /// `env!("CARGO_MANIFEST_DIR")` reads such a variable.
    pub fn reads_itself(&self, unit: &Unit) -> bool {
        let dep_info = unit.outputs().unwrap_or_default().into_iter();
        let mut dep_info =
            dep_info.filter(|output| output.extension().is_some_and(|kind| kind == "d"));
        dep_info.any(|path| {
            fs::read_to_string(path).is_ok_and(|text| {
                text.lines().any(|line| line.starts_with("# env-dep:") && line.contains(&self.path))
            })
        })
    }

    /// Marks the crate as one whose output names the slot, as an output does that bakes `env!("CARGO_MANIFEST_DIR")`.
    ///
    /// Such an output would read another worktree once the slot mirrors that one, so the crate is never carried again
    /// and its sessions are deleted.
    pub fn bake(&mut self) {
        let out = self.baked.parent().into_iter().try_for_each(fs::create_dir_all);
        let _ = out.and_then(|()| fs::File::create(&self.baked));
        let _ = self.discard().and_then(|()| remove_sessions(&self.sessions));
    }

    /// Deletes the session that the compile started from, and the record and snapshot of the slot.
    ///
    /// The session goes through [`remove_sessions`]. Where the sessions cannot be removed, the file that names the slot
    /// is removed instead, so a later compile does not take them as the sessions of the slot.
    pub fn discard(&mut self) -> io::Result<()> {
        self.restored = false;
        absent(fs::remove_file(self.pool.join(format!("{}.source", self.number))))?;
        absent(fs::remove_dir_all(self.snapshot_path()))?;
        remove_sessions(&self.sessions)
            .inspect_err(|_| {
                let _ = fs::remove_file(self.sessions.join(SLOT));
            })
            .and_then(|()| fs::create_dir_all(&self.sessions))
            .and_then(|()| fs::write(self.sessions.join(SLOT), self.number.to_string()))
    }
}

/// Hard-links `library` to `copy`, and remembers for the link the hash that `memos` hold for the library.
///
/// The link sets the ctime of the library, which drops its memo. The kernel grants a read lease only while no
/// descriptor has the file open for writing, and breaks it at an open for writing or a truncate. So a lease held from
/// before the link to after it shows that the contents still have the hash, whatever the times say. A break of the
/// lease raises SIGURG, which a process ignores by default, rather than SIGIO, which would end it.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn link_remembered(library: &Path, copy: &Path, memos: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // The value is that of asm-generic, which x86_64 and aarch64 share, and the libc crate leaves it out for glibc.
    const F_SETSIG: libc::c_int = 10;
    let file = fs::File::open(library)?;
    let descriptor = file.as_raw_fd();
    // SAFETY: the descriptor is an open file, and these commands take an integer or nothing as their argument.
    let leased = unsafe {
        libc::fcntl(descriptor, F_SETSIG, libc::SIGURG) == 0
            && libc::fcntl(descriptor, libc::F_SETLEASE, libc::F_RDLCK) == 0
    };
    let before = file.metadata()?;
    fs::hard_link(library, copy)?;
    let (after, linked) = (file.metadata(), fs::metadata(copy));
    // SAFETY: as above.
    let held = leased && unsafe { libc::fcntl(descriptor, libc::F_GETLEASE) } == libc::F_RDLCK;
    drop(file);
    let stamp = |file: &fs::Metadata| (file.len(), file.mtime(), file.mtime_nsec());
    if held
        && let (Ok(after), Ok(linked)) = (after, linked)
        && (linked.dev(), linked.ino()) == (after.dev(), after.ino())
        && stamp(&after) == stamp(&before)
        && let Some(hash) = crate::unit::recalled(memos, &identity_of(&before))
    {
        let _ = crate::unit::remember(memos, &identity_of(&after), &hash);
    }
    Ok(())
}

/// Reads `files` on threads of their own while rustc runs, so that rustc and the link find them in the page cache.
///
/// AGENTS.md says why a restored library or session is not in the page cache. The threads only read, and end with the
/// process.
fn read_ahead(files: Vec<PathBuf>) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    if files.is_empty() {
        return;
    }
    let (files, next) = (Arc::new(files), Arc::new(AtomicUsize::new(0)));
    for _ in 0..READ_AHEAD_THREADS.min(files.len()) {
        let (files, next) = (Arc::clone(&files), Arc::clone(&next));
        let _ = std::thread::Builder::new().spawn(move || {
            let mut buffer = vec![0; 1 << 20];
            while let Some(file) = files.get(next.fetch_add(1, Ordering::Relaxed)) {
                read_through(file, &mut buffer);
            }
        });
    }
}

/// The number of threads that [`read_ahead`] reads on, which leaves the other cores to rustc.
const READ_AHEAD_THREADS: usize = 4;

fn read_through(path: &Path, buffer: &mut [u8]) {
    use std::io::Read;
    if let Ok(mut file) = fs::File::open(path) {
        while file.read(buffer).is_ok_and(|read| read > 0) {}
    }
}

/// The variable that names the files to read, one per line, when this program runs as the reader that [`read_later`]
/// starts.
const READER: &str = "TRUCKER_READ";

/// Starts this program in the background to read the libraries that a hit restored for `unit`, where the store in
/// `store` clones them.
///
/// A clone has a page cache of its own, so the first link in a new worktree would read every library from disk again,
/// as AGENTS.md says. The reader only reads, and neither the hit nor cargo waits for it.
pub fn read_later(unit: &Unit, store: &Path) {
    use std::process::Stdio;
    let outputs = unit.outputs().unwrap_or_default().into_iter().filter(|path| is_library(path));
    let paths = Vec::from_iter(outputs.map(|path| path.into_os_string().into_vec()));
    if paths.is_empty() || crate::store::Files::probe(store) != Files::Clones {
        return;
    }
    let Ok(wrapper) = env::current_exe() else { return };
    let list = OsString::from_vec(paths.join(&b'\n'));
    let mut command = Command::new(wrapper);
    command.env(READER, list).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let _ = command.spawn();
}

/// Reads the files that [`READER`] names and exits, where [`read_later`] started this process.
pub fn read() {
    use std::os::unix::ffi::OsStrExt;
    let Some(list) = env::var_os(READER) else { return };
    let mut buffer = vec![0; 1 << 20];
    for path in list.as_bytes().split(|byte| *byte == b'\n') {
        read_through(Path::new(std::ffi::OsStr::from_bytes(path)), &mut buffer);
    }
    std::process::exit(0)
}

/// Whether `path` names a library that rustc or the linker reads.
fn is_library(path: &Path) -> bool {
    path.extension().is_some_and(|kind| ["rlib", "rmeta", "so", "dylib"].iter().any(|k| kind == *k))
}

/// The marker in `out/` of `store` that keeps the crate compiled with `args` from being carried in any of its pools.
///
/// A change of a dependency or a flag gives a crate another pool, so the marker names the package, the crate and
/// whether it links. A crate that shares them with a baked one only loses its carried session, and gc removes the
/// marker after three days without a compile that touches it.
fn baked(store: &Path, args: &[String]) -> PathBuf {
    let package = env::var("CARGO_PKG_NAME").unwrap_or_else(|_| "-".to_string());
    let kind = if links(args) { "linked" } else { "library" };
    store.join("out").join(format!("{package}.{}.{kind}.baked", crate::crate_name(args)))
}

/// Whether a compile with `args` links, as a test or a crate of another type than a library does.
fn links(args: &[String]) -> bool {
    let library = |kind: &String| matches!(kind.as_str(), "lib" | "rlib");
    args.iter().any(|arg| arg == "--test")
        || args.windows(2).any(|pair| pair[0] == "--crate-type" && !library(&pair[1]))
}

/// Runs as the linker of a carried compile and exits, where rustc started this process as one.
///
/// Each path of the worktree in the arguments, and in a response file that they name, is replaced by the same path in
/// the slot. A first argument that names rustc marks a wrapped compile, such as one that a proc macro starts.
pub fn link() {
    let (Ok(setting), Some(first)) = (env::var(LINKER), env::args().nth(1)) else { return };
    let mut parts = setting.splitn(3, '\n');
    let (Some(linker), Some(worktree), Some(slot)) = (parts.next(), parts.next(), parts.next())
    else {
        return;
    };
    if Path::new(&first).file_name().is_some_and(|name| name.to_string_lossy().starts_with("rustc"))
    {
        return;
    }
    let args = Vec::from_iter(env::args().skip(1).map(|arg| {
        if let Some(file) = arg.strip_prefix('@') {
            let _ = fs::read_to_string(file)
                .and_then(|text| fs::write(file, linked(&text, worktree, slot)));
        }
        linked(&arg, worktree, slot)
    }));
    let error = Command::new(linker).args(args).env_remove(LINKER).exec();
    std::process::exit(crate::report(Path::new(linker), error))
}

/// `text` with each path under `worktree`, or of a library shared in the [`LIBRARIES`] of `slot`, replaced by the same
/// path under `slot`.
pub fn linked(text: &str, worktree: &str, slot: &str) -> String {
    let slot = format!("{slot}/");
    text.replace(&format!("{slot}{LIBRARIES}/"), &slot).replace(&format!("{worktree}/"), &slot)
}

/// The first of `preferred`, then of every slot number, whose [`lock`] in `pool` this process takes without waiting.
///
/// It is `None` where a lock file cannot be opened or locked.
pub fn choose(pool: &Path, preferred: Vec<usize>) -> Option<(usize, fs::File)> {
    for number in preferred.into_iter().chain(0..) {
        if let Some(file) = lock(pool, number)? {
            return Some((number, file));
        }
    }
    None
}

/// The first of `warm` whose lock in `pool` this process takes within `patience`, which it polls for while it holds
/// the lock `wait.lock` of the pool.
///
/// It is `None` after `patience`, at once where another process holds `wait.lock`, or where a lock file cannot be
/// opened or locked.
fn wait(pool: &Path, warm: &[usize], patience: Duration) -> Option<(usize, fs::File)> {
    let started = Instant::now();
    let mut waiting = None;
    loop {
        for &number in warm {
            if let Some(file) = lock(pool, number)? {
                return Some((number, file));
            }
        }
        if started.elapsed() >= patience {
            return None;
        }
        if waiting.is_none() {
            let file = fs::File::create(pool.join("wait.lock")).ok()?;
            file.try_lock().ok()?;
            waiting = Some(file);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The lock of the slot `number` in `pool`, or `Some(None)` where another process holds it.
///
/// A lock file that gc removed after it was opened counts as held, because another process may lock its successor. It
/// is `None` where the lock file cannot be opened or locked.
fn lock(pool: &Path, number: usize) -> Option<Option<fs::File>> {
    let path = pool.join(format!("{number}.lock"));
    let file = fs::File::create(&path).ok()?;
    match file.try_lock() {
        Err(fs::TryLockError::Error(_)) => None,
        Err(fs::TryLockError::WouldBlock) => Some(None),
        Ok(()) => {
            let current = fs::metadata(&path).ok().map(|metadata| metadata.ino());
            if current != file.metadata().ok().map(|metadata| metadata.ino()) {
                return Some(None);
            }
            let _ = file.set_modified(SystemTime::now());
            Some(Some(file))
        }
    }
}

/// The longest that a compile waits for a slot with a session, whatever the pool recorded.
const PATIENCE: Duration = Duration::from_secs(600);

/// The directory of the crate and its newest finalized session in `sessions`, which is the `-C incremental` of a crate.
///
/// rustc names a session `s-<time>-<random>-<hash>`, and one that it is writing `s-<time>-<random>-working`.
fn newest(sessions: &Path) -> Option<(OsString, PathBuf)> {
    let mut found = Vec::new();
    for crate_directory in fs::read_dir(sessions).ok()?.flatten() {
        for session in fs::read_dir(crate_directory.path()).into_iter().flatten().flatten() {
            let name = session.file_name().to_string_lossy().into_owned();
            let parts = Vec::from_iter(name.split('-'));
            if parts.len() == 4 && parts[0] == "s" && parts[3] != "working" && !name.contains('.') {
                found.push((
                    (parts[1].len(), parts[1].to_string()),
                    crate_directory.file_name(),
                    session.path(),
                ));
            }
        }
    }
    found.into_iter().max().map(|(_, crate_directory, session)| (crate_directory, session))
}

/// Each entry of `session` with its length and mtime, which every write to it changes, and the latest of those mtimes.
///
/// The metadata of a library is listed without its mtime, because rustc replaces it by a rename and never writes it in
/// place. cargo sets back that mtime once a check ends.
fn listing(session: &Path) -> io::Result<(String, SystemTime)> {
    let (mut lines, mut latest) = (Vec::new(), SystemTime::UNIX_EPOCH);
    for (path, _) in crate::store::tree(session)? {
        let file = fs::symlink_metadata(session.join(&path))?;
        let mut line = format!("{}\t{}", path.display(), file.len());
        if path.extension().is_none_or(|kind| kind != "rmeta") {
            latest = latest.max(file.modified()?);
            line += &format!("\t{}.{}", file.mtime(), file.mtime_nsec());
        }
        lines.push(line);
    }
    lines.sort();
    Ok((lines.join("\n"), latest))
}

/// Removes the sessions of a crate in a worktree under an exclusive lock of the lock file of each, as rustc deletes a
/// session.
///
/// Another slot may have recorded one of them. Fails where [`Slot::restore`] is cloning one.
pub fn remove_sessions(sessions: &Path) -> io::Result<()> {
    let mut locks = Vec::new();
    for crate_directory in fs::read_dir(sessions).into_iter().flatten().flatten() {
        for entry in fs::read_dir(crate_directory.path()).into_iter().flatten().flatten() {
            if entry.path().extension().is_some_and(|kind| kind == "lock") {
                let lock = fs::File::open(entry.path())?;
                lock.try_lock().map_err(|_| io::ErrorKind::WouldBlock)?;
                locks.push(lock);
            }
        }
    }
    absent(fs::remove_dir_all(sessions))
}

/// `result` with a missing file taken as success.
fn absent(result: io::Result<()>) -> io::Result<()> {
    result
        .or_else(|error| if error.kind() == io::ErrorKind::NotFound { Ok(()) } else { Err(error) })
}
