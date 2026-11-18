//! One cached rustc invocation, of a third-party crate or of the repository's own, and the key of what its outputs
//! depend on.
//!
//! A key covers what cargo's own rebuild checks cover, and shares their blind spot: a proc macro or build script that
//! reads a file or variable without declaring it to cargo gets a stale output. Paths under the target directory,
//! CARGO_HOME and the worktree enter a key through placeholders, so that every worktree computes the same key. What
//! rustc names only in its dep-info, such as a source of an own crate, a file of OUT_DIR or a variable set by a build
//! script, selects among the entries of one key.

use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::{env, fs, io};

pub const TARGET: &str = "{trucker:target}/";
pub const CARGO_HOME: &str = "{trucker:cargo-home}/";
pub const WORKTREE: &str = "{trucker:worktree}";

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
    fn placeholders(&self) -> Vec<(&str, &str)> {
        let mut pairs =
            vec![(self.target.as_str(), TARGET), (self.cargo_home.as_str(), CARGO_HOME)];
        pairs.extend(self.worktree.as_deref().map(|root| (root, WORKTREE)));
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

    /// The target directory where it lies outside the worktree, which then needs a remap and a scan of its own.
    pub fn separate_target(&self) -> Option<&str> {
        let root = self.worktree.as_deref()?;
        let inside = self.target.strip_prefix(root).is_some_and(|rest| rest.starts_with('/'));
        (!inside).then_some(self.target.as_str())
    }

    /// Rewrites `dep_info` with `rewrite`, through each root as rustc writes it in that part of a line.
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
    /// Each native search directory, in rustc's order, because dep-info leaves out a linked library.
    trees: Vec<String>,
    /// The value of each `-l`, such as `z` or `static=foo`.
    links: Vec<String>,
    /// OUT_DIR, of which an entry covers only the files that rustc read.
    generated: Option<String>,
    /// The flags that make a link on macOS leave out the paths of the worktree and the target directory.
    link_paths: Vec<String>,
}

/// CARGO_HOME with a trailing slash.
pub fn cargo_home() -> String {
    let home = env::var("CARGO_HOME")
        .unwrap_or_else(|_| format!("{}/.cargo", env::var("HOME").unwrap_or_default()));
    format!("{}/", home.trim_end_matches('/'))
}

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

/// Whether `args` ask rustc about itself or compile stdin, as cargo's probes do, which are never cached or logged.
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

/// Whether the key covers the environment variable `name`, which cargo sets for rustc or a build script.
///
/// A variable that the user sets, such as CARGO_TARGET_DIR, enters only where dep-info names it.
pub fn is_keyed(name: &str) -> bool {
    let compiler = "CARGO OUT_DIR MACOSX_DEPLOYMENT_TARGET SDKROOT RUSTC_BOOTSTRAP CARGO_MANIFEST_DIR \
                    CARGO_MANIFEST_PATH CARGO_MANIFEST_LINKS CARGO_CRATE_NAME CARGO_BIN_NAME CARGO_PRIMARY_PACKAGE \
                    CARGO_TARGET_TMPDIR CARGO_RUSTC_CURRENT_DIR CARGO_SBOM_PATH CARGO_ENCODED_RUSTFLAGS \
                    CARGO_TRIM_PATHS_SCOPE CARGO_TRIM_PATHS_REMAP";
    let prefixes = ["CARGO_PKG_", "CARGO_BIN_EXE_", "CARGO_CFG_", "CARGO_FEATURE_", "CARGO_DEP_"];
    compiler.split_whitespace().any(|known| known == name)
        || prefixes.iter().any(|prefix| name.starts_with(prefix))
}

pub fn file_hash(path: &Path) -> io::Result<blake3::Hash> {
    Ok(blake3::Hasher::new().update_mmap(path)?.finalize())
}

/// Feeds the name and contents of every file under `directory` to `field`, in a fixed order.
pub fn hash_tree(root: &Path, directory: &Path, field: &mut impl FnMut(&[u8])) -> io::Result<()> {
    let mut entries = fs::read_dir(directory)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        field(path.strip_prefix(root).unwrap_or(&path).as_os_str().as_encoded_bytes());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            hash_tree(root, &path, field)?;
        } else if kind.is_symlink() {
            field(fs::read_link(&path)?.as_os_str().as_encoded_bytes());
        } else {
            field(file_hash(&path)?.as_bytes());
        }
    }
    Ok(())
}

/// The output of `rustc -vV`, remembered in `store` for each compiler binary and rustup toolchain.
///
/// Without RUSTUP_TOOLCHAIN the path may be a rustup proxy, whose compiler depends on more than its
/// path, so rustc is asked every time.
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
    let memo = directory.join(blake3::hash(identity.as_bytes()).to_hex().as_str());
    if let Ok(output) = fs::read(&memo) {
        return Ok(output);
    }
    let output = run()?;
    let staging = memo.with_extension(std::process::id().to_string());
    fs::create_dir_all(directory)?;
    fs::write(&staging, &output).and_then(|()| fs::rename(&staging, &memo))?;
    Ok(output)
}

