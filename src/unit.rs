//! One cached rustc invocation of a third-party or own crate, and the key of what its outputs depend on.
//!
//! A key covers what cargo's own rebuild checks cover, and shares their blind spot. A proc macro or build script that
//! reads a variable, or a file outside what an own crate's package covers, without declaring it gets a stale output.
//! Paths under the target directory, CARGO_HOME and the worktree enter a key through placeholders, so that every
//! worktree computes the same key. What rustc names only in its dep-info selects among the entries of one key. Such a
//! name is a source of an own crate, a file of OUT_DIR or a variable set by a build script. The files of the package
//! of an own crate that can load a proc macro select its entry too.

use std::path::{Component, MAIN_SEPARATOR, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};
use std::{env, fs, io};

pub const TARGET: &str = "{trucker:target}/";
pub const CARGO_HOME: &str = "{trucker:cargo-home}/";
pub const WORKTREE: &str = "{trucker:worktree}";

// The placeholders of the roots in a JSON string, which doubles each `\` of a path on Windows.
#[cfg(windows)]
const TARGET_JSON: &str = "{trucker:target-json}/";
#[cfg(windows)]
const CARGO_HOME_JSON: &str = "{trucker:cargo-home-json}/";

/// A field of the key of every own crate that changes with how entries are selected, so an older entry is not found.
const OWN: &[u8] =
    b"own crate, selected by settled sources, the files of its package and the directories of its declared files";

/// The names of the entries that a walk of [`Unit::package_hash`] skips, which version control, editors and tools own.
const UNREAD: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".jj",
    ".bzr",
    ".pijul",
    "_darcs",
    ".sl",
    ".github",
    ".idea",
    ".vscode",
    ".zed",
    ".claude",
    ".cursor",
    ".direnv",
    ".cache",
    "node_modules",
    ".DS_Store",
];

/// The names of the entries that make a directory another worktree, package or target directory, which no walk of
/// [`Unit::package_hash`] enters.
const ROOT_MARKERS: &[&str] = &["Cargo.toml", ".git", "CACHEDIR.TAG"];

/// How long before its compile started a source of the worktree must have last changed for the compile to be stored.
///
/// It covers a filesystem that keeps mtimes to the second, and a kernel clock that lags the system's.
const SETTLED: Duration = Duration::from_secs(1);

/// The directories whose paths differ between worktrees or machines.
pub struct Roots {
    /// The target directory, ending in a slash.
    pub target: String,
    /// CARGO_HOME, ending in a slash.
    pub cargo_home: String,
    /// The git worktree of an own crate, without a trailing slash, so that the bare root is replaced too.
    pub worktree: Option<String>,
}

impl Roots {
    /// Each root with its placeholder, the longest first, because the target directory may lie inside the worktree.
    #[cfg(unix)]
    fn placeholders(&self) -> Vec<(&str, &str)> {
        let mut pairs =
            vec![(self.target.as_str(), TARGET), (self.cargo_home.as_str(), CARGO_HOME)];
        pairs.extend(self.worktree.as_deref().map(|root| (root, WORKTREE)));
        pairs.sort_by_key(|(root, _)| std::cmp::Reverse(root.len()));
        pairs
    }

    /// Each root with its placeholder, also in the spelling of a JSON string, the longest first.
    ///
    /// rustc writes the paths of its JSON diagnostics and artifact notifications with each `\` doubled. A worktree is
    /// never set on Windows, where an own crate runs as plain rustc.
    #[cfg(windows)]
    fn placeholders(&self) -> Vec<(String, &'static str)> {
        let (target, home) = (&self.target, &self.cargo_home);
        let mut pairs = vec![
            (target.clone(), TARGET),
            (target.replace('\\', r"\\"), TARGET_JSON),
            (home.clone(), CARGO_HOME),
            (home.replace('\\', r"\\"), CARGO_HOME_JSON),
        ];
        pairs.sort_by_key(|(root, _)| std::cmp::Reverse(root.len()));
        pairs
    }

    pub fn normalized(&self, text: &str) -> String {
        let pairs = self.placeholders();
        pairs
            .iter()
            .fold(text.to_string(), |text, (root, placeholder)| text.replace(root, placeholder))
    }

    pub fn localized(&self, text: &str) -> String {
        let pairs = self.placeholders();
        pairs
            .iter()
            .fold(text.to_string(), |text, (root, placeholder)| text.replace(placeholder, root))
    }

    /// The target directory where it lies outside the worktree and so needs a remap and a scan of its own.
    pub fn separate_target(&self) -> Option<&str> {
        let root = self.worktree.as_deref()?;
        let inside =
            self.target.strip_prefix(root).is_some_and(|rest| rest.starts_with(MAIN_SEPARATOR));
        (!inside).then_some(self.target.as_str())
    }

    /// Rewrites `dep_info` with `rewrite`, through every root in the form that rustc writes in that part of a line.
    ///
    /// rustc escapes a space as `\ ` in the path of a source, but not in that of an output or in an `env-dep` value.
    pub fn dep_info(&self, dep_info: &str, rewrite: fn(&Roots, &str) -> String) -> String {
        let escape = |root: &str| root.replace(' ', "\\ ");
        let escaped = Roots {
            target: escape(&self.target),
            cargo_home: escape(&self.cargo_home),
            worktree: self.worktree.as_deref().map(escape),
        };
        let line = |line: &str| match line.split_once(": ") {
            _ if line.starts_with("# env-dep:") => rewrite(self, line),
            Some((output, sources)) => {
                format!("{}: {}", rewrite(self, output), rewrite(&escaped, sources))
            }
            None => rewrite(&escaped, line),
        };
        String::from_iter(dep_info.split_inclusive('\n').map(line))
    }
}

pub struct Unit {
    pub out_dir: PathBuf,
    pub roots: Roots,
    /// The working directory, against which a relative path of the arguments or the dep-info resolves.
    pub directory: PathBuf,
    /// The crate name and `-C extra-filename`, which start the name of every output after an optional `lib`.
    stem: String,
    externs: Vec<PathBuf>,
    /// Each `-L dependency=` directory, from which rustc loads the crates that the externs depend on.
    search: Vec<PathBuf>,
    /// The entries of each of `search`, which [`Unit::searched`] lists once.
    searched: std::sync::OnceLock<Vec<Result<Vec<PathBuf>, io::ErrorKind>>>,
    /// Each native search directory, in rustc's order, because dep-info leaves out a linked library.
    trees: Vec<String>,
    /// The value of each `-l`, such as `z` or `static=foo`.
    links: Vec<String>,
    /// OUT_DIR, of which an entry covers only the files that rustc read.
    generated: Option<String>,
    /// The flags that make a link on macOS leave out the paths of the worktree and the target directory.
    link_paths: Vec<String>,
    /// The directory of the package, from CARGO_MANIFEST_DIR, whose files a proc macro may read without declaring them.
    pub package: Option<PathBuf>,
}

