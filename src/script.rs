//! The stand-in that cargo runs in place of a third-party build script, which replays a stored run of the script.
//!
//! A run is keyed by the script binary, the compiler, the working directory and the variables that cargo gives a
//! script. Each variable that the stored run named in `rerun-if-env-changed` selects among the entries of one key. An
//! entry holds the OUT_DIR tree with the stdout and stderr of the script, and a file that contains the path of the
//! target directory is stored through the placeholders. CFLAGS and CXXFLAGS gain a prefix map of the target directory,
//! so the C objects of a run can be stored. A failure before the real script starts, or of a run with the prefix map,
//! leaves the script to run uncached in cargo's environment. A symlink of OUT_DIR is stored in the entry's `links`,
//! where [`Run::link`] allows it.
//!
//! A run that bakes the path of the target directory into a binary, as a vendored C library does, runs once more with
//! OUT_DIR at a path of the store. Markers in `out/` keep that decision for its key and its package, as AGENTS.md
//! describes.

use std::collections::BTreeMap;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::path::Component;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};
use std::process::{Command, Output, Stdio, exit};
use std::time::Instant;
use std::{env, fs, io};

use crate::store::{self, Files};
use crate::unit::{self, Roots, compiler, file_hash, hash_tree, remembered};

/// The extension of the real script, which `Unit::outputs` matches so that the next compile of the script removes it.
#[cfg(unix)]
const REAL: &str = "real";

/// The extension of the real script on Windows, which keeps `.exe` so that the script still looks like a program.
#[cfg(windows)]
const REAL: &str = "real.exe";

/// Moves the build script at `script` aside and duplicates a copy of this wrapper into its place.
///
/// Where a step fails, the real script is put back. The log then names the run as `passthrough no-stand-in`, because a
/// stand-in will not log it.
pub fn install(script: &Path, store: &Path, files: Files) -> io::Result<()> {
    let started = Instant::now();
    let stand_in = wrapper_copy(store);
    let real = script.with_extension(REAL);
    let installed = stand_in.and_then(|stand_in| {
        store::rename(script, &real)?;
        files.duplicate(&stand_in, script).or_else(|error| {
            let _ = fs::remove_file(script);
            store::rename(&real, script).and(Err(error))
        })
    });
    installed.inspect_err(|_| {
        let package = env::var("CARGO_PKG_NAME").unwrap_or_else(|_| "-".to_string());
        crate::log(store, &format!("{package}/build-script"), started, "passthrough no-stand-in");
    })
}

