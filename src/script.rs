//! The stand-in that cargo runs in place of a third-party build script, which replays a stored run of the script.
//!
//! A run is keyed by the script binary, the compiler, the working directory and the variables that cargo gives a
//! script. Each variable that the stored run named in `rerun-if-env-changed` selects among the entries of one key. An
//! entry is the OUT_DIR tree with the script's stdout and stderr, and its files that contain the target directory's
//! path are stored through the placeholders. Any failure before the real script starts leaves it to run uncached.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio, exit};
use std::time::Instant;
use std::{env, fs, io};

use crate::store::{self, clone};
use crate::unit::{self, Roots, compiler, file_hash, hash_tree, remembered};

/// The extension of the real script, which `Unit::outputs` matches, so the next compile of the script removes it.
const REAL: &str = "real";

/// Moves the build script at `script` aside and clones a copy of this wrapper into its place.
///
/// Where any step fails, the real script is put back.
pub fn install(script: &Path, store: &Path) -> io::Result<()> {
    let stand_in = wrapper_copy(store)?;
    let real = script.with_extension(REAL);
    fs::rename(script, &real)?;
    clone(&stand_in, script).or_else(|error| {
        let _ = fs::remove_file(script);
        fs::rename(&real, script).and(Err(error))
    })
}

/// A copy of the running wrapper in `store`, written once per wrapper binary, because a clone needs one filesystem.
pub fn wrapper_copy(store: &Path) -> io::Result<PathBuf> {
    let wrapper = env::current_exe()?;
    let copy = store.join("stand-in").join(file_hash(&wrapper)?.to_hex().as_str());
    if !copy.exists() {
        let staging = copy.with_extension(std::process::id().to_string());
        fs::create_dir_all(store.join("stand-in"))?;
        fs::copy(&wrapper, &staging).and_then(|_| fs::rename(&staging, &copy))?;
    }
    Ok(copy)
}

/// Runs as the stand-in of a build script and exits, where cargo started this process as one.
pub fn dispatch() {
    let script = PathBuf::from(env::args_os().next().unwrap_or_default());
    let name = script.file_name().unwrap_or_default().to_string_lossy();
    let real = script.with_extension(REAL);
    if !name.starts_with("build_script_") || !real.is_file() {
        return;
    }
    let started = Instant::now();
    let environment = env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)));
    let run = env::current_dir().map_err(|_| "no-working-directory").and_then(|directory| {
        Run::model(&real, directory, environment.collect(), unit::cargo_home())
    });
    let store = crate::store_directory();
    let (outcome, code) = replay_or_run(&real, run, &store);
    let package = env::var("CARGO_PKG_NAME").unwrap_or_else(|_| "-".to_string());
    crate::log(&store, &format!("{package}/build-script"), started, &outcome);
    if outcome == "miss" {
        // This process runs as the build script, so gc starts from the copy of the wrapper.
        let _ = wrapper_copy(&store).map(|wrapper| crate::gc::trigger(&store, &wrapper));
    }
    exit(code)
}

/// Replays or records the run of `real`, or runs `real` uncached where that fails before it starts.
pub fn replay_or_run(real: &Path, run: Result<Run, &'static str>, store: &Path) -> (String, i32) {
    run.and_then(|run| run.cached(store)).unwrap_or_else(|reason| {
        let status = Command::new(real).status();
        let code = status
            .map_or_else(|error| crate::report(real, error), |status| status.code().unwrap_or(1));
        (format!("passthrough {reason}"), code)
    })
}

/// Whether the key of a run covers the environment variable `name`.
fn is_keyed(name: &str) -> bool {
    let cargo =
        ["TARGET", "HOST", "PROFILE", "OPT_LEVEL", "DEBUG", "RUSTC", "RUSTDOC", "RUSTC_LINKER"];
    unit::is_keyed(name) || cargo.contains(&name) || name.starts_with("DEP_") || is_c_setting(name)
}