/// CARGO_HOME with a trailing slash.
#[cfg(unix)]
pub fn cargo_home() -> String {
    let home = env::var("CARGO_HOME")
        .unwrap_or_else(|_| format!("{}/.cargo", env::var("HOME").unwrap_or_default()));
    format!("{}/", home.trim_end_matches('/'))
}

#[cfg(windows)]
pub use crate::windows::cargo_home;

/// Whether `input` is a source of the registry or of a git dependency, which never changes once unpacked.
pub fn is_third_party(input: &Path, cargo_home: &str) -> bool {
    let sources = input.strip_prefix(cargo_home);
    sources.is_ok_and(|path| path.starts_with("registry/src") || path.starts_with("git/checkouts"))
}

/// The nearest ancestor of `path`, or `path` itself, that contains a `.git`.
pub fn repository(path: &Path) -> Option<&Path> {
    path.ancestors().find(|directory| directory.join(".git").exists())
}

/// The git worktree of an own crate whose source is `input`, which lies outside CARGO_HOME.
pub fn worktree(input: &Path, cargo_home: &str) -> Option<String> {
    let root = repository(input.parent()?)?;
    (!input.starts_with(cargo_home)).then(|| root.display().to_string())
}

/// Whether `args` ask rustc about itself or compile stdin, as the probes of cargo do.
///
/// Such an invocation is never cached or logged.
pub fn is_query(args: &[String]) -> bool {
    let query = |arg: &String| matches!(&**arg, "-" | "-vV" | "-V" | "--version");
    args.iter().any(|arg| query(arg) || arg.starts_with("--print"))
}

/// Whether the output of `rustc -vV` names Rust 1.100 or later, whose layout gives each unit a directory of its own.
pub fn is_supported(version: &[u8]) -> bool {
    let text = String::from_utf8_lossy(version);
    let release = text.lines().find_map(|line| line.strip_prefix("release: ")).unwrap_or("");
    let mut numbers = release.split(['.', '-']).map(|number| number.parse::<u32>().ok());
    numbers.next().flatten().zip(numbers.next().flatten()) >= Some((1, 100))
}

/// `path` with each `..` removing the component before it, without asking the filesystem.
pub fn lexical(path: &Path) -> PathBuf {
    let mut lexical = PathBuf::new();
    for component in path.components() {
        if component == Component::ParentDir {
            lexical.pop();
        } else {
            lexical.push(component);
        }
    }
    lexical
}

/// Whether the key of a compile covers the environment variable `name`, which rustc reads itself or cargo sets for
/// every compile of the crate.
///
/// Any other variable enters only where dep-info names it, because the `[env]` table of the cargo configuration can set
/// it to a path of the worktree. It can even set one that cargo leaves unset, such as CARGO_RUSTC_CURRENT_DIR or
/// CARGO_TARGET_TMPDIR.
pub fn is_keyed(name: &str) -> bool {
    let compiler = "CARGO OUT_DIR MACOSX_DEPLOYMENT_TARGET SDKROOT RUSTC_BOOTSTRAP CARGO_MANIFEST_DIR \
                    CARGO_MANIFEST_PATH CARGO_CRATE_NAME CARGO_BIN_NAME CARGO_PRIMARY_PACKAGE";
    compiler.split_whitespace().any(|known| known == name) || name.starts_with("CARGO_PKG_")
}

pub fn file_hash(path: &Path) -> io::Result<blake3::Hash> {
    Ok(blake3::Hasher::new().update_mmap(path)?.finalize())
}

/// The identity of the file at `path`, as [`identity_of`] gives it.
pub fn identity(path: &Path) -> io::Result<String> {
    Ok(identity_of(&fs::metadata(path)?))
}

/// The device, inode, length, mtime and ctime of `file`.
///
/// Every write to a file sets its ctime, which only a change of the system clock can set back. So a file whose identity
/// stays the same has the same contents.
#[cfg(unix)]
pub fn identity_of(file: &fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    let (mtime, ctime) = ((file.mtime(), file.mtime_nsec()), (file.ctime(), file.ctime_nsec()));
    format!("{} {} {} {mtime:?} {ctime:?}", file.dev(), file.ino(), file.len())
}

/// The length and mtime of `file`, which never select a memo, because [`settled`] never lets one be written.
///
/// A process that may write the attributes of a file can set back its change time on Windows, so no identity there
/// proves the contents of a file.
#[cfg(windows)]
pub fn identity_of(file: &fs::Metadata) -> String {
    format!("{} {:?}", file.len(), file.modified().ok())
}

/// The hash of the file at `path`, remembered in `memos` under its [`identity`].
///
/// A file with the identity of a memo has the contents that were hashed. A file that changes while it is hashed, or
/// that is not [`settled`], is not remembered.
pub fn remembered_hash(path: &Path, memos: &Path) -> io::Result<blake3::Hash> {
    let start = crate::now() as i64;
    let file = fs::metadata(path)?;
    let before = identity_of(&file);
    if let Some(Ok(bytes)) = recalled(memos, &before).map(<[u8; 32]>::try_from) {
        return Ok(blake3::Hash::from_bytes(bytes));
    }
    let hash = file_hash(path)?;
    if identity(path)? != before {
        return file_hash(path);
    }
    if settled(&file, start) {
        let _ = remember(memos, &before, hash.as_bytes());
    }
    Ok(hash)
}

/// Whether `file` last changed over a second before `start`, in seconds since the epoch.
///
/// A write in the same tick of a coarse clock leaves the identity of a file alone, so a memo is written only for a
/// file whose times lie further back than any tick.
#[cfg(unix)]
fn settled(file: &fs::Metadata, start: i64) -> bool {
    use std::os::unix::fs::MetadataExt;
    file.ctime().max(file.mtime()).saturating_add(1) < start
}

/// Never holds, so that Windows remembers no hash, for the reason that [`identity_of`] gives.
#[cfg(windows)]
fn settled(_: &fs::Metadata, _: i64) -> bool {
    false
}

