//! One rustc invocation of a third-party crate, and the key of everything that its outputs depend on.
//!
//! A key is built so that a wrong hit is impossible rather than so that the key is small. Paths under the target
//! directory and CARGO_HOME enter it through placeholders, so that every worktree computes the same key. What rustc
//! names only in its dep-info, a file of OUT_DIR or a variable set by a build script, selects among the entries of
//! one key.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs, io};

pub const TARGET: &str = "{crate-cache:target}/";
pub const CARGO_HOME: &str = "{crate-cache:cargo-home}/";

/// The directories whose paths differ between worktrees or machines, each ending in a slash.
pub struct Roots {
    pub target: String,
    pub cargo_home: String,
}

impl Roots {
    pub fn normalized(&self, text: &str) -> String {
        text.replace(&self.target, TARGET).replace(&self.cargo_home, CARGO_HOME)
    }

    pub fn localized(&self, text: &str) -> String {
        text.replace(TARGET, &self.target).replace(CARGO_HOME, &self.cargo_home)
    }
}

pub struct Unit {
    pub out_dir: PathBuf,
    pub roots: Roots,
    /// The crate name and `-C extra-filename`, which start the name of every output after an optional `lib`.
    stem: String,
    externs: Vec<PathBuf>,
    /// Each `-L dependency=` directory, from which rustc loads the crates that the externs depend on.
    search: Vec<PathBuf>,
    /// Each native search directory, whose whole contents enter the key, because dep-info leaves out a linked library.
    trees: Vec<String>,
    /// OUT_DIR, of which an entry covers only the files that rustc read.
    generated: Option<String>,
}

/// CARGO_HOME with a trailing slash.
pub fn cargo_home() -> String {
    let home = env::var("CARGO_HOME")
        .unwrap_or_else(|_| format!("{}/.cargo", env::var("HOME").unwrap_or_default()));
    format!("{}/", home.trim_end_matches('/'))
}

/// Whether `input` is a source of the registry or of a git dependency, which never changes once unpacked.
pub fn is_third_party(input: &str, cargo_home: &str) -> bool {
    let sources = input.strip_prefix(cargo_home);
    sources
        .is_some_and(|path| path.starts_with("registry/src/") || path.starts_with("git/checkouts/"))
}

/// Whether the key covers the environment variable `name`.
fn is_keyed(name: &str) -> bool {
    let compiler = ["CARGO", "OUT_DIR", "MACOSX_DEPLOYMENT_TARGET", "SDKROOT", "RUSTC_BOOTSTRAP"];
    compiler.contains(&name) || (name.starts_with("CARGO_") && name != "CARGO_MAKEFLAGS")
}

fn file_hash(path: &Path) -> io::Result<blake3::Hash> {
    Ok(blake3::Hasher::new().update_mmap(path)?.finalize())
}