/// The copy of the running wrapper in `store` that a stand-in duplicates, because a clone needs one filesystem.
///
/// It is written once per wrapper binary.
pub fn wrapper_copy(store: &Path) -> io::Result<PathBuf> {
    let wrapper = env::current_exe()?;
    let copy = store.join("stand-in").join(file_hash(&wrapper)?.to_hex().as_str());
    if !copy.exists() {
        let staging = copy.with_extension(std::process::id().to_string());
        fs::create_dir_all(store.join("stand-in"))?;
        fs::copy(&wrapper, &staging).and_then(|_| store::rename(&staging, &copy)).inspect_err(
            |_| {
                let _ = fs::remove_file(&staging);
            },
        )?;
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
    // Windows reads a variable whatever the case of its name, so the key and the reads see each name in upper case.
    #[cfg(windows)]
    let environment = environment.map(|(name, value)| (name.to_ascii_uppercase(), value));
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

/// The variables that cargo documents for build scripts and that the key leaves out, each with the reason.
///
/// A test checks every other variable in the reference of cargo 1.100 against [`is_keyed`], and changes with the
/// supported toolchain.
#[cfg(test)]
#[cfg(unix)]
pub const UNKEYED_CARGO: &[(&str, &str)] = &[
    ("NUM_JOBS", "the parallelism of the build, which a script spends and does not write"),
    ("CARGO_MAKEFLAGS", "the descriptors of cargo's jobserver, which differ in every build"),
    ("RUSTC_WRAPPER", "a wrapper such as trucker, which does not change what rustc writes"),
    ("RUSTC_WORKSPACE_WRAPPER", "a wrapper that cargo sets only for workspace members"),
];

/// Whether the key of a run covers the environment variable `name`.
pub fn is_keyed(name: &str) -> bool {
    let cargo = "TARGET HOST PROFILE OPT_LEVEL DEBUG RUSTC RUSTDOC RUSTC_LINKER CARGO_MANIFEST_LINKS \
                 CARGO_ENCODED_RUSTFLAGS CARGO_TRIM_PATHS_SCOPE CARGO_TRIM_PATHS_REMAP";
    let prefixes = ["DEP_", "CARGO_CFG_", "CARGO_FEATURE_"];
    unit::is_keyed(name)
        || cargo.split_whitespace().any(|known| known == name)
        || prefixes.iter().any(|prefix| name.starts_with(prefix))
        || is_c_setting(name)
}

/// Whether `name` is a variable that the `cc` crate reads to choose or configure the C toolchain.
fn is_c_setting(name: &str) -> bool {
    let tools = "CC CXX AR RANLIB NVCC CFLAGS CXXFLAGS CPPFLAGS ARFLAGS RANLIBFLAGS CXXSTDLIB";
    let compiler = "CRATE_CC_NO_DEFAULTS CROSS_COMPILE DEVELOPER_DIR TOOLCHAINS CPATH C_INCLUDE_PATH \
                    CPLUS_INCLUDE_PATH COMPILER_PATH GCC_EXEC_PREFIX CCC_OVERRIDE_OPTIONS";
    // The variables of a Visual Studio prompt, which choose the compiler, headers and libraries of MSVC.
    #[cfg(windows)]
    let compiler = format!(
        "{compiler} INCLUDE LIB LIBPATH VCINSTALLDIR VCTOOLSINSTALLDIR VCTOOLSVERSION VSINSTALLDIR \
         WINDOWSSDKDIR WINDOWSSDKVERSION VSCMD_ARG_TGT_ARCH PLATFORM"
    );
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
    /// Whether CFLAGS and CXXFLAGS map the target directory to `target/`, so that the C objects of the run leave out
    /// the path of the worktree.
    prefix_map: bool,
}

impl Run {
    pub fn model(
        real: &Path,
        directory: PathBuf,
        mut environment: BTreeMap<String, String>,
        cargo_home: String,
    ) -> Result<Run, &'static str> {
        let out_dir = PathBuf::from(environment.get("OUT_DIR").ok_or("no-out-dir")?);
        let triple = environment.get("TARGET").map(String::as_str);
        let target = unit::target_directory(&out_dir, triple);
        let target = format!("{}{MAIN_SEPARATOR}", target.ok_or("no-target-dir")?.display());
        // The `cc` crate splits these variables at whitespace, so a target directory with a space is not mapped.
        let prefix_map = !target.contains(char::is_whitespace);
        // MSVC ignores the prefix map with a warning, so it is not given there.
        #[cfg(windows)]
        let prefix_map = prefix_map && !triple.is_some_and(|triple| triple.contains("msvc"));
        for name in ["CFLAGS", "CXXFLAGS"].into_iter().filter(|_| prefix_map) {
            let flags = environment.entry(name.to_string()).or_default();
            *flags = format!("{flags} -ffile-prefix-map={target}=target/").trim_start().to_string();
        }
        let roots = Roots { target, cargo_home, worktree: None };
        #[cfg(windows)]
        let keyed = environment.iter().filter(|(name, _)| is_keyed(name));
        #[cfg(windows)]
        if !crate::windows::is_keyable(&roots, keyed.map(|(_, value)| value.as_str())) {
            return Err("path-spelling");
        }
        Ok(Run { real: real.to_path_buf(), directory, environment, out_dir, roots, prefix_map })
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
        let (key, files) = (self.key(store).map_err(|_| "unreadable-input")?, Files::probe(store));
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
            if self.replay(&entry, files).is_ok() {
                store::touch(&scripts, &key, &entry);
                return Ok((if waited { "hit waited" } else { "hit" }.to_string(), 0));
            }
            // A replay that failed part way, such as one that lost a race with gc, leaves files that the script did not
            // write.
            self.empty_out_dir()?;
        }
        // A marker of the key says whether the run at a fixed OUT_DIR is stored, or must never be tried again. A marker
        // of the package sends every other key of it to that run first. A run there that cannot be stored falls back to
        // the run in cargo's environment, as one that baked nothing does.
        let marks = store.join("out").join(&key);
        let mark = |kind| drop(fs::write(marks.with_extension(kind), ""));
        let by_package = store.join("out").join(format!("{package}.fixed-package"));
        let mark_package = || drop((package != "-").then(|| fs::write(&by_package, "")));
        let never = marks.with_extension("never").exists();
        let fixed_first = !never
            && (marks.with_extension("fixed").exists() || package != "-" && by_package.exists());
        // A package that declares `links` usually builds a C library, so its script runs at the fixed OUT_DIR first too.
        // That run is kept only where it is stored, and OUT_DIR is otherwise emptied for the run in cargo's environment.
        let links = cfg!(unix)
            && !never
            && !fixed_first
            && self.environment.contains_key("CARGO_MANIFEST_LINKS");
        let mut first = (fixed_first || links).then(|| self.fixed(store, &key, files));
        let mut published = None;
        if let Some(Ok(output)) = &first {
            match self.publish(&scripts, &key, output, files) {
                Err(reason) if links => {
                    self.empty_out_dir()?;
                    first = Some(Err(reason));
                }
                stored => published = Some(stored),
            }
        }
        let (output, stored) = if let (Some(Ok(output)), Some(stored)) = (&first, published) {
            if stored.is_ok() {
                if links {
                    mark("fixed");
                }
                mark_package();
            }
            (output.clone(), stored)
        } else {
            let mut output = self.normal()?;
            let mut stored = if output.status.success() {
                self.publish(&scripts, &key, &output, files)
            } else {
                Err("script-failed")
            };
            if stored == Err("binary-target-path") && first.is_none() && !never {
                stored = self.fixed(store, &key, files).and_then(|fixed| {
                    let stored = self.publish(&scripts, &key, &fixed, files);
                    output = fixed;
                    stored
                });
                if stored.is_ok() {
                    mark("fixed");
                    mark_package();
                } else {
                    mark("never");
                }
            }
            // A refused first run marks its key, also for a `links` package, so that a run in cargo's environment that
            // cannot be stored does not run twice in every build.
            if let Some(Err(reason)) = first {
                mark("never");
                stored = stored.or(Err(reason));
            }
            (output, stored)
        };
        let code = output.status.code().unwrap_or(1);
        let _ = io::stdout().write_all(&output.stdout);
        let _ = io::stderr().write_all(&output.stderr);
        Ok((
            stored.map_or_else(|reason| format!("passthrough {reason}"), |()| "miss".to_string()),
            code,
        ))
    }

    /// Runs the script in cargo's environment with the prefix map.
    fn normal(&self) -> Result<Output, &'static str> {
        let output = Command::new(&self.real)
            .current_dir(&self.directory)
            .envs(&self.environment)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|_| "script-not-started")?;
        // A compiler that rejects the prefix map fails the script, which then runs again with cargo's environment.
        if !output.status.success() && self.prefix_map {
            self.empty_out_dir()?;
            return Err("script-failed");
        }
        Ok(output)
    }

    /// Runs the script with OUT_DIR in a slot of the store, and moves its outputs into OUT_DIR.
    ///
    /// OUT_DIR lies at `out/<key>/<n>/target/<path of OUT_DIR in the target>`, so a path that a C build bakes into a
    /// binary does not name a worktree. Other paths of the target directory go through the symlink `<n>.target`. The
    /// slot is then removed, so a baked path dangles as after `cargo clean`. Fails, and leaves OUT_DIR as the run in
    /// cargo's environment left it, where the run does one of these.
    /// - It fails or changes a file that the key covers.
    /// - It writes anything into the slot outside its OUT_DIR.
    /// - It leaves the path of the target directory in a binary.
    #[cfg(unix)]
    fn fixed(&self, store: &Path, key: &str, files: Files) -> Result<Output, &'static str> {
        let relative =
            self.out_dir.strip_prefix(&self.roots.target).map_err(|_| "no-target-dir")?;
        let pool = store.join("out").join(key);
        let failed = |_| "store-error";
        fs::create_dir_all(&pool).map_err(failed)?;
        let (number, _lock) = crate::session::choose(&pool, Vec::new()).ok_or("store-error")?;
        let (slot, link) = (pool.join(number.to_string()), pool.join(format!("{number}.target")));
        let clear = || drop((fs::remove_dir_all(&slot), fs::remove_file(&link)));
        clear();
        let out_dir = slot.join("target").join(relative);
        let roots = |target: &Path| Roots {
            target: format!("{}/", target.display()),
            cargo_home: self.roots.cargo_home.clone(),
            worktree: None,
        };
        let (mirror, through) = (roots(&slot.join("target")), roots(&link));
        // The slot's target directory is mapped too, so that a C object built from a source in OUT_DIR names it as in
        // cargo's environment.
        let map = self.prefix_map && !mirror.target.contains(char::is_whitespace);
        let map = map.then(|| format!(" -ffile-prefix-map={}=target/", mirror.target));
        let run = fs::create_dir_all(&out_dir)
            .and_then(|()| std::os::unix::fs::symlink(&self.roots.target, &link))
            .map_err(failed)
            .and_then(|()| {
                let environment = self.environment.iter().map(|(name, value)| {
                    let value = through.localized(&self.roots.normalized(value));
                    match &map {
                        Some(map) if name == "CFLAGS" || name == "CXXFLAGS" => (name, value + map),
                        _ => (name, value),
                    }
                });
                Command::new(&self.real)
                    .current_dir(&self.directory)
                    .envs(environment)
                    .env("OUT_DIR", &out_dir)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .output()
                    .map_err(|_| "fixed-run-failed")
            });
        let local =
            |text: &str| self.roots.localized(&mirror.normalized(&through.normalized(text)));
        let adopted = run.and_then(|mut output| {
            if !output.status.success() {
                return Err("fixed-run-failed");
            }
            let tree = store::tree(&slot.join("target")).map_err(failed)?;
            let mirrored = |(name, kind): &(PathBuf, fs::FileType)| {
                name.starts_with(relative) || (kind.is_dir() && relative.starts_with(name))
            };
            let slot_entries = fs::read_dir(&slot).map_err(failed)?.count();
            // A write that the slot does not show, through the symlink to a file that the key covers, changes the key.
            if slot_entries != 1
                || !tree.iter().all(mirrored)
                || self.key(store).ok() != Some(key.into())
            {
                return Err("script-escaped");
            }
            for text in [&mut output.stdout, &mut output.stderr] {
                if let Ok(mapped) = std::str::from_utf8(text).map(local) {
                    *text = mapped.into_bytes();
                }
            }
            self.adopt(&out_dir, files, &[&mirror.target, &through.target], &local)?;
            Ok(output)
        });
        clear();
        adopted
    }

    /// Fails, because the run at a fixed OUT_DIR needs a symlink, which Windows allows only with Developer Mode.
    #[cfg(windows)]
    fn fixed(&self, _: &Path, _: &str, _: Files) -> Result<Output, &'static str> {
        Err("windows-fixed")
    }

    /// Replaces OUT_DIR with a copy of `from`, and writes a text file that contains one of `roots` through `local`.
    ///
    /// The copy is staged beside OUT_DIR and renamed into place, so a failure leaves OUT_DIR as it was.
    #[cfg(unix)]
    fn adopt(
        &self,
        from: &Path,
        files: Files,
        roots: &[&str],
        local: &dyn Fn(&str) -> String,
    ) -> Result<(), &'static str> {
        let (staging, aside) =
            (self.out_dir.with_extension("fixed"), self.out_dir.with_extension("normal"));
        let _ = (fs::remove_dir_all(&staging), fs::remove_dir_all(&aside));
        let copied = store::tree(from).map_err(|_| "store-error").and_then(|tree| {
            fs::create_dir_all(&staging).map_err(|_| "store-error")?;
            let mut links = String::new();
            for (name, kind) in tree {
                let (source, destination) = (from.join(&name), staging.join(&name));
                let text = || fs::read_to_string(&source).ok();
                let written = if kind.is_symlink() {
                    let (kind, target) = self.link(from, &name)?;
                    links.push_str(&line(kind, &target, &name, local)?);
                    Ok(())
                } else if kind.is_dir() {
                    fs::create_dir_all(&destination)
                } else if let Some(text) =
                    store::any_contains(&[&source], roots).then(text).flatten()
                {
                    fs::write(&destination, local(&text))
                } else if store::any_contains(&[&source], &[&self.roots.target]) && text().is_none()
                {
                    return Err("binary-target-path");
                } else {
                    files.duplicate(&source, &destination)
                };
                written.map_err(|_| "store-error")?;
            }
            self.make_links(&links, &staging, &str::to_string).map_err(|_| "store-error")?;
            fs::rename(&self.out_dir, &aside).map_err(|_| "unmovable-output")
        });
        if copied.is_ok() && fs::rename(&staging, &self.out_dir).is_err() {
            let _ = fs::rename(&aside, &self.out_dir);
            let _ = fs::remove_dir_all(&staging);
            return Err("unmovable-output");
        }
        let _ = (fs::remove_dir_all(&staging), fs::remove_dir_all(&aside));
        copied
    }

    fn empty_out_dir(&self) -> Result<(), &'static str> {
        fs::remove_dir_all(&self.out_dir)
            .and_then(|()| fs::create_dir_all(&self.out_dir))
            .map_err(|_| "unremovable-output")
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
        #[cfg(windows)]
        field(crate::windows::toolchains().as_bytes());
        let rustc = self.environment.get("RUSTC").ok_or(io::ErrorKind::NotFound)?;
        field(&compiler(rustc, store)?);
        field(self.roots.normalized(&self.directory.to_string_lossy()).as_bytes());
        let compilers = self
            .environment
            .iter()
            .filter(|(name, _)| is_tool_setting(name, "CC") || is_tool_setting(name, "CXX"));
        #[cfg(unix)]
        let defaults = ["cc", "c++"];
        // The `cc` crate finds MSVC through Visual Studio rather than PATH, which `windows::toolchains` covers.
        #[cfg(windows)]
        let defaults = ["cc", "c++", "cl", "gcc", "clang"];
        for setting in compilers.map(|(_, value)| value.as_str()).chain(defaults) {
            field(&self.c_compiler(setting, store));
        }
        for (name, value) in self.environment.iter().filter(|(name, _)| is_keyed(name)) {
            field(name.as_bytes());
            field(self.roots.normalized(value).as_bytes());
            let path = Path::new(value);
            if value.starts_with(&self.roots.target) && path != self.out_dir && path.is_dir() {
                hash_tree(&self.roots, path, &store.join("hashes"), &mut field)?;
            } else if value.starts_with(&self.roots.target) && path.is_file() {
                field(file_hash(path)?.as_bytes());
            }
        }
        Ok(hasher.finalize().to_hex().to_string())
    }

    /// The real path and the `--version` output of the compiler that `setting` names, such as `ccache clang -m32`.
    ///
    /// The output is remembered in `store` for the file of the compiler. On macOS a compiler in `/usr/bin` is a shim
    /// that runs the compiler of the active developer directory, and the file of that compiler is covered too.
    fn c_compiler(&self, setting: &str, store: &Path) -> Vec<u8> {
        let is_compiler =
            |word: &&str| !["ccache", "distcc"].iter().any(|tool| word.ends_with(tool));
        let program = setting.split_whitespace().find(is_compiler).unwrap_or_default();
        let path = self.environment.get("PATH");
        let mut found =
            env::split_paths(path.map_or("", String::as_str)).map(|path| path.join(program));
        #[cfg(unix)]
        let found = if program.contains('/') {
            Some(self.directory.join(program))
        } else {
            found.find(|path| path.is_file())
        };
        #[cfg(windows)]
        let found = if program.contains(['/', '\\']) {
            Some(self.directory.join(program))
        } else {
            let executable = |path: PathBuf| [path.with_extension("exe"), path];
            found.find_map(|path| executable(path).into_iter().find(|path| path.is_file()))
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
            #[cfg(unix)]
            let metadata = fs::metadata(file).map(|file| (file.ino(), file.len(), file.mtime()));
            #[cfg(windows)]
            let metadata = fs::metadata(file).map(|metadata| {
                (
                    crate::windows::information(file, true)
                        .map(|(volume, index, ..)| (volume, index)),
                    metadata.len(),
                    metadata.modified().ok(),
                )
            });
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
    /// Fails where `stdout` names a file in `rerun-if-changed` outside CARGO_HOME and OUT_DIR, because an entry does
    /// not cover it.
    fn reads(&self, stdout: &str) -> Result<String, &'static str> {
        let mut reads = Vec::new();
        for line in stdout.lines() {
            let instruction = line.strip_prefix("cargo::").or_else(|| line.strip_prefix("cargo:"));
            let Some((kind, value)) =
                instruction.and_then(|instruction| instruction.split_once('='))
            else {
                continue;
            };
            #[cfg(windows)]
            let value = &value.to_ascii_uppercase();
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

    /// Stores OUT_DIR and the output of the script under `key` in `scripts`, or fails with the reason why it cannot.
    fn publish(
        &self,
        scripts: &Path,
        key: &str,
        output: &Output,
        files: Files,
    ) -> Result<(), &'static str> {
        let stdout = std::str::from_utf8(&output.stdout).map_err(|_| "binary-output")?;
        let reads = self.reads(stdout)?;
        let staging = scripts.join(format!("{key}.{}", std::process::id()));
        let write = |name, text: &str| fs::write(staging.join(name), self.roots.normalized(text));
        // A path in another spelling than that of the target directory is not replaced, and would name it in another
        // worktree, as a link search path that names its OUT_DIR would.
        #[cfg(windows)]
        for text in [stdout, &String::from_utf8_lossy(&output.stderr)] {
            if crate::windows::contains(&self.roots.normalized(text), &[&self.roots.target]) {
                return Err("unnormalized-path");
            }
        }
        let stored =
            self.stage(files, &staging.join("out"), &staging.join("text")).and_then(|links| {
                if !links.is_empty() {
                    fs::write(staging.join("links"), links).map_err(|_| "store-error")?;
                }
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

    /// Duplicates each file of OUT_DIR into `cloned`, except a file that contains the target directory's path, and
    /// returns the lines of `links` for its symlinks.
    ///
    /// Such a file is written into `rewritten` through the placeholders. Fails where it is not UTF-8, or where OUT_DIR
    /// contains a symlink that [`Run::link`] refuses.
    fn stage(&self, files: Files, cloned: &Path, rewritten: &Path) -> Result<String, &'static str> {
        let failed = |_| "store-error";
        fs::create_dir_all(cloned).and_then(|()| fs::create_dir_all(rewritten)).map_err(failed)?;
        let mut links = String::new();
        for (name, kind) in store::tree(&self.out_dir).map_err(failed)? {
            let path = self.out_dir.join(&name);
            if kind.is_symlink() {
                let (kind, target) = self.link(&self.out_dir, &name)?;
                links.push_str(&line(kind, &target, &name, &|path| self.roots.normalized(path))?);
            } else if kind.is_dir() {
                fs::create_dir_all(cloned.join(&name))
                    .and_then(|()| fs::create_dir_all(rewritten.join(&name)))
                    .map_err(failed)?;
            } else if store::any_contains(&[&path], &[&self.roots.target]) {
                let text = fs::read_to_string(&path).map_err(|_| "binary-target-path")?;
                let text = self.roots.normalized(&text);
                #[cfg(windows)]
                if crate::windows::contains(&text, &[&self.roots.target]) {
                    return Err("unnormalized-path");
                }
                fs::write(rewritten.join(&name), text).map_err(failed)?;
            } else {
                files.duplicate(&path, &cloned.join(&name)).map_err(|_| "clone-failed")?;
            }
        }
        Ok(links)
    }

    /// The kind and target of the symlink at `name` in `out_dir`, or why an entry cannot store it.
    ///
    /// A relative target without `..` stays under the directory of the link and is kept as written, as `inside`. Any
    /// other target is kept as the absolute path that it names, as `absolute` or `relative`, where both of these hold.
    /// - The path lies in `out_dir`, or in a source of the registry or of a git checkout, which never changes.
    /// - The path resolves where it reads, so no `..` follows a symlink.
    #[cfg(unix)]
    fn link(&self, out_dir: &Path, name: &Path) -> Result<(&'static str, String), &'static str> {
        let link = out_dir.join(name);
        let target = fs::read_link(&link).map_err(|_| "symlink")?;
        let text = target.to_str().filter(|text| !text.contains(['\t', '\n'])).ok_or("symlink")?;
        let normal = |part: Component| matches!(part, Component::Normal(_) | Component::CurDir);
        if target.components().all(normal) {
            return Ok(("inside", text.to_string()));
        }
        let path = unit::lexical(&link.parent().unwrap_or(out_dir).join(&target));
        let rooted =
            path.starts_with(out_dir) || unit::is_third_party(&path, &self.roots.cargo_home);
        let real = path.canonicalize().ok();
        if !rooted || real.is_none() || real != link.canonicalize().ok() {
            return Err("symlink");
        }
        let kind = if target.is_relative() { "relative" } else { "absolute" };
        Ok((kind, path.to_str().ok_or("symlink")?.to_string()))
    }

    /// Fails, because a symlink needs Developer Mode on Windows.
    #[cfg(windows)]
    fn link(&self, _: &Path, _: &Path) -> Result<(&'static str, String), &'static str> {
        Err("symlink")
    }

    /// Writes the symlinks that the lines of `links` describe into `at`, and returns those that name an absolute path.
    ///
    /// A target is written as it reads from the place of its link in OUT_DIR, into which `at` may yet be renamed. A
    /// `relative` link gets the path that [`relative`] finds, or the absolute path where it finds none.
    #[cfg(unix)]
    fn make_links(
        &self,
        links: &str,
        at: &Path,
        local: &dyn Fn(&str) -> String,
    ) -> io::Result<Vec<PathBuf>> {
        let mut rooted = Vec::new();
        for line in links.lines() {
            let mut fields = line.splitn(3, '\t');
            let (Some(kind), Some(target), Some(name)) =
                (fields.next(), fields.next(), fields.next())
            else {
                return Err(io::ErrorKind::InvalidData.into());
            };
            let (link, place) = (at.join(name), self.out_dir.join(name));
            let target = match kind {
                "inside" => PathBuf::from(target),
                "absolute" | "relative" => {
                    rooted.push(link.clone());
                    let path = PathBuf::from(local(target));
                    let parent = place.parent().filter(|_| kind == "relative");
                    let relative = parent.and_then(|from| relative(from, &path, &self.out_dir));
                    relative.unwrap_or(path)
                }
                _ => return Err(io::ErrorKind::InvalidData.into()),
            };
            std::os::unix::fs::symlink(target, link)?;
        }
        Ok(rooted)
    }

    /// Fails where `links` describes a symlink, which an entry on Windows never holds.
    #[cfg(windows)]
    fn make_links(
        &self,
        links: &str,
        _: &Path,
        _: &dyn Fn(&str) -> String,
    ) -> io::Result<Vec<PathBuf>> {
        if links.is_empty() { Ok(Vec::new()) } else { Err(io::ErrorKind::Unsupported.into()) }
    }

    /// Empties OUT_DIR, restores the stored tree of `entry` into it, then replays the script's output for cargo.
    fn replay(&self, entry: &Path, files: Files) -> io::Result<()> {
        let stdout = self.roots.localized(&fs::read_to_string(entry.join("stdout"))?);
        let stderr = self.roots.localized(&fs::read_to_string(entry.join("stderr"))?);
        fs::remove_dir_all(&self.out_dir)?;
        fs::create_dir_all(&self.out_dir)?;
        restore(files, &entry.join("out"), &self.out_dir, None)?;
        restore(files, &entry.join("text"), &self.out_dir, Some(&self.roots))?;
        let links = match fs::read_to_string(entry.join("links")) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
            links => links?,
        };
        // A link that dangles names a path that the stored run found and that is gone, so the script runs.
        let rooted = self.make_links(&links, &self.out_dir, &|path| self.roots.localized(path))?;
        if rooted.iter().any(|link| fs::metadata(link).is_err()) {
            return Err(io::ErrorKind::NotFound.into());
        }
        io::stdout().write_all(stdout.as_bytes())?;
        io::stderr().write_all(stderr.as_bytes())
    }
}

/// The line of `links` for the symlink at `name`, whose `target` is written through `map` unless it is `inside`.
fn line(
    kind: &str,
    target: &str,
    name: &Path,
    map: &dyn Fn(&str) -> String,
) -> Result<String, &'static str> {
    let name = name.to_str().filter(|name| !name.contains(['\t', '\n'])).ok_or("symlink")?;
    let target = if kind == "inside" { target.to_string() } else { map(target) };
    Ok(format!("{kind}\t{target}\t{name}\n"))
}

/// The relative path from the directory `from` to `to`, where `..` from `from` reaches their common ancestor.
///
/// The directories under `out_dir` are real, so only those between that ancestor and `out_dir` can be symlinks. It is
/// `None` where one is, as `cxx-build` then writes an absolute link too.
#[cfg(unix)]
fn relative(from: &Path, to: &Path, out_dir: &Path) -> Option<PathBuf> {
    let common = from.ancestors().find(|ancestor| to.starts_with(ancestor))?;
    if let Ok(down) = out_dir.strip_prefix(common) {
        (common.canonicalize().ok()?.join(down) == out_dir.canonicalize().ok()?).then_some(())?;
    }
    let up = from.strip_prefix(common).ok()?.components().map(|_| Component::ParentDir);
    Some(up.collect::<PathBuf>().join(to.strip_prefix(common).ok()?))
}

/// Duplicates each file of `from` into `to`, or writes it through `roots` where it was stored through the placeholders.
fn restore(files: Files, from: &Path, to: &Path, roots: Option<&Roots>) -> io::Result<()> {
    for (name, kind) in store::tree(from)? {
        let (source, destination) = (from.join(&name), to.join(&name));
        if kind.is_dir() {
            fs::create_dir_all(&destination)?;
        } else if let Some(roots) = roots {
            fs::write(&destination, roots.localized(&fs::read_to_string(&source)?))?;
        } else {
            files.duplicate(&source, &destination)?;
        }
    }
    Ok(())
}