/// The hash of each of `paths` in order, remembered in `memos` as [`remembered_hash`] remembers it.
///
/// The libraries of a linked crate take a gigabyte or more and an edit changes few of them, so remembered hashes keep
/// them from delaying its compile. The files without a memo are hashed through [`crate::lazy::scan`], so a library
/// that an edit replaced, which may take hundreds of megabytes, is hashed on every core.
pub fn file_hashes(paths: &[PathBuf], memos: &Path) -> io::Result<Vec<blake3::Hash>> {
    let start = crate::now() as i64;
    let recall = |path: &PathBuf| {
        let file = fs::metadata(path)?;
        let before = identity_of(&file);
        let bytes = recalled(memos, &before).and_then(|bytes| <[u8; 32]>::try_from(bytes).ok());
        Ok((bytes.map(blake3::Hash::from_bytes), (before, settled(&file, start))))
    };
    let (mut hashes, before): (Vec<_>, Vec<_>) = on_every_core(paths, recall)?.into_iter().unzip();
    let missing = Vec::from_iter((0..paths.len()).filter(|&at| hashes[at].is_none()));
    // The files of a chunk are open at once, so a chunk stays well within the limit of open files.
    for chunk in missing.chunks(64) {
        #[cfg(unix)]
        let computed =
            crate::lazy::scan(&Vec::from_iter(chunk.iter().map(|&at| paths[at].clone())), &[])
                .map(|(computed, _)| computed.into_iter());
        // Windows does not build `lazy`, so each file is hashed on its own.
        #[cfg(windows)]
        let computed = None::<std::vec::IntoIter<blake3::Hash>>;
        let mut computed = computed.into_iter().flatten();
        for &at in chunk {
            hashes[at] = Some(match computed.next() {
                Some(hash) if identity(&paths[at])? == before[at].0 => {
                    if before[at].1 {
                        let _ = remember(memos, &before[at].0, hash.as_bytes());
                    }
                    hash
                }
                _ => remembered_hash(&paths[at], memos)?,
            });
        }
    }
    Ok(Vec::from_iter(hashes.into_iter().flatten()))
}

/// The result of `run` for each item of `paths` in order, computed on every core.
pub fn on_every_core<I: Sync, T: Send>(
    paths: &[I],
    run: impl Fn(&I) -> io::Result<T> + Sync,
) -> io::Result<Vec<T>> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let worker = || {
        let mut results = Vec::new();
        loop {
            let at = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let Some(path) = paths.get(at) else { return results };
            results.push((at, run(path)));
        }
    };
    let workers = std::thread::available_parallelism().map_or(1, usize::from).min(paths.len());
    let mut results = std::thread::scope(|scope| {
        let threads = Vec::from_iter((1..workers).map(|_| scope.spawn(worker)));
        let mut results = worker();
        for thread in threads {
            results
                .extend(thread.join().map_err(|_| io::Error::other("a hashing thread panicked"))?);
        }
        io::Result::Ok(results)
    })?;
    results.sort_by_key(|(at, _)| *at);
    results.into_iter().map(|(_, result)| result).collect()
}

/// An entry of a tree that [`hash_tree`] lists.
enum Listed {
    Directory,
    /// A symlink, with what [`link_target`] feeds for it.
    Link(Vec<u8>),
    /// A file, with its metadata, which does not follow a symlink.
    File(fs::Metadata),
}

/// Feeds the hash of the names and contents of every entry under `root` to `field`.
///
/// A UTF-8 file is hashed through the placeholders of `roots`, because a file such as a pkg-config file names the target
/// directory, and a key covers that only through its placeholder. The hash is remembered in `memos` under the roots and
/// the name and [`identity_of`] of every entry, where each file is [`settled`] and keeps its identity while it is
/// hashed. The key of every dependent lists the tree, which for libwebrtc holds tens of thousands of headers, so each
/// level of directories is listed on every core. A file is hashed on every core through [`tree_file_hash`], because a
/// native library such as that of RocksDB takes a gigabyte.
pub fn hash_tree(
    roots: &Roots,
    root: &Path,
    memos: &Path,
    field: &mut impl FnMut(&[u8]),
) -> io::Result<()> {
    let start = crate::now() as i64;
    let list = |directory: &PathBuf| {
        let mut entries = Vec::new();
        for entry in fs::read_dir(root.join(directory))? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let listed = if kind.is_dir() {
                Listed::Directory
            } else if kind.is_symlink() {
                Listed::Link(link_target(roots, &entry.path())?)
            } else {
                Listed::File(entry.metadata()?)
            };
            entries.push((directory.join(entry.file_name()), listed));
        }
        Ok(entries)
    };
    let (mut listed, mut level) = (Vec::new(), vec![PathBuf::new()]);
    while !level.is_empty() {
        let entries = Vec::from_iter(on_every_core(&level, list)?.into_iter().flatten());
        let directories = entries.iter().filter(|(_, entry)| matches!(entry, Listed::Directory));
        level = Vec::from_iter(directories.map(|(path, _)| path.clone()));
        listed.extend(entries);
    }
    // A path sorts by its components, so a directory comes right before its own entries.
    listed.sort_by(|(a, _), (b, _)| a.cmp(b));
    let feed = |hasher: &mut blake3::Hasher, bytes: &[u8]| {
        hasher.update(&(bytes.len() as u64).to_le_bytes()).update(bytes);
    };
    let mut listing = blake3::Hasher::new();
    for (path, entry) in &listed {
        feed(&mut listing, path.as_os_str().as_encoded_bytes());
        match entry {
            Listed::Directory => feed(&mut listing, b"directory"),
            Listed::Link(target) => feed(&mut listing, &[b"link ", &target[..]].concat()),
            Listed::File(file) => {
                feed(&mut listing, format!("file {}", identity_of(file)).as_bytes())
            }
        }
    }
    let worktree = roots.worktree.as_deref().unwrap_or_default();
    let (target, home, listing) = (&roots.target, &roots.cargo_home, listing.finalize().to_hex());
    let memo = format!("tree {target}\n{home}\n{worktree}\n{listing}");
    if let Some(Ok(bytes)) = recalled(memos, &memo).map(<[u8; 32]>::try_from) {
        field(&bytes);
        return Ok(());
    }
    let files = Vec::from_iter(listed.iter().filter_map(|(path, entry)| match entry {
        Listed::File(file) => Some((root.join(path), file)),
        _ => None,
    }));
    // The metadata is read again after the hash, so that a write during the hash is seen.
    let hash = |(path, file): &(PathBuf, &fs::Metadata)| {
        let hash = tree_file_hash(roots, path, memos)?;
        Ok((hash, settled(file, start) && identity(path)? == identity_of(file)))
    };
    let hashes = on_every_core(&files, hash)?;
    let (mut hashed, mut tree) = (hashes.iter(), blake3::Hasher::new());
    for (path, entry) in &listed {
        feed(&mut tree, path.as_os_str().as_encoded_bytes());
        match entry {
            Listed::Directory => feed(&mut tree, b"directory"),
            Listed::Link(target) => feed(&mut tree, &[b"link ", &target[..]].concat()),
            Listed::File(_) => {
                let (hash, _) = hashed.next().ok_or(io::ErrorKind::InvalidData)?;
                feed(&mut tree, &[b"file ", hash.as_bytes().as_slice()].concat());
            }
        }
    }
    let tree = tree.finalize();
    if hashes.iter().all(|(_, unchanged)| *unchanged) {
        let _ = remember(memos, &memo, tree.as_bytes());
    }
    field(tree.as_bytes());
    Ok(())
}