/// Feeds the name and contents of every file under `directory` to `field`, in a fixed order.
fn hash_tree(root: &Path, directory: &Path, field: &mut impl FnMut(&[u8])) -> io::Result<()> {
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
fn compiler(rustc: &str, store: &Path) -> io::Result<Vec<u8>> {
    let version = || Command::new(rustc).arg("-vV").output().map(|output| output.stdout);
    let (Ok(toolchain), Ok(binary)) = (env::var("RUSTUP_TOOLCHAIN"), fs::metadata(rustc)) else {
        return version();
    };
    let identity = format!("{rustc}\n{toolchain}\n{}\n{:?}", binary.len(), binary.modified()?);
    let memo = store.join("rustc").join(blake3::hash(identity.as_bytes()).to_hex().as_str());
    if let Ok(version) = fs::read(&memo) {
        return Ok(version);
    }
    let version = version()?;
    let staging = memo.with_extension(std::process::id().to_string());
    fs::create_dir_all(store.join("rustc"))?;
    fs::write(&staging, &version).and_then(|()| fs::rename(&staging, &memo))?;
    Ok(version)
}

impl Unit {
    /// Reads rustc's arguments, or names the reason that their outputs cannot be predicted from a key.
    ///
    /// `generated` is OUT_DIR, where a build script wrote the sources that this crate includes.
    pub fn model(
        args: &[String],
        cargo_home: String,
        generated: Option<String>,
    ) -> Result<Unit, &'static str> {
        let (mut name, mut extra, mut out_dir, mut emit) = (None, "", None, "");
        let (mut externs, mut search, mut trees) = (Vec::new(), Vec::new(), Vec::new());
        let mut rest = args.iter().map(String::as_str);
        while let Some(arg) = rest.next() {
            let (flag, value) = match arg {
                "-C" | "-L" | "-o" | "--extern" | "--out-dir" | "--emit" | "--crate-name"
                | "--print" => (arg, rest.next().unwrap_or("")),
                _ if arg.starts_with("--") => arg.split_once('=').unwrap_or((arg, "")),
                _ if arg.starts_with('-') && arg.len() > 1 => arg.split_at(2),
                _ => ("", arg),
            };
            match (flag, value) {
                ("--crate-name", _) => name = Some(value),
                ("--out-dir", _) => out_dir = Some(Path::new(value)),
                ("--emit", _) => emit = value,
                ("--extern", _) => {
                    externs.extend(value.split_once('=').map(|(_, path)| PathBuf::from(path)))
                }
                ("-C", _) if value.starts_with("extra-filename=") => {
                    extra = &value["extra-filename=".len()..]
                }
                ("-C", _) if value.starts_with("incremental") => return Err("incremental"),
                ("-L", _) => match value.split_once('=') {
                    Some(("dependency", directory)) => search.push(PathBuf::from(directory)),
                    Some((_, directory)) => trees.push(directory.to_string()),
                    None => trees.push(value.to_string()),
                },
                ("--print" | "--version" | "-V", _) | ("-v", "V") => return Err("query"),
                ("-o", _) => return Err("explicit-output"),
                ("", "-") => return Err("stdin-input"),
                ("", _) if value.starts_with('@') => return Err("argument-file"),
                _ => {}
            }
        }
        let (name, out_dir) = (name.ok_or("no-crate-name")?, out_dir.ok_or("no-out-dir")?);
        let known = |kind| matches!(kind, "dep-info" | "metadata" | "link");
        if !emit.split(',').all(known) || !emit.split(',').any(|kind| kind == "dep-info") {
            return Err("emit");
        }
        let target = out_dir.ancestors().find(|directory| directory.join("CACHEDIR.TAG").exists());
        let roots =
            Roots { target: format!("{}/", target.ok_or("no-target-dir")?.display()), cargo_home };
        let escaped = |c: char| c.is_whitespace() || c.is_control() || c == '\\' || c == '"';
        if roots.target.contains(escaped) || roots.cargo_home.contains(escaped) {
            return Err("escaped-path");
        }
        if trees.iter().chain(&generated).any(|tree| !tree.starts_with(&roots.target)) {
            return Err("search-path");
        }
        let stem = format!("{name}{extra}");
        Ok(Unit { out_dir: out_dir.to_path_buf(), roots, stem, externs, search, trees, generated })
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
        field(self.roots.normalized(&env::current_dir()?.to_string_lossy()).as_bytes());
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            if arg == "--out-dir" {
                args.next();
            } else if !arg.starts_with("--out-dir=") {
                field(self.roots.normalized(arg).as_bytes());
            }
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
            for entry in fs::read_dir(directory)? {
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
            hash_tree(Path::new(tree), Path::new(tree), &mut field)?;
        }
        Ok(hasher.finalize().to_hex().to_string())
    }

    /// Rewrites dep-info through the placeholders, and lists what the crate read that the key does not cover.
    ///
    /// The list names each unkeyed environment variable and each file under OUT_DIR. Returns `None` where the crate
    /// read a file outside CARGO_HOME, OUT_DIR and the hashed trees, which no entry can cover.
    pub fn portable_dep_info(&self, dep_info: &str) -> Option<(String, String)> {
        let text = self.roots.normalized(dep_info);
        let directory =
            |path: &str| self.roots.normalized(&format!("{}/", path.trim_end_matches('/')));
        let mut covered = vec![CARGO_HOME.to_string(), directory(&self.out_dir.to_string_lossy())];
        covered.extend(self.trees.iter().map(|tree| directory(tree)));
        let generated = self.generated.as_deref().map(directory);
        let mut reads = Vec::new();
        for line in text.lines() {
            if let Some(variable) = line.strip_prefix("# env-dep:") {
                let name = variable.split('=').next().unwrap_or(variable);
                reads.extend((!is_keyed(name)).then(|| format!("env {name}")));
            } else if !line.starts_with('#') {
                for path in line.split_whitespace().map(|path| path.trim_end_matches(':')) {
                    if generated.as_ref().is_some_and(|generated| path.starts_with(generated)) {
                        reads.push(format!("file {path}"));
                    } else if !covered.iter().any(|prefix| path.starts_with(prefix)) {
                        return None;
                    }
                }
            }
        }
        reads.sort();
        reads.dedup();
        Some((text, reads.join("\n")))
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