/// Whether `name` is a variable that the `cc` crate reads to choose or configure the C toolchain.
fn is_c_setting(name: &str) -> bool {
    let tools = "CC CXX AR RANLIB NVCC CFLAGS CXXFLAGS CPPFLAGS ARFLAGS RANLIBFLAGS CXXSTDLIB";
    let compiler = "CRATE_CC_NO_DEFAULTS CROSS_COMPILE DEVELOPER_DIR TOOLCHAINS CPATH C_INCLUDE_PATH \
                    CPLUS_INCLUDE_PATH COMPILER_PATH GCC_EXEC_PREFIX CCC_OVERRIDE_OPTIONS";
    tools.split(' ').any(|tool| is_tool_setting(name, tool))
        || compiler.split_whitespace().any(|known| known == name)
        || name.ends_with("_DEPLOYMENT_TARGET")
}

/// Whether `name` is the variable of `tool`, with or without a `HOST_` or `TARGET_` prefix or a target as its suffix.
fn is_tool_setting(name: &str, tool: &str) -> bool {
    let name = name.strip_prefix("HOST_").or_else(|| name.strip_prefix("TARGET_")).unwrap_or(name);
    name.strip_prefix(tool).is_some_and(|rest| rest.is_empty() || rest.starts_with('_'))
}

/// One run of a third-party build script, with the environment that cargo gave it.
pub struct Run {
    real: PathBuf,
    directory: PathBuf,
    environment: BTreeMap<String, String>,
    out_dir: PathBuf,
    roots: Roots,
}