/// What [`hash_tree`] feeds for the symlink at `link`.
///
/// A target that climbs with `..` or is absolute is fed as the path that it names, through the placeholders of `roots`,
/// where that path resolves as it reads. A relative link that climbs out of the target directory, as `cxx-build` writes
/// one into CARGO_HOME, then hashes alike in worktrees at different depths. Any other target is fed as written.
fn link_target(roots: &Roots, link: &Path) -> io::Result<Vec<u8>> {
    let target = fs::read_link(link)?;
    let normal = |part| matches!(part, Component::Normal(_) | Component::CurDir);
    let path = lexical(&link.parent().unwrap_or(link).join(&target));
    let real = path.canonicalize().ok();
    match path.to_str() {
        Some(path)
            if !target.components().all(normal)
                && real.is_some()
                && real == link.canonicalize().ok() =>
        {
            Ok(roots.normalized(path).into_bytes())
        }
        _ => Ok(target.into_os_string().into_encoded_bytes()),
    }
}

/// The hash that [`hash_tree`] feeds for the file at `path`, remembered in `memos` under its [`identity`] where the
/// file is not UTF-8.
///
/// A UTF-8 file is hashed through the placeholders of `roots`, which differ between worktrees, so it is read every
/// time. A memo of this kind is named apart from those of [`remembered_hash`], so that it also says that the file is
/// not UTF-8. A file that is not [`settled`] is not remembered.
fn tree_file_hash(roots: &Roots, path: &Path, memos: &Path) -> io::Result<blake3::Hash> {
    let start = crate::now() as i64;
    let file = fs::metadata(path)?;
    let before = format!("binary {}", identity_of(&file));
    if let Some(Ok(bytes)) = recalled(memos, &before).map(<[u8; 32]>::try_from) {
        return Ok(blake3::Hash::from_bytes(bytes));
    }
    let map = crate::store::map(path);
    let bytes = match &map {
        Some(map) => std::borrow::Cow::Borrowed(&map[..]),
        None => std::borrow::Cow::Owned(fs::read(path)?),
    };
    if let Ok(text) = std::str::from_utf8(&bytes) {
        return Ok(blake3::hash(roots.normalized(text).as_bytes()));
    }
    let hash = blake3::hash(&bytes);
    if settled(&file, start)
        && identity(path).is_ok_and(|after| format!("binary {after}") == before)
    {
        let _ = remember(memos, &before, hash.as_bytes());
    }
    Ok(hash)
}

/// The output of `rustc -vV`, remembered in `store` for each compiler binary and rustup toolchain.
///
/// Without RUSTUP_TOOLCHAIN the path may be a rustup proxy that picks its compiler by more than its path, so rustc is
/// asked every time.
pub fn compiler(rustc: &str, store: &Path) -> io::Result<Vec<u8>> {
    let version = || Command::new(rustc).arg("-vV").output().map(|output| output.stdout);
    let (Ok(toolchain), Ok(binary)) = (env::var("RUSTUP_TOOLCHAIN"), fs::metadata(rustc)) else {
        return version();
    };
    let identity = format!("{rustc}\n{toolchain}\n{}\n{:?}", binary.len(), binary.modified()?);
    remembered(&store.join("rustc"), &identity, version)
}

/// The output of `run`, remembered in `directory` under the hash of `identity`.
pub fn remembered(
    directory: &Path,
    identity: &str,
    run: impl FnOnce() -> io::Result<Vec<u8>>,
) -> io::Result<Vec<u8>> {
    if let Some(output) = recalled(directory, identity) {
        return Ok(output);
    }
    let output = run()?;
    remember(directory, identity, &output)?;
    Ok(output)
}

/// The output remembered in `directory` under the hash of `identity`, where there is one.
pub fn recalled(directory: &Path, identity: &str) -> Option<Vec<u8>> {
    fs::read(directory.join(blake3::hash(identity.as_bytes()).to_hex().as_str())).ok()
}

/// Remembers `output` in `directory` under the hash of `identity`.
///
/// The memo is staged and renamed into place, so a reader finds it whole or missing.
pub fn remember(directory: &Path, identity: &str, output: &[u8]) -> io::Result<()> {
    let memo = directory.join(blake3::hash(identity.as_bytes()).to_hex().as_str());
    let staging = memo.with_extension(std::process::id().to_string());
    fs::create_dir_all(directory)?;
    fs::write(&staging, output).and_then(|()| crate::store::rename(&staging, &memo))
}

/// The target directory that holds `out_dir` as the output directory of one unit in the layout of cargo.
///
/// Where cargo was given `--target`, a directory named after `triple` lies above the profile's. An output directory
/// of another layout belongs to the nearest ancestor that cargo tagged, and cargo tags only a directory that it
/// created.
pub fn target_directory<'a>(out_dir: &'a Path, triple: Option<&str>) -> Option<&'a Path> {
    let mut ancestors = out_dir.ancestors();
    if ancestors.nth(3).and_then(Path::file_name).is_some_and(|name| name == "build") {
        let above = ancestors.nth(1)?;
        let named = triple.is_some_and(|triple| above.file_name() == Path::new(triple).file_stem());
        return if named { above.parent() } else { Some(above) };
    }
    out_dir.ancestors().find(|directory| directory.join("CACHEDIR.TAG").exists())
}