/// The target directory that `out_dir` lies in, which is the output directory of one unit in cargo's layout.
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
    /// Reads rustc's arguments, or names the reason that their outputs cannot be predicted from a key.
    ///
    /// `generated` is OUT_DIR, where a build script wrote the sources that this crate includes. `worktree` is that of
    /// an own crate, whose incremental compile is allowed.
    pub fn model(
        args: &[String],
        cargo_home: String,
        generated: Option<String>,
        worktree: Option<String>,
    ) -> Result<Unit, &'static str> {
        let (mut name, mut extra, mut out_dir, mut emit, mut triple) = (None, "", None, "", None);
        let (mut externs, mut search, mut trees, mut links) = (vec![], vec![], vec![], vec![]);
        let (mut linked, mut proc_macro) = (false, false);
        let mut rest = args.iter().map(String::as_str);
        while let Some(arg) = rest.next() {
            let (flag, value) = match arg {
                "-C" | "-L" | "-l" | "-o" | "--extern" | "--out-dir" | "--emit"
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
        let (name, out_dir) = (name.ok_or("no-crate-name")?, out_dir.ok_or("no-out-dir")?);
        let known = |kind| matches!(kind, "dep-info" | "metadata" | "link");
        if !emit.split(',').all(known) || !emit.split(',').any(|kind| kind == "dep-info") {
            return Err("emit");
        }
        let target = target_directory(out_dir, triple);
        let target = format!("{}/", target.ok_or("no-target-dir")?.display());
        // rustc's JSON diagnostics escape these characters, where the placeholders would not find a root.
        let escaped = |c: char| c.is_control() || c == '\\' || c == '"';
        if [&target, &cargo_home].into_iter().chain(&worktree).any(|path| path.contains(escaped)) {
            return Err("escaped-path");
        }
        let roots = Roots { target, cargo_home, worktree };
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
            trees,
            links,
            generated,
            link_paths,
        })
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
        script.then(|| self.out_dir.join(&self.stem))
    }

    /// The flags that name paths in spans, panics, debug info and links, which the key covers.
    ///
    /// A file under the target directory, such as a build script's generated source, is named from `target/`, and a
    /// source of an own crate relative to its worktree.
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

    /// The key of this invocation by `rustc` with `args`, as hex.
    ///
    /// A crate that only needs metadata is keyed by the `.rmeta` of each dependency, because cargo
    /// starts it while their other files are still being written.
    pub fn key(&self, rustc: &str, args: &[String], store: &Path) -> io::Result<String> {
        let mut hasher = blake3::Hasher::new();
        let mut field = |bytes: &[u8]| {
            hasher.update(&(bytes.len() as u64).to_le_bytes()).update(bytes);
        };
        field(&compiler(rustc, store)?);
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
            Vec::from_iter(env::vars_os().map(|(name, value)| (lossy(name), lossy(value))));
        variables.sort();
        for (name, value) in variables.iter().filter(|(name, _)| is_keyed(name)) {
            field(name.as_bytes());
            field(self.roots.normalized(value).as_bytes());
        }
        field(b"libraries");
        let metadata_only =
            self.externs.iter().any(|path| path.extension().is_some_and(|kind| kind == "rmeta"));
        let mut libraries = self.externs.clone();
        for directory in &self.search {
            // A cross build names the host's directory, which does not exist until a host crate is built.
            let entries = match fs::read_dir(directory) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                entries => entries?,
            };
            for entry in entries {
                let path = entry?.path();
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                if name.starts_with("lib")
                    && !name.ends_with(".d")
                    && (!metadata_only || name.ends_with(".rmeta"))
                {
                    libraries.push(path);
                }
            }
        }
        libraries.sort();
        libraries.dedup();
        for library in &libraries {
            field(library.file_name().unwrap_or_default().as_encoded_bytes());
            field(file_hash(library)?.as_bytes());
        }
        field(b"trees");
        for tree in &self.trees {
            field(self.roots.normalized(tree).as_bytes());
            if tree.starts_with(&self.roots.target) {
                hash_tree(Path::new(tree), Path::new(tree), &mut field)?;
            }
        }
        for link in &self.links {
            field(link.as_bytes());
            let library = self.native_library(link).map(|path| file_hash(&path)).transpose()?;
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
        let dynamic: &[&str] = if cfg!(target_os = "macos") { &["tbd", "dylib"] } else { &["so"] };
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
    /// The list names each unkeyed environment variable and each file under OUT_DIR or the worktree, by its path
    /// resolved against the working directory. Returns `None` where the crate read a file elsewhere outside
    /// CARGO_HOME and the hashed trees, or a path with a control character, which no entry can cover.
    pub fn portable_dep_info(&self, dep_info: &str) -> Option<(String, String)> {
        let directory =
            |path: &str| self.roots.normalized(&format!("{}/", path.trim_end_matches('/')));
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
        reads.sort();
        reads.dedup();
        Some((self.roots.dep_info(dep_info, Roots::normalized), reads.join("\n")))
    }

    /// The name of the entry of `key` for the current value of each of `reads`, which an earlier compile read.
    ///
    /// A compile that finds every one of them unchanged reads nothing else, because what it reads next follows from
    /// what it has read.
    pub fn variant(&self, key: &str, reads: &str) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(key.as_bytes());
        for read in reads.lines() {
            let value = match read.split_once(' ') {
                Some(("file", path)) => file_hash(Path::new(&self.roots.localized(path)))
                    .ok()
                    .map(|hash| hash.to_hex().to_string()),
                Some((_, name)) => env::var(name).ok().map(|value| self.roots.normalized(&value)),
                None => None,
            };
            hasher.update(format!("\n{read}={value:?}").as_bytes());
        }
        if reads.is_empty() { key.to_string() } else { hasher.finalize().to_hex().to_string() }
    }
}