impl Run {
    pub fn model(
        real: &Path,
        directory: PathBuf,
        environment: BTreeMap<String, String>,
        cargo_home: String,
    ) -> Result<Run, &'static str> {
        let out_dir = PathBuf::from(environment.get("OUT_DIR").ok_or("no-out-dir")?);
        let triple = environment.get("TARGET").map(String::as_str);
        let target = unit::target_directory(&out_dir, triple);
        let target = format!("{}/", target.ok_or("no-target-dir")?.display());
        let roots = Roots { target, cargo_home, worktree: None };
        Ok(Run { real: real.to_path_buf(), directory, environment, out_dir, roots })
    }

    /// Restores the outputs of this run from `store`, or runs the script and stores them, and returns the outcome.
    ///
    /// Fails with the reason, before the script has started, where the store cannot be used.
    fn cached(&self, store: &Path) -> Result<(String, i32), &'static str> {
        let scripts = store.join("scripts");
        fs::create_dir_all(&scripts).map_err(|_| "store-error")?;
        if !store::same_filesystem(&scripts, &self.out_dir) {
            return Err("other-filesystem");
        }
        let key = self.key(store).map_err(|_| "unreadable-input")?;
        let current_entry = || {
            let reads = fs::read_to_string(store::reads_of(&scripts, &key)).unwrap_or_default();
            scripts.join(self.variant(&key, &reads))
        };
        let mut entry = current_entry();
        let waited = !entry.is_dir();
        let package = self.environment.get("CARGO_PKG_NAME").map_or("-", String::as_str);
        let name = format!("{package}/build-script");
        let _lock = waited.then(|| store::lock(store, &key, &name)).flatten();
        if waited {
            entry = current_entry();
        }
        if entry.is_dir() {
            if self.replay(&entry).is_ok() {
                store::touch(&scripts, &key, &entry);
                return Ok((if waited { "hit waited" } else { "hit" }.to_string(), 0));
            }
            // A replay that failed part way, as one does that loses a race with gc, leaves files that the script did
            // not write.
            fs::remove_dir_all(&self.out_dir)
                .and_then(|()| fs::create_dir_all(&self.out_dir))
                .map_err(|_| "unremovable-output")?;
        }
        let output = Command::new(&self.real)
            .current_dir(&self.directory)
            .envs(&self.environment)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|_| "script-not-started")?;
        let _ = io::stdout().write_all(&output.stdout);
        let _ = io::stderr().write_all(&output.stderr);
        let code = output.status.code().unwrap_or(1);
        let stored =
            if code == 0 { self.publish(&scripts, &key, &output) } else { Err("script-failed") };
        Ok((
            stored.map_or_else(|reason| format!("passthrough {reason}"), |()| "miss".to_string()),
            code,
        ))
    }

    /// The key of this run, as hex.
    ///
    /// A keyed variable that names a file or directory under the target directory, such as a dependency's include
    /// directory, also contributes its contents.
    pub fn key(&self, store: &Path) -> io::Result<String> {
        let mut hasher = blake3::Hasher::new();
        let mut field = |bytes: &[u8]| {
            hasher.update(&(bytes.len() as u64).to_le_bytes()).update(bytes);
        };
        field(file_hash(&self.real)?.as_bytes());
        let rustc = self.environment.get("RUSTC").ok_or(io::ErrorKind::NotFound)?;
        field(&compiler(rustc, store)?);
        field(self.roots.normalized(&self.directory.to_string_lossy()).as_bytes());
        let compilers = self
            .environment
            .iter()
            .filter(|(name, _)| is_tool_setting(name, "CC") || is_tool_setting(name, "CXX"));
        for setting in compilers.map(|(_, value)| value.as_str()).chain(["cc", "c++"]) {
            field(&self.c_compiler(setting, store));
        }
        for (name, value) in self.environment.iter().filter(|(name, _)| is_keyed(name)) {
            field(name.as_bytes());
            field(self.roots.normalized(value).as_bytes());
            let path = Path::new(value);
            if value.starts_with(&self.roots.target) && path != self.out_dir && path.is_dir() {
                hash_tree(path, path, &mut field)?;
            } else if value.starts_with(&self.roots.target) && path.is_file() {
                field(file_hash(path)?.as_bytes());
            }
        }
        Ok(hasher.finalize().to_hex().to_string())
    }

    /// The real path and the `--version` output of the compiler that `setting` names, such as `ccache clang -m32`.
    ///
    /// The output is remembered in `store` for the file of the compiler. On macOS a compiler in `/usr/bin` is a shim
    /// that runs the compiler of the active developer directory, whose file is then covered too.
    fn c_compiler(&self, setting: &str, store: &Path) -> Vec<u8> {
        let is_compiler =
            |word: &&str| !["ccache", "distcc"].iter().any(|tool| word.ends_with(tool));
        let program = setting.split_whitespace().find(is_compiler).unwrap_or_default();
        let path = self.environment.get("PATH");
        let mut found =
            env::split_paths(path.map_or("", String::as_str)).map(|path| path.join(program));
        let found = if program.contains('/') {
            Some(self.directory.join(program))
        } else {
            found.find(|path| path.is_file())
        };
        let Some(program) = found.and_then(|found| found.canonicalize().ok()) else {
            return b"none".to_vec();
        };
        let mut files = vec![program.clone()];
        if cfg!(target_os = "macos") && program.starts_with("/usr/bin") {
            let developer = self.environment.get("DEVELOPER_DIR").map(PathBuf::from);
            let developer = developer.or_else(|| fs::read_link("/var/db/xcode_select_link").ok());
            let developer =
                developer.unwrap_or_else(|| "/Library/Developer/CommandLineTools".into());
            let name = program.file_name().unwrap_or_default();
            files.push(developer.join("usr/bin").join(name));
            files.push(developer.join("Toolchains/XcodeDefault.xctoolchain/usr/bin").join(name));
        }
        let identity = String::from_iter(files.iter().map(|file| {
            let metadata = fs::metadata(file).map(|file| (file.ino(), file.len(), file.mtime()));
            format!("{} {metadata:?}\n", file.display())
        }));
        let version =
            || Command::new(&program).arg("--version").output().map(|output| output.stdout);
        let version = remembered(&store.join("cc"), &identity, version).unwrap_or_default();
        [identity.into_bytes(), version].concat()
    }

    /// The name of the entry of `key` for the current value of each variable in `reads`.
    fn variant(&self, key: &str, reads: &str) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(key.as_bytes());
        for read in reads.lines() {
            let value = read.strip_prefix("env ").and_then(|name| self.environment.get(name));
            let value = value.map(|value| self.roots.normalized(value));
            hasher.update(format!("\n{read}={value:?}").as_bytes());
        }
        if reads.is_empty() { key.to_string() } else { hasher.finalize().to_hex().to_string() }
    }

    /// The unkeyed variables that the script's `stdout` names in `rerun-if-env-changed`.
    ///
    /// Fails where `stdout` names a file in `rerun-if-changed` outside CARGO_HOME and OUT_DIR, which no entry covers.
    fn reads(&self, stdout: &str) -> Result<String, &'static str> {
        let mut reads = Vec::new();
        for line in stdout.lines() {
            let instruction = line.strip_prefix("cargo::").or_else(|| line.strip_prefix("cargo:"));
            let Some((kind, value)) =
                instruction.and_then(|instruction| instruction.split_once('='))
            else {
                continue;
            };
            if kind == "rerun-if-env-changed" && !is_keyed(value) {
                reads.push(format!("env {value}"));
            } else if kind == "rerun-if-changed" {
                let path = unit::lexical(&self.directory.join(value));
                if !path.starts_with(&self.roots.cargo_home) && !path.starts_with(&self.out_dir) {
                    return Err("unkeyed-input");
                }
            }
        }
        reads.sort();
        reads.dedup();
        Ok(reads.join("\n"))
    }

    /// Stores OUT_DIR and the script's output under `key` in `scripts`, or names the reason that it cannot.
    fn publish(&self, scripts: &Path, key: &str, output: &Output) -> Result<(), &'static str> {
        let stdout = std::str::from_utf8(&output.stdout).map_err(|_| "binary-output")?;
        let reads = self.reads(stdout)?;
        let staging = scripts.join(format!("{key}.{}", std::process::id()));
        let write = |name, text: &str| fs::write(staging.join(name), self.roots.normalized(text));
        let stored =
            self.stage(&self.out_dir, &staging.join("out"), &staging.join("text")).and_then(|()| {
                write("stdout", stdout).map_err(|_| "store-error")?;
                write("stderr", &String::from_utf8_lossy(&output.stderr))
                    .map_err(|_| "store-error")?;
                store::commit(
                    scripts,
                    key,
                    &reads,
                    &staging,
                    &scripts.join(self.variant(key, &reads)),
                )
            });
        let _ = fs::remove_dir_all(&staging);
        stored
    }

    /// Clones each file of `from` into `cloned`, except a file that contains the target directory's path.
    ///
    /// Such a file is written into `rewritten` through the placeholders. Fails where it is not UTF-8, or where `from`
    /// contains a symlink.
    fn stage(&self, from: &Path, cloned: &Path, rewritten: &Path) -> Result<(), &'static str> {
        let failed = |_| "store-error";
        fs::create_dir_all(cloned).and_then(|()| fs::create_dir_all(rewritten)).map_err(failed)?;
        for entry in fs::read_dir(from).map_err(failed)? {
            let entry = entry.map_err(failed)?;
            let (path, name, kind) =
                (entry.path(), entry.file_name(), entry.file_type().map_err(failed)?);
            let target = self.roots.target.as_bytes();
            let contains_target =
                |bytes: Vec<u8>| bytes.windows(target.len()).any(|bytes| bytes == target);
            if kind.is_symlink() {
                return Err("symlink");
            } else if kind.is_dir() {
                self.stage(&path, &cloned.join(&name), &rewritten.join(&name))?;
            } else if contains_target(fs::read(&path).map_err(failed)?) {
                let text = fs::read_to_string(&path).map_err(|_| "binary-target-path")?;
                fs::write(rewritten.join(&name), self.roots.normalized(&text)).map_err(failed)?;
            } else {
                clone(&path, &cloned.join(&name)).map_err(|_| "clone-failed")?;
            }
        }
        Ok(())
    }

    /// Empties OUT_DIR, restores the stored tree of `entry` into it, then replays the script's output for cargo.
    fn replay(&self, entry: &Path) -> io::Result<()> {
        let stdout = self.roots.localized(&fs::read_to_string(entry.join("stdout"))?);
        let stderr = self.roots.localized(&fs::read_to_string(entry.join("stderr"))?);
        fs::remove_dir_all(&self.out_dir)?;
        fs::create_dir_all(&self.out_dir)?;
        restore(&entry.join("out"), &self.out_dir, None)?;
        restore(&entry.join("text"), &self.out_dir, Some(&self.roots))?;
        io::stdout().write_all(stdout.as_bytes())?;
        io::stderr().write_all(stderr.as_bytes())
    }
}

/// Clones each file of `from` into `to`, or writes it through `roots` where it was stored through the placeholders.
fn restore(from: &Path, to: &Path, roots: Option<&Roots>) -> io::Result<()> {
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let (source, destination) = (entry.path(), to.join(entry.file_name()));
        if entry.file_type()?.is_dir() {
            fs::create_dir_all(&destination)?;
            restore(&source, &destination, roots)?;
        } else if let Some(roots) = roots {
            fs::write(&destination, roots.localized(&fs::read_to_string(&source)?))?;
        } else {
            clone(&source, &destination)?;
        }
    }
    Ok(())
}