impl Unit {
    /// Reads the arguments of rustc, or fails with the reason why a key cannot predict their outputs.
    ///
    /// `generated` is OUT_DIR, where a build script wrote the sources that this crate includes. `worktree` is the
    /// worktree of an own crate, and only such a crate may compile incrementally.
    pub fn model(
        args: &[String],
        cargo_home: String,
        generated: Option<String>,
        worktree: Option<String>,
    ) -> Result<Unit, &'static str> {
        let (mut name, mut extra, mut out_dir, mut emit, mut triple) = (None, "", None, "", None);
        let (mut externs, mut search, mut trees, mut links) = (vec![], vec![], vec![], vec![]);
        let (mut linked, mut proc_macro, mut coverage) = (false, false, false);
        let mut rest = args.iter().map(String::as_str);
        while let Some(arg) = rest.next() {
            let (flag, value) = match arg {
                "-C" | "--codegen" | "-L" | "-l" | "-o" | "--extern" | "--out-dir" | "--emit"
                | "--crate-name" | "--crate-type" | "--target" => (arg, rest.next().unwrap_or("")),
                _ if arg.starts_with("--") => arg.split_once('=').unwrap_or((arg, "")),
                _ if arg.starts_with('-') && arg.len() > 1 => arg.split_at(2),
                _ => ("", arg),
            };
            match (flag, value) {
                ("--crate-name", _) => name = Some(value),
                ("--out-dir", _) => out_dir = Some(Path::new(value)),
                ("--emit", _) => emit = value,
                ("--target", _) => triple = Some(value),
                ("--crate-type", _) => {
                    linked |= !matches!(value, "lib" | "rlib" | "staticlib");
                    proc_macro |= value == "proc-macro";
                }
                ("--test", _) => linked = true,
                ("-l", _) => links.push(value.to_string()),
                ("--extern", _) => {
                    externs.extend(value.split_once('=').map(|(_, path)| PathBuf::from(path)))
                }
                ("-C", _) if value.starts_with("extra-filename=") => {
                    extra = &value["extra-filename=".len()..]
                }
                ("-C", _) if value.starts_with("incremental") && worktree.is_none() => {
                    return Err("incremental");
                }
                // The last of these flags wins, as in rustc.
                ("-C" | "--codegen", _)
                    if value.split('=').next() == Some("instrument-coverage") =>
                {
                    let off =
                        matches!(value.split_once('='), Some((_, "n" | "no" | "off" | "false")));
                    coverage = !off;
                }
                ("-L", _) => match value.split_once('=') {
                    Some(("dependency", directory)) => search.push(PathBuf::from(directory)),
                    Some((_, directory)) => trees.push(directory.to_string()),
                    None => trees.push(value.to_string()),
                },
                ("-o", _) => return Err("explicit-output"),
                ("", _) if value.starts_with('@') => return Err("argument-file"),
                _ => {}
            }
        }
        // A remap would make the coverage map name the sources of an own crate relative to its worktree, which a tool
        // that matches them by absolute path, as cargo-llvm-cov does, then fails to find.
        if coverage && worktree.is_some() {
            return Err("coverage");
        }
        let (name, out_dir) = (name.ok_or("no-crate-name")?, out_dir.ok_or("no-out-dir")?);
        let known = |kind| matches!(kind, "dep-info" | "metadata" | "link");
        if !emit.split(',').all(known) || !emit.split(',').any(|kind| kind == "dep-info") {
            return Err("emit");
        }
        let target = target_directory(out_dir, triple);
        let target = format!("{}{MAIN_SEPARATOR}", target.ok_or("no-target-dir")?.display());
        // The JSON diagnostics of rustc escape these characters, so the placeholders would not find a root in them. On
        // Windows a root has a placeholder for its spelling in JSON too.
        let escaped = |c: char| c.is_control() || (c == '\\' && cfg!(unix)) || c == '"';
        if [&target, &cargo_home].into_iter().chain(&worktree).any(|path| path.contains(escaped)) {
            return Err("escaped-path");
        }
        let roots = Roots { target, cargo_home, worktree };
        #[cfg(windows)]
        if !crate::windows::is_keyable(&roots, trees.iter().map(String::as_str)) {
            return Err("path-spelling");
        }
        let stem = format!("{name}{extra}");
        let apple =
            cfg!(target_os = "macos") && triple.is_none_or(|triple| triple.contains("-apple-"));
        let mut link_paths = Vec::new();
        if let Some(root) = roots.worktree.as_deref().filter(|_| apple && linked) {
            let prefix = roots.separate_target().map_or(format!("{root}/"), str::to_string);
            link_paths.push(format!("link-arg=-Wl,-oso_prefix,{prefix}"));
            if proc_macro {
                link_paths.push(format!("link-arg=-Wl,-install_name,@rpath/lib{stem}.dylib"));
            }
        }
        let link_paths = link_paths.into_iter().flat_map(|flag| ["-C".to_string(), flag]).collect();
        let directory = env::current_dir().map_err(|_| "no-working-directory")?;
        let out_dir = out_dir.to_path_buf();
        Ok(Unit {
            out_dir,
            roots,
            directory,
            stem,
            externs,
            search,
            searched: std::sync::OnceLock::new(),
            trees,
            links,
            generated,
            link_paths,
            package: env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from),
        })
    }

    /// The file names of the proc macros that rustc may load for this crate, from its externs and search directories.
    ///
    /// cargo names the directory of each crate that the externs depend on, so a macro that a dependency re-exports, as
    /// `sqlx` does `sqlx-macros`, is found too.
    fn proc_macros(&self) -> Vec<String> {
        let found = self.searched().iter().filter_map(|entries| entries.as_ref().ok()).flatten();
        let libraries = self.externs.iter().chain(found);
        let dynamic = |path: &&PathBuf| {
            path.extension().is_some_and(|kind| kind == "dylib" || kind == "so" || kind == "dll")
        };
        let names = libraries
            .filter(dynamic)
            .map(|path| path.file_name().map(|name| name.to_string_lossy().into_owned()));
        names.flatten().collect()
    }

    /// The paths of the entries of each `-L dependency=` directory, or the error of its listing, read on every core.
    ///
    /// A crate depends on hundreds of crates, whose directories the check for sqlx, the key and the reads each want.
    /// They do not change while it compiles, because cargo starts it only once they are built.
    fn searched(&self) -> &[Result<Vec<PathBuf>, io::ErrorKind>] {
        self.searched.get_or_init(|| {
            let list = |directory: &PathBuf| {
                let entries = fs::read_dir(directory).and_then(|entries| {
                    entries.map(|entry| entry.map(|entry| entry.path())).collect()
                });
                Ok(entries.map_err(|error| error.kind()))
            };
            on_every_core(&self.search, list)
                .unwrap_or_else(|error| vec![Err(error.kind()); self.search.len()])
        })
    }

    /// Whether this own crate loads a proc macro that may read what no file of its worktree holds.
    ///
    /// sqlx-macros queries the database of DATABASE_URL unless SQLX_OFFLINE is true, and reads its offline data from
    /// SQLX_OFFLINE_DIR wherever that lies.
    pub fn reads_undeclared(&self) -> bool {
        let offline = env::var("SQLX_OFFLINE")
            .is_ok_and(|value| value.eq_ignore_ascii_case("true") || value == "1")
            && env::var_os("SQLX_OFFLINE_DIR").is_none();
        let sqlx = |name: &String| {
            let name = name.strip_prefix("lib").unwrap_or(name);
            name.starts_with("sqlx_macros-") || name.starts_with("sqlx_macros.")
        };
        self.roots.worktree.is_some() && !offline && self.proc_macros().iter().any(sqlx)
    }

    /// The directories whose files a proc macro of this own crate may read without declaring them, each with the kind
    /// of its read in [`Unit::variant`].
    ///
    /// The package is walked whole, as a `tree`. The workspace root, which rustc runs in, is a `top`, of which only the
    /// hidden files and directories count, as the `.sqlx` of sqlx and a `.env` do. Its other files, such as a README
    /// or `Cargo.lock`, would make every such crate of the workspace compile again after an edit of any one of them.
    fn packages(&self) -> Vec<(&'static str, &Path)> {
        let mut packages = Vec::from_iter(self.package.as_deref().map(|package| ("tree", package)));
        if self.package.as_deref() != Some(&self.directory) {
            packages.push(("top", &self.directory));
        }
        packages
    }

    /// Whether `path` is the target directory or holds an entry of [`ROOT_MARKERS`], as another worktree, package or
    /// target directory does, which no walk enters.
    fn other_root(&self, path: &Path) -> bool {
        self.is_target(path)
            || ROOT_MARKERS.iter().any(|name| fs::symlink_metadata(path.join(name)).is_ok())
    }

    /// Whether `path` is the target directory or the separate one.
    fn is_target(&self, path: &Path) -> bool {
        let targets = [Some(self.roots.target.as_str()), self.roots.separate_target()];
        targets.into_iter().flatten().any(|target| Path::new(target) == path)
    }

    /// The directories outside the walk of the package that hold a file of `declared`, each as a `dir` read.
    ///
    /// A proc macro that lists a directory, as `sqlx::migrate!` lists `migrations`, declares each file that it reads
    /// but not the directory, so a file added there would change nothing else that selects the entry. A directory of
    /// another package counts too, because a macro may list it alike. Left out are the directories that the `tree` of
    /// the package walks, those under an entry of [`UNREAD`], and those that [`Unit::other_root`] names, such as the
    /// workspace root, whose hidden files the `top` read covers. A file outside the worktree is a read of its own or
    /// leaves the crate uncached already.
    fn listed<'a>(&self, declared: impl Iterator<Item = &'a str>) -> Vec<String> {
        let package = self.package.as_deref().map(lexical);
        let walked = |directory: &Path| {
            package.as_deref().is_some_and(|package| {
                directory.starts_with(package)
                    && !directory
                        .ancestors()
                        .take_while(|ancestor| *ancestor != package)
                        .any(|ancestor| self.other_root(ancestor))
            })
        };
        let mut listed = Vec::new();
        for path in declared.filter(|path| path.starts_with(&format!("{WORKTREE}/"))) {
            let Some(directory) = Path::new(path).parent() else { continue };
            let local = PathBuf::from(self.roots.localized(&directory.to_string_lossy()));
            let unread = directory
                .components()
                .any(|component| UNREAD.iter().any(|name| component.as_os_str() == *name));
            if !unread && !walked(&local) && !self.other_root(&local) {
                listed.push(format!("dir {}", directory.display()));
            }
        }
        listed
    }

    /// The hash of the names and contents of the files under `root`, other than those of `declared`.
    ///
    /// A `top` walk takes only the hidden entries of `root`. A `dir` walk takes only the entries of `root` itself, of
    /// which a directory counts by its name. No walk takes an entry of [`UNREAD`], an editor's backup or swap file, or
    /// a directory that [`Unit::other_root`] names. Fails with `changed-source` where a file changed less than
    /// [`SETTLED`] before `started`, or after it. The hash is remembered in `memos` under the name and [`identity_of`]
    /// of every entry, where each file is [`settled`] and keeps its identity while it is hashed.
    fn package_hash(
        &self,
        root: &Path,
        kind: &str,
        declared: &std::collections::HashSet<PathBuf>,
        started: Option<SystemTime>,
        memos: &Path,
    ) -> Result<String, &'static str> {
        let start = crate::now() as i64;
        let skipped = |path: &Path, name: &str, directory: bool| {
            let backup = [".swp", ".swo", ".swx", "~"].iter().any(|end| name.ends_with(end));
            UNREAD.contains(&name)
                || backup
                || name.starts_with(".#")
                || declared.contains(path)
                || (directory && self.is_target(path))
                || (directory && kind == "dir" && self.other_root(path))
        };
        // Each entry of the walk with its metadata, which does not follow a symlink, or None for a directory of a
        // `dir` walk, which counts by its name alone.
        let (mut files, mut pending) = (Vec::new(), vec![root.to_path_buf()]);
        while let Some(directory) = pending.pop() {
            let entries =
                fs::read_dir(&directory).and_then(Iterator::collect::<io::Result<Vec<_>>>);
            let entries = entries.map_err(|_| "unreadable-read")?;
            let marker =
                |entry: &fs::DirEntry| ROOT_MARKERS.iter().any(|name| entry.file_name() == *name);
            if directory != root && entries.iter().any(marker) {
                continue;
            }
            for entry in entries {
                let (path, entry_kind) =
                    (entry.path(), entry.file_type().map_err(|_| "unreadable-read")?);
                let name = entry.file_name().to_string_lossy().into_owned();
                if skipped(&path, &name, entry_kind.is_dir())
                    || (kind == "top" && directory == root && !name.starts_with('.'))
                {
                    continue;
                }
                if entry_kind.is_dir() && kind == "dir" {
                    files.push((path, None));
                } else if entry_kind.is_dir() {
                    pending.push(path);
                } else {
                    files.push((path, Some(entry.metadata().map_err(|_| "unreadable-read")?)));
                }
            }
        }
        files.sort_by(|(a, _), (b, _)| a.cmp(b));
        fn relative<'a>(path: &'a Path, root: &Path) -> &'a [u8] {
            path.strip_prefix(root).unwrap_or(path).as_os_str().as_encoded_bytes()
        }
        let mut listing = blake3::Hasher::new();
        for (path, file) in &files {
            let identity = file.as_ref().map(identity_of).unwrap_or_default();
            for field in [relative(path, root), identity.as_bytes()] {
                listing.update(&(field.len() as u64).to_le_bytes()).update(field);
            }
        }
        let listing = format!("walk {}", listing.finalize().to_hex());
        let memo = recalled(memos, &listing).and_then(|bytes| <[u8; 32]>::try_from(bytes).ok());
        let hash = |(path, file): &(PathBuf, Option<fs::Metadata>)| {
            let contents = match file {
                None => return io::Result::Ok((Vec::new(), None)),
                Some(file) if file.is_symlink() => {
                    fs::read_link(path)?.into_os_string().into_encoded_bytes()
                }
                Some(_) => file_hash(path)?.as_bytes().to_vec(),
            };
            // The metadata is read again after the hash, so that a write during the hash is seen.
            Ok((contents, Some(fs::symlink_metadata(path)?)))
        };
        let hashed = match memo {
            Some(_) => None,
            None => Some(on_every_core(&files, hash).map_err(|_| "unreadable-read")?),
        };
        let (mut hasher, mut unchanged) = (blake3::Hasher::new(), true);
        for (at, (path, file)) in files.iter().enumerate() {
            let (contents, after) = match &hashed {
                Some(hashed) => (hashed[at].0.as_slice(), hashed[at].1.as_ref()),
                None => (&[][..], file.as_ref()),
            };
            if let Some(after) = after {
                let modified = after.modified().map_err(|_| "unreadable-read")?;
                if let Some(started) = started
                    && started.duration_since(modified).map_or(true, |age| age < SETTLED)
                {
                    return Err("changed-source");
                }
                unchanged &= settled(after, start)
                    && file.as_ref().map(identity_of) == Some(identity_of(after));
            }
            for field in [relative(path, root), contents] {
                hasher.update(&(field.len() as u64).to_le_bytes()).update(field);
            }
        }
        let hash = match memo {
            Some(bytes) => blake3::Hash::from_bytes(bytes),
            None => hasher.finalize(),
        };
        if memo.is_none() && unchanged {
            let _ = remember(memos, &listing, hash.as_bytes());
        }
        Ok(hash.to_hex().to_string())
    }

    /// The files that rustc wrote, or will write, into the output directory for this crate.
    pub fn outputs(&self) -> io::Result<Vec<PathBuf>> {
        let mut outputs = Vec::new();
        for entry in fs::read_dir(&self.out_dir)? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            let suffix = |name: &str| {
                name.strip_prefix(&self.stem)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
            };
            if suffix(&name) || name.strip_prefix("lib").is_some_and(suffix) {
                outputs.push(self.out_dir.join(name));
            }
        }
        Ok(outputs)
    }

    /// The executable of a third-party build script that cargo runs by this path, as it does without an extra filename.
    pub fn script(&self) -> Option<PathBuf> {
        let script = self.roots.worktree.is_none()
            && self.stem.starts_with("build_script_")
            && !self.stem.contains('-');
        #[cfg(unix)]
        let name = &self.stem;
        #[cfg(windows)]
        let name = format!("{}.exe", self.stem);
        script.then(|| self.out_dir.join(name))
    }

    /// The flags that name paths in spans, panics, debug info and links, which the key covers.
    ///
    /// A file under the target directory, such as a generated source of a build script, is named from `target/`. A
    /// source of an own crate is named relative to its worktree.
    pub fn remap(&self) -> Vec<String> {
        let worktree =
            self.roots.worktree.as_ref().map(|root| format!("--remap-path-prefix={root}/="));
        let target = (worktree.is_none() || self.roots.separate_target().is_some())
            .then(|| format!("--remap-path-prefix={}=target/", self.roots.target));
        let flags = target.into_iter().chain(worktree).chain(self.link_paths.iter().cloned());
        flags.collect()
    }

    /// `args` through the placeholders, without `--out-dir` and `-C incremental`.
    pub fn normalized_args(&self, args: &[String]) -> Vec<String> {
        let (mut normalized, mut args) = (Vec::new(), args.iter().peekable());
        while let Some(arg) = args.next() {
            let incremental = |value: &&String| value.starts_with("incremental=");
            if arg == "--out-dir" || (arg == "-C" && args.peek().is_some_and(incremental)) {
                args.next();
            } else if !arg.starts_with("--out-dir=") && !arg.starts_with("-Cincremental=") {
                normalized.push(self.roots.normalized(arg));
            }
        }
        normalized
    }

    /// The key of this invocation by `rustc` with `args` in `environment`, as hex.
    ///
    /// A crate that only needs metadata is keyed by the `.rmeta` of each dependency, because cargo
    /// starts it while their other files are still being written.
    pub fn key(
        &self,
        rustc: &str,
        args: &[String],
        store: &Path,
        environment: impl Iterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
    ) -> io::Result<String> {
        let mut hasher = blake3::Hasher::new();
        let mut field = |bytes: &[u8]| {
            hasher.update(&(bytes.len() as u64).to_le_bytes()).update(bytes);
        };
        field(&compiler(rustc, store)?);
        if self.roots.worktree.is_some() {
            field(OWN);
        }
        field(self.roots.normalized(&self.directory.to_string_lossy()).as_bytes());
        for arg in self.normalized_args(args) {
            field(arg.as_bytes());
        }
        for flag in self.remap() {
            field(self.roots.normalized(&flag).as_bytes());
        }
        field(b"environment");
        let lossy = |text: std::ffi::OsString| text.to_string_lossy().into_owned();
        let mut variables =
            Vec::from_iter(environment.map(|(name, value)| (lossy(name), lossy(value))));
        // Windows reads a variable whatever the case of its name.
        #[cfg(windows)]
        variables.iter_mut().for_each(|(name, _)| name.make_ascii_uppercase());
        variables.sort();
        for (name, value) in variables.iter().filter(|(name, _)| is_keyed(name)) {
            field(name.as_bytes());
            field(self.roots.normalized(value).as_bytes());
        }
        field(b"libraries");
        let metadata_only =
            self.externs.iter().any(|path| path.extension().is_some_and(|kind| kind == "rmeta"));
        let mut libraries = self.externs.clone();
        for entries in self.searched() {
            // A cross build names the host's directory, which does not exist until a host crate is built.
            let entries = match entries {
                Err(io::ErrorKind::NotFound) => continue,
                entries => entries.as_ref().map_err(|&kind| io::Error::from(kind))?,
            };
            for path in entries {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                if name.starts_with("lib")
                    && !name.ends_with(".d")
                    && (!metadata_only || name.ends_with(".rmeta"))
                {
                    libraries.push(path.clone());
                }
            }
        }
        libraries.sort();
        libraries.dedup();
        for (library, hash) in libraries.iter().zip(file_hashes(&libraries, &store.join("hashes"))?)
        {
            field(library.file_name().unwrap_or_default().as_encoded_bytes());
            field(hash.as_bytes());
        }
        field(b"trees");
        for tree in &self.trees {
            field(self.roots.normalized(tree).as_bytes());
            if tree.starts_with(&self.roots.target) {
                hash_tree(&self.roots, Path::new(tree), &store.join("hashes"), &mut field)?;
            }
        }
        for link in &self.links {
            field(link.as_bytes());
            let memos = store.join("hashes");
            let library = self.native_library(link).map(|path| remembered_hash(&path, &memos));
            let library = library.transpose()?;
            field(library.as_ref().map_or(b"default".as_slice(), |hash| hash.as_bytes()));
        }
        Ok(hasher.finalize().to_hex().to_string())
    }

    /// The file that the `-l` value `link` names, in the first native search directory that has one.
    ///
    /// It is `None` for a framework and for a library that the linker finds in its default directories, such as
    /// libz in the SDK of macOS.
    fn native_library(&self, link: &str) -> Option<PathBuf> {
        let (kind, name) = link.split_once('=').unwrap_or(("dylib", link));
        let name = name.split(':').next().unwrap_or(name);
        #[cfg(windows)]
        let spellings = match kind {
            _ if kind.contains("+verbatim") => vec![name.to_string()],
            _ if kind.starts_with("static") => vec![format!("{name}.lib"), format!("lib{name}.a")],
            _ => ["{}.lib", "{}.dll.lib", "lib{}.dll.a", "{}.dll.a", "lib{}.a"]
                .map(|spelling| spelling.replace("{}", name))
                .to_vec(),
        };
        #[cfg(unix)]
        let dynamic: &[&str] = if cfg!(target_os = "macos") { &["tbd", "dylib"] } else { &["so"] };
        #[cfg(unix)]
        let spellings = match kind {
            _ if kind.contains("+verbatim") => vec![name.to_string()],
            _ if kind.starts_with("framework") => vec![],
            _ if kind.starts_with("static") => vec![format!("lib{name}.a")],
            _ => dynamic.iter().chain(&["a"]).map(|kind| format!("lib{name}.{kind}")).collect(),
        };
        let mut candidates = self
            .trees
            .iter()
            .flat_map(|tree| spellings.iter().map(move |file| Path::new(tree).join(file)));
        candidates.find(|path| path.is_file())
    }

    /// Rewrites dep-info through the placeholders, and lists what the crate read that the key does not cover.
    ///
    /// The list names every unkeyed environment variable and every file under OUT_DIR or the worktree, by its path
    /// resolved against the working directory. Returns `None` where the crate read a path with a control character, or
    /// a file elsewhere outside CARGO_HOME and the hashed trees, because an entry cannot cover it.
    pub fn portable_dep_info(&self, dep_info: &str) -> Option<(String, String)> {
        // A directory is spelled as each path of the dep-info is, through `lexical`, because cargo joins the directory
        // of a unit with `/` on Windows.
        let directory = |path: &str| {
            let path = lexical(Path::new(path)).to_string_lossy().into_owned();
            let path = path.trim_end_matches(MAIN_SEPARATOR);
            self.roots.normalized(&format!("{path}{MAIN_SEPARATOR}"))
        };
        let mut covered = vec![CARGO_HOME.to_string(), directory(&self.out_dir.to_string_lossy())];
        let hashed = self.trees.iter().filter(|tree| tree.starts_with(&self.roots.target));
        covered.extend(hashed.map(|tree| directory(tree)));
        let mut selecting = Vec::from_iter(self.generated.as_deref().map(directory));
        selecting.extend(self.roots.worktree.as_ref().map(|_| format!("{WORKTREE}/")));
        let mut reads = Vec::new();
        for line in dep_info.lines() {
            if let Some(variable) = line.strip_prefix("# env-dep:") {
                let name = variable.split('=').next().unwrap_or(variable);
                reads.extend((!is_keyed(name)).then(|| format!("env {name}")));
            } else if line.contains(char::is_control) {
                return None;
            } else if !line.starts_with('#') {
                let (output, sources) = line.split_once(": ").unwrap_or(("", line));
                let sources = sources.trim_end_matches(':').replace("\\ ", "\0");
                let sources = sources.split_whitespace().map(|path| path.replace('\0', " "));
                for path in sources.chain((!output.is_empty()).then(|| output.to_string())) {
                    let path = lexical(&self.directory.join(path));
                    let path = self.roots.normalized(&path.to_string_lossy());
                    if selecting.iter().any(|prefix| path.starts_with(prefix)) {
                        reads.push(format!("file {path}"));
                    } else if !covered.iter().any(|prefix| path.starts_with(prefix)) {
                        return None;
                    }
                }
            }
        }
        // A proc macro may read a file of the package or the workspace root, as sqlx reads `.sqlx`, without declaring it,
        // or list a directory of the files that it declares, as `sqlx::migrate!` does.
        if self.roots.worktree.is_some() && !self.proc_macros().is_empty() {
            for (kind, directory) in self.packages() {
                let path = self.roots.normalized(&lexical(directory).to_string_lossy());
                if path != WORKTREE && !path.starts_with(&format!("{WORKTREE}/")) {
                    return None;
                }
                reads.push(format!("{kind} {path}"));
            }
            let declared = reads.iter().filter_map(|read| read.strip_prefix("file "));
            reads.extend(self.listed(declared));
        }
        reads.sort();
        reads.dedup();
        Some((self.roots.dep_info(dep_info, Roots::normalized), reads.join("\n")))
    }

    /// The name of the entry of `key` for the current value of each of `reads`, which an earlier compile read.
    ///
    /// A compile that finds every one of them unchanged reads nothing else, because what it reads next follows from
    /// what it has read. `started` is the start of the compile that `reads` come from, where they are to be stored.
    /// `memos` holds the hashes that the store remembers.
    ///
    /// Fails, so that the compile is neither looked up nor stored, where:
    /// - an own crate read no source of its worktree;
    /// - a file of `reads` cannot be hashed;
    /// - a source of the worktree changed less than [`SETTLED`] before `started`, or after it, because rustc may then
    ///   have read other contents than those hashed.
    pub fn variant(
        &self,
        key: &str,
        reads: &str,
        started: Option<SystemTime>,
        memos: &Path,
    ) -> Result<String, &'static str> {
        let source = format!("file {WORKTREE}/");
        if self.roots.worktree.is_some() && !reads.lines().any(|read| read.starts_with(&source)) {
            return Err("no-source-read");
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update(key.as_bytes());
        let declared = reads.lines().filter_map(|read| read.strip_prefix("file "));
        let declared = declared.map(|path| PathBuf::from(self.roots.localized(path))).collect();
        for read in reads.lines() {
            let value = match read.split_once(' ') {
                Some((kind @ ("tree" | "top" | "dir"), path)) => {
                    let root = PathBuf::from(self.roots.localized(path));
                    Some(self.package_hash(&root, kind, &declared, started, memos)?)
                }
                Some(("file", path)) => {
                    let file = PathBuf::from(self.roots.localized(path));
                    let hash = file_hash(&file).map_err(|_| "unreadable-read")?;
                    // The mtime is read after the hash, so that a write during the hash is seen.
                    if let Some(started) = started.filter(|_| path.starts_with(WORKTREE)) {
                        let modified = fs::metadata(&file).and_then(|metadata| metadata.modified());
                        let age = started.duration_since(modified.map_err(|_| "unreadable-read")?);
                        if age.map_or(true, |age| age < SETTLED) {
                            return Err("changed-source");
                        }
                    }
                    Some(hash.to_hex().to_string())
                }
                Some(("env", name)) => {
                    env::var(name).ok().map(|value| self.roots.normalized(&value))
                }
                _ => return Err("unreadable-read"),
            };
            hasher.update(format!("\n{read}={value:?}").as_bytes());
        }
        Ok(if reads.is_empty() { key.to_string() } else { hasher.finalize().to_hex().to_string() })
    }
}
