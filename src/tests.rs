use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};
use std::{env, fs, thread};

use crate::script::{self, Run, replay_or_run};
use crate::store::Files;
use crate::unit::{self, CARGO_HOME, Roots, TARGET, Unit, WORKTREE};
use crate::{cached, gc, is_program, lazy, note, session, stats, store};

/// A directory of one test, with a stand-in rustc and a CARGO_HOME, on the filesystem of the temporary directory.
struct Fixture {
    root: PathBuf,
    rustc: String,
    cargo_home: String,
}

/// A fixture whose stand-in rustc writes the outputs of a crate `foo`, its arguments and a line of stderr.
///
/// The dep-info of `foo` names `$extra`.
fn fixture(name: &str, extra: &str) -> Fixture {
    let root = env::temp_dir().join(format!("trucker-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let cargo_home = format!("{}/cargo/", root.display());
    fs::create_dir_all(root.join("store/entries")).unwrap();
    let script = format!(
        r#"#!/bin/sh
[ "$1" = -vV ] && {{ printf 'stand-in\nrelease: 1.100.0\n'; exit 0; }}
args="$*"
while [ $# -gt 0 ]; do case "$1" in --out-dir) out=$2;; incremental=*) inc=${{1#incremental=}};; *.rs) src=$1;; esac; shift; done
echo ran >> "$out/runs"
if [ -n "$inc" ]; then
    old=$(ls "$inc/foo-1x" 2> /dev/null | grep -v lock)
    [ -e "$inc/foo-1x/${{old%-*}}.lock" ] || old=
    echo "${{old:-none}} $(pwd -P)" >> "$out/sessions"
    [ -n "$old" ] && ls -i "$inc/foo-1x/$old" > "$out/inodes"
    [ -e "$inc/foo-1x/$old/poison" ] && exit 101
    rm -rf "$inc/foo-1x" && mkdir -p "$inc/foo-1x/s-$$-r-svh" && touch "$inc/foo-1x/s-$$-r.lock"
    printf graph > "$inc/foo-1x/s-$$-r-svh/dep-graph.bin"
    grep -qs poison "$src" && touch "$inc/foo-1x/s-$$-r-svh/poison"
    touch -t 202001010000 "$inc/foo-1x/s-$$-r-svh/"*
fi
echo "$args" > "$out/args"
printf rmeta > "$out/libfoo-abc.rmeta"
printf rlib > "$out/libfoo-abc.rlib"
deps="{cargo_home}registry/src/index/foo-1.0.0/src/lib.rs {extra}"
printf '%s/libfoo-abc.rmeta: %s\n# env-dep:CARGO_PKG_NAME=foo\n' "$out" "$deps" > "$out/foo-abc.d"
printf '{{"artifact":"%s/libfoo-abc.rmeta"}}\n' "$out" >&2
"#
    );
    let rustc = root.join("rustc");
    executable(&rustc, &script);
    Fixture { root, rustc: rustc.display().to_string(), cargo_home }
}

impl Fixture {
    /// A worktree's target directory, with the output directory of `foo` and the metadata of its dependency.
    fn worktree(&self, name: &str, dependency: &str) -> PathBuf {
        let target = self.root.join(name).join("target");
        fs::create_dir_all(target.join("debug/build/foo/abc/out")).unwrap();
        fs::create_dir_all(target.join("debug/build/dep/def/out")).unwrap();
        fs::write(target.join("debug/build/dep/def/out/libdep-def.rmeta"), dependency).unwrap();
        target
    }

    fn args(&self, target: &Path) -> Vec<String> {
        let (build, home) = (target.join("debug/build").display().to_string(), &self.cargo_home);
        let args = format!(
            "--crate-name foo {home}registry/src/index/foo-1.0.0/src/lib.rs --emit=dep-info,metadata,link \
             -C extra-filename=-abc --out-dir {build}/foo/abc/out -L dependency={build}/dep/def/out \
             --extern dep={build}/dep/def/out/libdep-def.rmeta"
        );
        args.split(' ').map(str::to_string).collect()
    }

    fn unit(&self, args: &[String]) -> Result<Unit, &'static str> {
        Unit::model(args, self.cargo_home.clone(), None, None)
    }

    /// The unit of `foo` in `target` after a build script wrote `read` and `unread` into its OUT_DIR.
    fn generated(&self, target: &Path, read: &str, unread: &str) -> (Unit, String) {
        let out_dir = target.join("debug/build/foo/run/out");
        fs::create_dir_all(&out_dir).unwrap();
        fs::write(out_dir.join("read.rs"), read).unwrap();
        fs::write(out_dir.join("unread.rs"), unread).unwrap();
        let out_dir = out_dir.display().to_string();
        let dep_info = format!(
            "{}/debug/build/foo/abc/out/libfoo-abc.rmeta: {out_dir}/read.rs\n",
            target.display()
        );
        (
            Unit::model(&self.args(target), self.cargo_home.clone(), Some(out_dir), None).unwrap(),
            dep_info,
        )
    }

    fn build(
        &self,
        args: &[String],
        store: &Path,
        worktree: Option<String>,
    ) -> Result<(String, i32), &'static str> {
        cached(
            &self.rustc,
            args,
            store,
            &Unit::model(args, self.cargo_home.clone(), None, worktree)?,
            &mut std::io::stderr(),
        )
    }

    fn key(&self, args: &[String]) -> String {
        let store = self.root.join("store");
        self.unit(args).unwrap().key(&self.rustc, args, &store, env::vars_os()).unwrap()
    }

    /// A run of the build script `body` in a worktree, with `variables` added, and its OUT_DIR.
    ///
    /// The real script is `build_script_build.real` next to OUT_DIR, and each run appends to `runs` there.
    fn script(&self, name: &str, body: &str, variables: &[(&str, &str)]) -> (Run, PathBuf) {
        let out_dir = self.worktree(name, "metadata").join("debug/build/foo/run/out");
        let directory = PathBuf::from(format!("{}registry/src/index/foo-1.0.0", self.cargo_home));
        fs::create_dir_all(&out_dir).and_then(|()| fs::create_dir_all(&directory)).unwrap();
        let real = out_dir.with_file_name("build_script_build.real");
        executable(&real, &format!("#!/bin/sh\necho ran >> \"$OUT_DIR/../runs\"\n{body}\n"));
        let tmp = self.root.join(name).join("tmp").display().to_string();
        let given = [
            ("OUT_DIR", out_dir.display().to_string()),
            ("RUSTC", self.rustc.clone()),
            ("TMPDIR", tmp),
        ];
        let added = variables.iter().map(|(name, value)| (*name, value.to_string()));
        let environment =
            given.into_iter().chain(added).map(|(name, value)| (name.to_string(), value));
        (
            Run::model(&real, directory, environment.collect(), self.cargo_home.clone()).unwrap(),
            out_dir,
        )
    }
}

/// Writes the executable script `text` at `path` through `cp`.
///
/// Linux refuses to run a file that a process has open for writing. A child that another test forks has it open until
/// it runs exec, so no test process opens `path` for writing.
fn executable(path: &Path, text: &str) {
    let source = path.with_extension("source");
    fs::write(&source, text).unwrap();
    assert!(Command::new("cp").arg(&source).arg(path).status().unwrap().success());
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn real(out_dir: &Path) -> PathBuf {
    out_dir.with_file_name("build_script_build.real")
}

/// A fixture whose stand-in rustc takes a second after it writes `runs`.
fn slow(name: &str) -> Fixture {
    let fixture = fixture(name, "");
    let script = fs::read_to_string(&fixture.rustc).unwrap();
    executable(
        Path::new(&fixture.rustc),
        &script.replace("\nprintf rmeta", "\nsleep 1\nprintf rmeta"),
    );
    fixture
}

fn wait_for(path: &Path) {
    while !path.exists() {
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn roots_round_trip_and_leave_sibling_directories_alone() {
    let roots = Roots {
        target: "/work/a/target/".to_string(),
        cargo_home: "/home/dev/.cargo/".to_string(),
        worktree: None,
    };
    let text =
        "dependency=/work/a/target/debug /home/dev/.cargo/registry/src/x /work/a/target-old/debug";
    let normalized = roots.normalized(text);
    assert_eq!(
        normalized,
        format!("dependency={TARGET}debug {CARGO_HOME}registry/src/x /work/a/target-old/debug")
    );
    assert_eq!(roots.localized(&normalized), text);
    let other = Roots { target: "/work/b/target/".to_string(), ..roots };
    assert!(other.localized(&normalized).starts_with("dependency=/work/b/target/debug "));
}

// A target directory that something other than cargo created has no CACHEDIR.TAG.
#[test]
fn target_directory_follows_the_layout_and_the_triple() {
    let found = |out_dir: &str, triple| {
        unit::target_directory(Path::new(out_dir), triple).map(Path::to_path_buf)
    };
    let target = Some(PathBuf::from("/work/target"));
    assert_eq!(found("/work/target/debug/build/foo/abc/out", None), target);
    let wasm = "/work/target/wasm32-unknown-unknown/release/build/foo/abc/out";
    assert_eq!(found(wasm, Some("wasm32-unknown-unknown")), target);
    assert_eq!(found("/work/target/debug/build/foo/abc/out", Some("aarch64-apple-darwin")), target);
    assert_eq!(found("/nowhere/target/debug/deps", None), None);
}

#[test]
fn key_is_shared_by_worktrees_and_changes_with_each_input() {
    let fixture = fixture("key", "");
    let key = |name: &str, dependency: &str| {
        fixture.key(&fixture.args(&fixture.worktree(name, dependency)))
    };
    let first = key("first", "metadata");
    assert_eq!(first, key("second", "metadata"));
    assert_eq!(
        first,
        key("a-worktree-with-a-longer-path", "metadata"),
        "the remap of the target is keyed by placeholder"
    );
    assert_ne!(
        first,
        key("changed", "other metadata"),
        "the contents of an extern are part of the key"
    );

    let second = fixture.root.join("second/target");
    let mut args = fixture.args(&second);
    args.push("--cfg=feature=\"std\"".to_string());
    assert_ne!(first, fixture.key(&args), "the arguments are part of the key");

    // A sibling of the metadata that is still being written when a dependent starts must not move the key.
    fs::write(second.join("debug/build/dep/def/out/libdep-def.rlib"), "half written").unwrap();
    assert_eq!(first, fixture.key(&fixture.args(&second)));
    // A cross build names the host's directory of dependencies before any host crate is built.
    let missing = format!("dependency={}/debug/build/host/out", second.display());
    fixture.key(&[&fixture.args(&second)[..], &["-L".to_string(), missing]].concat());
}

#[test]
fn model_refuses_what_a_key_cannot_predict() {
    let fixture = fixture("model", "");
    let target = fixture.worktree("only", "metadata");
    let args = fixture.args(&target);
    let with = |extra: &[&str]| {
        fixture.unit(
            &[&args[..], &extra.iter().map(|arg| arg.to_string()).collect::<Vec<_>>()].concat(),
        )
    };
    assert!(with(&[]).is_ok());
    assert!(unit::is_query(&[&args[..], &["--print=cfg".to_string()]].concat()));
    assert!(unit::is_query(&["-".to_string()]) && !unit::is_query(&args));
    assert_eq!(with(&["-C", "incremental=/tmp/incremental"]).err(), Some("incremental"));
    assert_eq!(fixture.unit(&args[..6]).err(), Some("no-out-dir"));
    let assembly = Vec::from_iter(args.iter().map(|arg| arg.replace("metadata,link", "asm")));
    assert_eq!(fixture.unit(&assembly).err(), Some("emit"));
}

#[test]
fn dep_info_is_rewritten_unless_it_names_an_unkeyed_file() {
    let fixture = fixture("dep-info", "");
    let target = fixture.worktree("only", "metadata");
    let unit = fixture.unit(&fixture.args(&target)).unwrap();
    let out = target.join("debug/build/foo/abc/out").display().to_string();
    let source = format!("{}registry/src/index/foo-1.0.0/src/lib.rs", fixture.cargo_home);
    let dep_info =
        format!("{out}/libfoo-abc.rmeta: {source}\n\n{source}:\n# env-dep:CARGO_PKG_NAME=foo\n");

    let (portable, reads) = unit.portable_dep_info(&dep_info).unwrap();
    assert_eq!(reads, "");
    assert!(portable.starts_with(&format!(
        "{TARGET}debug/build/foo/abc/out/libfoo-abc.rmeta: {CARGO_HOME}registry"
    )));
    assert_eq!(unit.roots.localized(&portable), dep_info);

    let read = format!("{dep_info}# env-dep:PATH=/bin\n# env-dep:HOME=/home/dev\n");
    assert_eq!(unit.portable_dep_info(&read).unwrap().1, "env HOME\nenv PATH");
    let variant = |reads| unit.variant("key", reads, None, &fixture.root.join("hashes")).unwrap();
    assert_eq!(variant(""), "key");
    assert_ne!(variant("env HOME\nenv PATH"), variant("env PATH"));
    assert_eq!(variant("env PATH"), variant("env PATH"));
    assert_eq!(
        unit.portable_dep_info(&format!("{out}/libfoo-abc.rmeta: /usr/include/zlib.h\n")),
        None
    );
    let elsewhere = target.join("debug/build/other/123/out/generated.rs");
    assert_eq!(
        unit.portable_dep_info(&format!("{out}/libfoo-abc.rmeta: {}\n", elsewhere.display())),
        None
    );
}

// A build script of Cranelift writes its own path into a file that only another target of the crate includes.
#[test]
fn entry_follows_the_generated_files_that_rustc_read_and_no_others() {
    let fixture = fixture("generated", "");
    let (first, second) =
        (fixture.worktree("first", "metadata"), fixture.worktree("second", "metadata"));
    let (unit, dep_info) = fixture.generated(&first, "const N: u8 = 1;", "the first worktree");
    let reads = unit.portable_dep_info(&dep_info).unwrap().1;
    assert_eq!(reads, format!("file {TARGET}debug/build/foo/run/out/read.rs"));

    let (same, _) = fixture.generated(&second, "const N: u8 = 1;", "the second worktree");
    let variant =
        |unit: &Unit| unit.variant("key", &reads, None, &fixture.root.join("hashes")).unwrap();
    assert_eq!(variant(&unit), variant(&same));
    let (changed, _) = fixture.generated(&second, "const N: u8 = 2;", "the second worktree");
    assert_ne!(variant(&unit), variant(&changed));
}

#[test]
fn miss_publishes_and_another_worktree_restores_clones() {
    let fixture = fixture("restore", "");
    let store = fixture.root.join("store");
    let (first, second) =
        (fixture.worktree("first", "metadata"), fixture.worktree("second", "metadata"));
    let out = |target: &Path| target.join("debug/build/foo/abc/out");
    let build = |target: &Path| fixture.build(&fixture.args(target), &store, None);

    assert_eq!(build(&first), Ok(("miss".to_string(), 0)));
    let args = fs::read_to_string(out(&first).join("args")).unwrap();
    assert!(
        args.ends_with(&format!(" --remap-path-prefix={}/=target/\n", first.display())),
        "{args}"
    );
    let entry = fs::read_dir(store.join("entries")).unwrap().next().unwrap().unwrap().path();
    let stored = entry.join("libfoo-abc.rlib");
    // The file of the store is older than the dependency in the second worktree, as a file from an earlier build is.
    fs::File::open(&stored)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(3600))
        .unwrap();

    assert_eq!(build(&second), Ok(("hit".to_string(), 0)));
    assert!(!out(&second).join("runs").exists(), "rustc must not run on a hit");
    assert!(entry.join(store::RECORD).is_file());
    assert!(!out(&second).join(store::RECORD).exists(), "the record must not be restored");
    let restored = out(&second).join("libfoo-abc.rlib");
    assert_eq!(fs::read_to_string(&restored).unwrap(), "rlib");
    let memo = unit::recalled(&store.join("hashes"), &unit::identity(&restored).unwrap());
    assert_eq!(
        memo,
        Some(blake3::hash(b"rlib").as_bytes().to_vec()),
        "a dependent's key reads no library"
    );
    let dep_info = fs::read_to_string(out(&second).join("foo-abc.d")).unwrap();
    assert!(dep_info.starts_with(&format!(
        "{}/libfoo-abc.rmeta: {}registry",
        out(&second).display(),
        fixture.cargo_home
    )));

    let modified = |path: &Path| fs::metadata(path).unwrap().modified().unwrap();
    let dependency = second.join("debug/build/dep/def/out/libdep-def.rmeta");
    assert!(
        modified(&restored) >= modified(&dependency),
        "cargo rebuilds a crate that is older than its dependency"
    );
    assert_ne!(fs::metadata(&restored).unwrap().ino(), fs::metadata(&stored).unwrap().ino());
    fs::write(&restored, "overwritten").unwrap();
    assert_eq!(
        fs::read_to_string(&stored).unwrap(),
        "rlib",
        "a write to a restored output must not reach the store"
    );

    // The overwritten output is deleted before the store's is cloned in again.
    assert_eq!(build(&second), Ok(("hit".to_string(), 0)));
    assert_eq!(fs::read_to_string(&restored).unwrap(), "rlib");
    assert_eq!(fs::read_to_string(out(&first).join("runs")).unwrap(), "ran\n");
}

#[test]
fn store_that_cannot_clone_links_in_and_copies_out_what_its_hash_verifies() {
    let fixture = fixture("links", "");
    let store = fixture.root.join("store");
    assert!(Files::probe(&store) == Files::Clones && store.join("clones").exists(), "APFS and XFS");
    fs::remove_file(store.join("clones"))
        .and_then(|()| fs::write(store.join("links"), ""))
        .unwrap();
    assert_eq!(Files::probe(&store), Files::Links);
    let worktrees = ["first", "second", "third", "fourth"];
    let [first, second, third, fourth] = worktrees.map(|name| fixture.worktree(name, "metadata"));
    let out = |target: &Path| target.join("debug/build/foo/abc/out");
    let build = |target: &Path| fixture.build(&fixture.args(target), &store, None);
    let collect = |cap| gc::collect(&store, 0, cap, false, || (100, 100));

    assert_eq!(build(&first), Ok(("miss".to_string(), 0)));
    let entry = fs::read_dir(store.join("entries")).unwrap().next().unwrap().unwrap().path();
    let (stored, output) = (entry.join("libfoo-abc.rlib"), out(&first).join("libfoo-abc.rlib"));
    let added = fs::read_to_string(store.join("gc.added")).unwrap();
    let lengths = fs::read_dir(&entry).unwrap().map(|file| file.unwrap().metadata().unwrap().len());
    assert_eq!(
        added,
        format!("{}\n", lengths.sum::<u64>()),
        "the store gains the size of the entry"
    );
    let metadata = |path: &Path| fs::metadata(path).unwrap();
    assert_eq!(metadata(&stored).ino(), metadata(&output).ino());
    assert!(!metadata(&output).permissions().readonly());
    age(&stored, 3600);

    assert_eq!(build(&second), Ok(("hit".to_string(), 0)));
    let restored = out(&second).join("libfoo-abc.rlib");
    assert_ne!(metadata(&restored).ino(), metadata(&stored).ino());
    let memo = unit::recalled(&store.join("hashes"), &unit::identity(&restored).unwrap());
    assert_eq!(memo, Some(blake3::hash(b"rlib").as_bytes().to_vec()));
    let modified = |path: &Path| metadata(path).modified().unwrap();
    assert!(modified(&restored) > modified(&stored), "cargo rebuilds an older output");

    // A tool that writes the output of the first worktree in place changes the file of the store, and the next restore
    // detects that.
    fs::write(&output, "stripped").unwrap();
    assert_eq!(build(&third), Ok(("miss evicted".to_string(), 0)));
    assert_eq!(fs::read_to_string(out(&third).join("libfoo-abc.rlib")).unwrap(), "rlib");
    let names = fs::read_dir(out(&third)).unwrap().map(|file| file.unwrap().file_name());
    assert!(!names.into_iter().any(|name| name.to_string_lossy().ends_with(".trucker")));
    assert_eq!(build(&fourth), Ok(("hit".to_string(), 0)));

    // A file of an entry that a worktree links too counts nothing, until a measure finds that link gone.
    let size = collect(u64::MAX).size;
    fs::remove_file(out(&third).join("libfoo-abc.rlib")).unwrap();
    assert_eq!(collect(u64::MAX).size, size, "a measure holds for an hour");
    fs::remove_file(store.join("gc.sizes")).unwrap();
    let rlib = crate::usage::measure(&entry_of(&store).join("libfoo-abc.rlib")).own;
    assert!(rlib > 0);
    assert_eq!(collect(u64::MAX).size, size + rlib);
    let entry = crate::usage::measure(&entry_of(&store));
    assert_eq!(entry.shared.len(), 1, "the rmeta that the worktree links");
    age(&entry_of(&store), 7200);
    let collected = collect(0);
    assert_eq!((collected.entries, collected.freed), (1, entry.own));
}

/// The only entry of `foo` in `store`.
fn entry_of(store: &Path) -> PathBuf {
    let mut entries =
        fs::read_dir(store.join("entries")).unwrap().map(|entry| entry.unwrap().path());
    entries.find(|path| path.is_dir()).unwrap()
}

#[test]
fn linked_entry_is_verified_in_the_worktree_that_stored_it_and_after_the_marker_changes() {
    let fixture = fixture("linked", "");
    let store = fixture.root.join("store");
    fs::write(store.join("links"), "").unwrap();
    let [first, second] = ["first", "second"].map(|name| fixture.worktree(name, "metadata"));
    let out = |target: &Path| target.join("debug/build/foo/abc/out");
    let build = |target: &Path| fixture.build(&fixture.args(target), &store, None);
    assert_eq!(build(&first), Ok(("miss".to_string(), 0)));

    // A write through the kept `.rmeta`, which is the stored file itself, is not served again.
    fs::write(out(&first).join("libfoo-abc.rmeta"), "changed").unwrap();
    assert_eq!(build(&first), Ok(("miss evicted".to_string(), 0)));
    assert_eq!(fs::read_to_string(out(&first).join("libfoo-abc.rmeta")).unwrap(), "rmeta");

    // An entry that was linked is still verified once the marker of the store says that it clones.
    fs::remove_file(store.join("links"))
        .and_then(|()| fs::write(store.join("clones"), ""))
        .unwrap();
    fs::write(out(&first).join("libfoo-abc.rlib"), "changed").unwrap();
    assert_eq!(build(&second), Ok(("miss evicted".to_string(), 0)));
    assert_eq!(fs::read_to_string(out(&second).join("libfoo-abc.rlib")).unwrap(), "rlib");
    assert!(!entry_of(&store).join(".hashes").exists(), "the entry is stored again by clone");
}

/// A sink for cargo that asserts at each write that every output of `foo` in `out` is whole and verified.
struct Cargo<'a>(&'a Path, Vec<u8>);

impl std::io::Write for Cargo<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let read = |name: &str| fs::read_to_string(self.0.join(name)).unwrap_or_default();
        assert_eq!(
            (read("libfoo-abc.rlib"), read("libfoo-abc.rmeta")),
            ("rlib".into(), "rmeta".into())
        );
        self.1.extend(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// cargo marks a unit's metadata ready at each artifact notification and panics at a second one.
#[test]
fn restore_gives_cargo_its_stderr_only_once_every_file_verified_and_an_eviction_none() {
    let fixture = fixture("sink", "");
    let store = fixture.root.join("store");
    fs::write(store.join("links"), "").unwrap();
    let [first, second, third] =
        ["first", "second", "third"].map(|n| fixture.worktree(n, "metadata"));
    let out = |target: &Path| target.join("debug/build/foo/abc/out");
    assert_eq!(fixture.build(&fixture.args(&first), &store, None), Ok(("miss".to_string(), 0)));
    let mut cargo = Cargo(&out(&second), Vec::new());
    let unit = fixture.unit(&fixture.args(&second)).unwrap();
    assert_eq!(store::restore(&unit, &entry_of(&store), Files::Links, &mut cargo), Ok(()));
    assert_eq!(String::from_utf8(cargo.1).unwrap().lines().count(), 1);

    fs::write(out(&first).join("libfoo-abc.rlib"), "stripped").unwrap();
    let (args, mut cargo) = (fixture.args(&third), Cargo(&out(&third), Vec::new()));
    let unit = fixture.unit(&args).unwrap();
    let outcome = cached(&fixture.rustc, &args, &store, &unit, &mut cargo);
    assert_eq!(outcome, Ok(("miss evicted".to_string(), 0)));
    assert_eq!(String::from_utf8(cargo.1).unwrap().lines().count(), 1, "only the compile's line");
}

#[test]
fn cloning_store_keeps_its_layout_with_a_file_of_its_own_for_each_output() {
    let fixture = fixture("layout", "");
    let store = fixture.root.join("store");
    let first = fixture.worktree("first", "metadata");
    assert_eq!(fixture.build(&fixture.args(&first), &store, None), Ok(("miss".to_string(), 0)));
    let body = r#"mkdir "$OUT_DIR/lib" && printf a > "$OUT_DIR/lib/libfoo.a" && echo "$OUT_DIR" > "$OUT_DIR/f""#;
    let (run, out) = fixture.script("script", body, &[]);
    assert_eq!(replay_or_run(&real(&out), Ok(run), &store), ("miss".to_string(), 0));
    let files = |root: &Path| {
        let mut files =
            Vec::from_iter(store::tree(root).unwrap().into_iter().map(|(path, kind)| {
                let links =
                    if kind.is_dir() { 0 } else { fs::metadata(root.join(&path)).unwrap().nlink() };
                (path.display().to_string(), links)
            }));
        files.sort();
        files
    };
    let entry = [".record", ".stderr", "foo-abc.d", "libfoo-abc.rlib", "libfoo-abc.rmeta"];
    assert_eq!(files(&entry_of(&store)), entry.map(|name| (name.to_string(), 1)));
    let mut scripts =
        fs::read_dir(store.join("scripts")).unwrap().map(|entry| entry.unwrap().path());
    let script = files(&scripts.find(|path| path.is_dir()).unwrap());
    let script_entry = [("out", 0), ("out/lib", 0), ("out/lib/libfoo.a", 1), ("stderr", 1)];
    let script_entry = script_entry.into_iter().chain([
        ("stdout", 1),
        ("text", 0),
        ("text/f", 1),
        ("text/lib", 0),
    ]);
    assert_eq!(script, Vec::from_iter(script_entry.map(|(name, links)| (name.to_string(), links))));
    for kind in [Files::Clones, Files::Links] {
        kind.duplicate_tree(&out, &fixture.root.join(format!("{kind:?}"))).unwrap();
        let shared = files(&fixture.root.join(format!("{kind:?}")));
        assert!(shared.iter().all(|(_, n)| [0, 1].contains(n)), "{shared:?}");
    }
}

#[test]
fn unkeyed_file_is_compiled_every_time_and_never_stored() {
    let fixture = fixture("unkeyed", "/usr/include/zlib.h");
    let (store, target) = (fixture.root.join("store"), fixture.worktree("only", "metadata"));
    let build = || fixture.build(&fixture.args(&target), &store, None);
    assert_eq!(build(), Ok(("passthrough unkeyed-input".to_string(), 0)));
    assert_eq!(build(), Ok(("passthrough unkeyed-input".to_string(), 0)));
    assert_eq!(fs::read_dir(store.join("entries")).unwrap().count(), 0);
}

#[test]
fn store_on_another_filesystem_is_refused_and_said_once() {
    let fixture = fixture("filesystems", "");
    let target = fixture.worktree("only", "metadata");
    assert!(store::same_filesystem(&fixture.root.join("store"), &target));
    assert!(!store::same_filesystem(Path::new("/dev"), &target));

    let build = || fixture.build(&fixture.args(&target), Path::new("/dev"), None);
    assert_eq!(build(), Err("other-filesystem"));
    let store = fixture.root.join("store");
    crate::notice(&store, "notice", "is said once per build");
    crate::notice(&store, "notice", "is said once per build");
    assert_eq!(fs::read_dir(store.join("notices")).unwrap().count(), 1);
    assert!(
        !target.join("debug/build/foo/abc/out/runs").exists(),
        "the caller runs rustc, uncached"
    );
}

// The list holds the variables that the reference of cargo 1.100 documents for build scripts, and a name that ends in
// an underscore is a prefix. When the supported toolchain changes, the list follows its reference and
// `script::UNKEYED_CARGO` is reviewed.
#[test]
fn every_documented_script_variable_is_keyed_or_left_out_with_a_reason() {
    let documented = "CARGO CARGO_MANIFEST_DIR CARGO_MANIFEST_PATH CARGO_MANIFEST_LINKS CARGO_MAKEFLAGS \
                      CARGO_FEATURE_ CARGO_CFG_ OUT_DIR TARGET HOST NUM_JOBS DEBUG OPT_LEVEL PROFILE DEP_ RUSTC \
                      RUSTDOC RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER RUSTC_LINKER CARGO_ENCODED_RUSTFLAGS CARGO_PKG_";
    let documented = Vec::from_iter(documented.split_whitespace());
    for name in &documented {
        let variable = if name.ends_with('_') { format!("{name}NAME") } else { name.to_string() };
        let reason = script::UNKEYED_CARGO.iter().find(|(unkeyed, _)| unkeyed == name);
        assert_ne!(script::is_keyed(&variable), reason.is_some(), "{name} is keyed or left out");
        assert!(reason.is_none_or(|(_, reason)| !reason.is_empty()), "{name} has a reason");
    }
    for (name, _) in script::UNKEYED_CARGO {
        assert!(documented.contains(name), "{name} is documented");
    }
}

#[test]
fn script_key_is_shared_by_worktrees_and_moved_only_by_keyed_variables() {
    let fixture = fixture("script-key", "");
    let store = fixture.root.join("store");
    let key = |name, variables: &[(&str, &str)]| {
        fixture.script(name, "", variables).0.key(&store).unwrap()
    };
    let first = key("first", &[]);
    assert_eq!(first, key("second", &[]), "the worktrees differ in TMPDIR, which is not keyed");
    assert_eq!(first, key("second", &[("NUM_JOBS", "3"), ("PATH", "/opt/bin")]));
    assert_ne!(first, key("second", &[("CARGO_FEATURE_STD", "1")]));

    let include = fixture.root.join("second/target/debug/build/dep/def/out");
    let include = include.display().to_string();
    let with_include = || key("second", &[("DEP_DEP_INCLUDE", &include)]);
    let before = with_include();
    fs::write(Path::new(&include).join("dep.h"), "#define DEP 1").unwrap();
    assert_ne!(
        before,
        with_include(),
        "the contents of a dependency's include directory are keyed"
    );

    let with_pkg_config = |name: &'static str| {
        let root = fixture.root.join(name).join("target/debug/build/pc/def/out");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("dep.pc"), format!("prefix={}\n", root.display())).unwrap();
        key(name, &[("DEP_DEP_ROOT", &root.display().to_string())])
    };
    assert_eq!(
        with_pkg_config("first"),
        with_pkg_config("second"),
        "a text file names its worktree"
    );
}

#[test]
fn script_compiles_c_with_a_prefix_map_and_runs_again_without_one_where_it_fails() {
    let fixture = fixture("script-prefix-map", "");
    let store = fixture.root.join("store");
    let (run, out) = fixture.script("only", "", &[("CFLAGS", "-O2")]);
    let target = fixture.root.join("only/target/").display().to_string();
    let runs = fixture.root.join("runs");
    let body = format!(
        "echo \"$CFLAGS|$CXXFLAGS\" >> {}\ncase $CFLAGS in *-map=*) exit 1;; esac",
        runs.display()
    );
    executable(&real(&out), &format!("#!/bin/sh\n{body}\n"));
    let outcome = replay_or_run(&real(&out), Ok(run), &store);
    assert_eq!(outcome, ("passthrough script-failed".to_string(), 0));
    let cargo = |name| env::var(name).unwrap_or_default();
    let (cflags, cxxflags) = (cargo("CFLAGS"), cargo("CXXFLAGS"));
    let runs = fs::read_to_string(&runs).unwrap();
    let mapped =
        format!("-O2 -ffile-prefix-map={target}=target/|-ffile-prefix-map={target}=target/");
    assert_eq!(
        runs,
        format!("{mapped}\n{cflags}|{cxxflags}\n"),
        "the second run has cargo's environment"
    );
}

#[test]
fn script_run_is_stored_and_replayed_with_rewritten_paths_and_current_mtimes() {
    let fixture = fixture("script-replay", "");
    let store = fixture.root.join("store");
    let body = r#"mkdir -p "$OUT_DIR/lib" && printf archive > "$OUT_DIR/lib/libfoo.a"
printf 'include!("%s/data.rs");' "$OUT_DIR" > "$OUT_DIR/generated.rs"
echo "cargo:rustc-link-search=native=$OUT_DIR/lib"
echo cargo::rerun-if-env-changed=FOO_LIB_DIR
echo cargo:rerun-if-changed=build.rs"#;
    let (first, first_out) = fixture.script("first", body, &[]);
    assert_eq!(replay_or_run(&real(&first_out), Ok(first), &store), ("miss".to_string(), 0));
    let mut entries =
        fs::read_dir(store.join("scripts")).unwrap().map(|entry| entry.unwrap().path());
    let entry = entries.find(|path| path.is_dir()).unwrap();
    let stdout = fs::read_to_string(entry.join("stdout")).unwrap();
    assert!(
        stdout.starts_with(&format!("cargo:rustc-link-search=native={TARGET}debug/")),
        "{stdout}"
    );
    fs::File::open(entry.join("out/lib/libfoo.a"))
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(3600))
        .unwrap();

    let (second, out) = fixture.script("second", body, &[]);
    fs::write(out.join("stale"), "").unwrap();
    assert_eq!(replay_or_run(&real(&out), Ok(second), &store), ("hit".to_string(), 0));
    assert!(!out.join("../runs").exists(), "the script must not run on a hit");
    assert!(!out.join("stale").exists());
    let generated = fs::read_to_string(out.join("generated.rs")).unwrap();
    assert_eq!(generated, format!("include!(\"{}/data.rs\");", out.display()));
    let archive = out.join("lib/libfoo.a");
    assert_eq!(fs::read_to_string(&archive).unwrap(), "archive");
    let modified = fs::metadata(&archive).unwrap().modified().unwrap();
    assert!(modified > SystemTime::now() - Duration::from_secs(600), "cargo reruns what is older");

    let (other, out) = fixture.script("second", body, &[("FOO_LIB_DIR", "/opt/foo")]);
    assert_eq!(replay_or_run(&real(&out), Ok(other), &store), ("miss".to_string(), 0));
}

#[test]
fn script_run_whose_outputs_depend_on_more_than_the_key_is_never_stored() {
    let fixture = fixture("script-refusals", "");
    let store = fixture.root.join("store");
    let cases = [
        ("echo cargo:rerun-if-changed=/usr/include/zlib.h", "unkeyed-input", 0),
        ("echo cargo::rerun-if-changed=../../../../../outside.h", "unkeyed-input", 0),
        (r#"ln -s /etc/hosts "$OUT_DIR/hosts""#, "symlink", 0),
        (r#"ln -s ../runs "$OUT_DIR/runs""#, "symlink", 0),
        (r#"ln -s . "$OUT_DIR/here" && ln -s here/.. "$OUT_DIR/up""#, "symlink", 0),
        (&format!(r#"ln -s {}runs "$OUT_DIR/home""#, fixture.cargo_home), "symlink", 0),
        ("exit 3", "script-failed", 3),
    ];
    fs::create_dir_all(&fixture.cargo_home).unwrap();
    fs::write(format!("{}runs", fixture.cargo_home), "").unwrap();
    for (case, (body, reason, code)) in cases.into_iter().enumerate() {
        let (run, out) = fixture.script(&format!("case-{case}"), body, &[]);
        let outcome = (format!("passthrough {reason}"), code);
        assert_eq!(replay_or_run(&real(&out), Ok(run), &store), outcome, "{body}");
        assert_eq!(fs::read_to_string(out.join("../runs")).unwrap(), "ran\n");
    }
    assert_eq!(fs::read_dir(store.join("scripts")).unwrap().count(), 0);
}

// cxx-build links a header beside its bridge, links cxx.h by absolute path, and links its crate by a relative path
// that climbs out of the target directory into CARGO_HOME.
#[test]
fn script_run_with_symlinks_into_out_dir_or_the_registry_is_stored_and_replayed() {
    let fixture = fixture("script-links", "");
    let store = fixture.root.join("store");
    let source = format!("{}registry/src/index/foo-1.0.0", fixture.cargo_home);
    let body = format!(
        r#"mkdir -p "$OUT_DIR/include/foo" "$OUT_DIR/crate"
printf header > "$OUT_DIR/include/foo/lib.rs.h"
ln -s lib.rs.h "$OUT_DIR/include/foo/lib.rs"
ln -s "$OUT_DIR/include/foo/lib.rs.h" "$OUT_DIR/header"
ln -s {source}/include/cxx.h "$OUT_DIR/include/cxx.h"
ln -s ../../../../../../../../cargo/registry/src/index/foo-1.0.0 "$OUT_DIR/crate/foo""#
    );
    let (first, first_out) = fixture.script("first", &body, &[]);
    fs::create_dir_all(format!("{source}/include")).unwrap();
    fs::write(format!("{source}/include/cxx.h"), "cxx").unwrap();
    assert_eq!(replay_or_run(&real(&first_out), Ok(first), &store), ("miss".to_string(), 0));
    let links = fs::read_to_string(entry_of_scripts(&store).join("links")).unwrap();
    assert!(
        links.contains(&format!("absolute\t{TARGET}debug/build/foo/run/out/include/foo/lib.rs.h"))
    );
    assert!(
        links.contains(&format!("relative\t{CARGO_HOME}registry/src/index/foo-1.0.0\tcrate/foo"))
    );

    // A worktree one directory deeper climbs once more to reach CARGO_HOME.
    let (second, out) = fixture.script("deeper/second", &body, &[]);
    assert_eq!(replay_or_run(&real(&out), Ok(second), &store), ("hit".to_string(), 0));
    assert!(!out.join("../runs").exists(), "the script must not run on a hit");
    let read = |name: &str| fs::read_to_string(out.join(name)).unwrap();
    let link = |name: &str| fs::read_link(out.join(name)).unwrap();
    assert_eq!(link("include/foo/lib.rs"), Path::new("lib.rs.h"));
    assert_eq!(read("include/foo/lib.rs"), "header");
    assert_eq!(link("header"), out.join("include/foo/lib.rs.h"));
    assert_eq!(link("include/cxx.h"), Path::new(&source).join("include/cxx.h"));
    let climb = "../../../../../../../../../cargo/registry/src/index/foo-1.0.0";
    assert_eq!(link("crate/foo"), Path::new(climb));
    assert_eq!(read("crate/foo/include/cxx.h"), "cxx");
    let fed = |out: &Path, name: &str| {
        let target = format!("{}/", fixture.root.join(name).join("target").display());
        let roots = Roots { target, cargo_home: fixture.cargo_home.clone(), worktree: None };
        let mut fields = Vec::new();
        let memos = fixture.root.join("memos");
        unit::hash_tree(&roots, out, &memos, &mut |bytes| fields.push(bytes.to_vec())).unwrap();
        fields
    };
    assert_eq!(
        fed(&first_out, "first"),
        fed(&out, "deeper/second"),
        "a dependent's key is the same"
    );

    fs::remove_dir_all(format!("{source}/include")).unwrap();
    let (third, out) = fixture.script("third", &body, &[]);
    let outcome = replay_or_run(&real(&out), Ok(third), &store);
    assert_eq!(outcome.0, "passthrough symlink", "a stored link that would dangle runs the script");
    assert_eq!(fs::read_to_string(out.join("../runs")).unwrap(), "ran\n");
}

/// A run of a script that bakes OUT_DIR and CFLAGS into an archive, as a vendored C library does, then runs `extra`.
///
/// Each run appends its OUT_DIR to `runs` in the fixture.
fn baking_script(
    fixture: &Fixture,
    name: &str,
    extra: &str,
    variables: &[(&str, &str)],
) -> (Run, PathBuf) {
    let (run, out) = fixture.script(name, "", variables);
    let runs = fixture.root.join("runs").display().to_string();
    let body = format!(
        r#"#!/bin/sh
echo "$OUT_DIR" >> {runs}
mkdir -p "$OUT_DIR/lib" && printf '\377%s %s' "$OUT_DIR" "$CFLAGS" > "$OUT_DIR/lib/libcrypto.a"
printf 'prefix=%s\n' "$OUT_DIR" > "$OUT_DIR/lib/openssl.pc"
echo "cargo:rustc-link-search=native=$OUT_DIR/lib"
echo "built in $OUT_DIR" >&2
{extra}
"#
    );
    executable(&real(&out), &body);
    (run, out)
}

fn contains(path: &Path, text: &Path) -> bool {
    store::any_contains(&[path], &[&text.display().to_string()])
}

fn runs(fixture: &Fixture) -> Vec<String> {
    let runs = fs::read_to_string(fixture.root.join("runs")).unwrap_or_default();
    Vec::from_iter(runs.lines().map(str::to_string))
}

fn marked(store: &Path, kind: &str) -> usize {
    let children = fs::read_dir(store.join("out")).into_iter().flatten();
    children
        .filter(|entry| entry.as_ref().unwrap().path().extension() == Some(kind.as_ref()))
        .count()
}

#[test]
fn script_that_bakes_out_dir_into_a_binary_is_stored_from_a_run_at_a_fixed_out_dir() {
    let fixture = fixture("script-fixed", "");
    let store = fixture.root.join("store");
    let (first, out) = baking_script(&fixture, "first", "", &[]);
    assert_eq!(replay_or_run(&real(&out), Ok(first), &store), ("miss".to_string(), 0));
    let ran = runs(&fixture);
    assert_eq!(ran[0], out.display().to_string(), "the script runs in cargo's environment first");
    assert!(ran[1].starts_with(&store.join("out").display().to_string()), "{ran:?}");
    assert!(ran[1].ends_with("/0/target/debug/build/foo/run/out"), "{ran:?}");
    let library = out.join("lib/libcrypto.a");
    assert!(contains(&library, Path::new(&ran[1])));
    assert!(!contains(&library, &fixture.root.join("first")), "the archive names the worktree");
    let pc = fs::read_to_string(out.join("lib/openssl.pc")).unwrap();
    assert_eq!(pc, format!("prefix={}\n", out.display()), "a text file names the real OUT_DIR");
    assert_eq!(marked(&store, "fixed"), 1);
    let pools = Vec::from_iter(fs::read_dir(store.join("out")).unwrap().map(|e| e.unwrap().path()));
    let pool = pools.iter().find(|path| path.is_dir()).unwrap();
    let left = Vec::from_iter(fs::read_dir(pool).unwrap().map(|entry| entry.unwrap().file_name()));
    assert_eq!(left, ["0.lock"], "the slot and its symlink are removed");
    let entry = entry_of_scripts(&store);
    let stored = |name| fs::read_to_string(entry.join(name)).unwrap();
    let out_dir = format!("{TARGET}debug/build/foo/run/out");
    assert_eq!(stored("stdout"), format!("cargo:rustc-link-search=native={out_dir}/lib\n"));
    assert_eq!(stored("stderr"), format!("built in {out_dir}\n"));

    let (second, second_out) = baking_script(&fixture, "second", "", &[]);
    assert_eq!(replay_or_run(&real(&second_out), Ok(second), &store), ("hit".to_string(), 0));
    assert_eq!(fs::read(second_out.join("lib/libcrypto.a")).unwrap(), fs::read(&library).unwrap());
    let pc = fs::read_to_string(second_out.join("lib/openssl.pc")).unwrap();
    assert_eq!(pc, format!("prefix={}\n", second_out.display()));

    fs::remove_dir_all(&entry).unwrap();
    let (third, third_out) = baking_script(&fixture, "third", "", &[]);
    assert_eq!(replay_or_run(&real(&third_out), Ok(third), &store), ("miss".to_string(), 0));
    assert_eq!(runs(&fixture).len(), 3, "a marked key runs only at the fixed OUT_DIR");
}

#[test]
fn script_at_a_fixed_out_dir_keeps_its_symlinks_with_the_real_out_dir() {
    let fixture = fixture("script-fixed-links", "");
    let store = fixture.root.join("store");
    let extra = r#"ln -s lib/libcrypto.a "$OUT_DIR/libcrypto.a"
ln -s "$OUT_DIR/lib/openssl.pc" "$OUT_DIR/openssl.pc""#;
    let (first, out) = baking_script(&fixture, "first", extra, &[]);
    assert_eq!(replay_or_run(&real(&out), Ok(first), &store), ("miss".to_string(), 0));
    assert_eq!(runs(&fixture).len(), 2, "the script ran at the fixed OUT_DIR");
    assert_eq!(fs::read_link(out.join("libcrypto.a")).unwrap(), Path::new("lib/libcrypto.a"));
    assert_eq!(fs::read_link(out.join("openssl.pc")).unwrap(), out.join("lib/openssl.pc"));

    let (second, out) = baking_script(&fixture, "second", extra, &[]);
    assert_eq!(replay_or_run(&real(&out), Ok(second), &store), ("hit".to_string(), 0));
    assert_eq!(fs::read_link(out.join("openssl.pc")).unwrap(), out.join("lib/openssl.pc"));
    let pc = fs::read_to_string(out.join("openssl.pc")).unwrap();
    assert_eq!(pc, format!("prefix={}\n", out.display()));
}

fn entry_of_scripts(store: &Path) -> PathBuf {
    let mut entries =
        fs::read_dir(store.join("scripts")).unwrap().map(|entry| entry.unwrap().path());
    entries.find(|path| path.is_dir()).unwrap()
}

#[test]
fn script_that_escapes_or_fails_its_fixed_out_dir_keeps_its_normal_run_and_is_never_tried_again() {
    let fixture = fixture("script-fixed-refused", "");
    let store = fixture.root.join("store");
    let include = fixture.root.join("case-2/target/debug/build/dep/def/out");
    let target = fixture.root.join("case-1/target/debug").display().to_string();
    let store_path = store.display().to_string();
    let cases = [
        (format!("case $OUT_DIR in {store_path}*) exit 1;; esac"), "fixed-run-failed"),
        (format!("printf '\\377{target}' > \"$OUT_DIR/lib/real.a\""), "binary-target-path"),
        ("echo x >> \"$DEP_FOO_INCLUDE/libdep-def.rmeta\"".to_string(), "script-escaped"),
        ("printf x > \"$OUT_DIR/../../../escaped\"".to_string(), "script-escaped"),
    ];
    let include = include.display().to_string();
    for (case, (extra, reason)) in cases.iter().enumerate() {
        let name = format!("case-{case}");
        let variables = [("DEP_FOO_INCLUDE", include.as_str())];
        let (run, out) = baking_script(&fixture, &name, extra, &variables);
        let outcome = replay_or_run(&real(&out), Ok(run), &store);
        assert_eq!(outcome, (format!("passthrough {reason}"), 0), "{extra}");
        assert!(contains(&out.join("lib/libcrypto.a"), &out), "the normal run's outputs stay");
        assert_eq!(marked(&store, "never"), case + 1);
        assert_eq!(runs(&fixture).len(), 2 * case + 2);
    }
    let (again, out) =
        baking_script(&fixture, "case-3", &cases[3].0, &[("DEP_FOO_INCLUDE", &include)]);
    let outcome = replay_or_run(&real(&out), Ok(again), &store);
    assert_eq!(outcome, ("passthrough binary-target-path".to_string(), 0));
    assert_eq!(runs(&fixture).len(), 9, "a key marked never runs only in cargo's environment");
    assert_eq!(marked(&store, "fixed"), 0);
    assert!(!entry_of_scripts_exists(&store));
}

#[test]
fn package_whose_script_was_stored_from_a_fixed_out_dir_runs_another_key_only_there() {
    let fixture = fixture("script-fixed-package", "");
    let store = fixture.root.join("store");
    let package = ("CARGO_PKG_NAME", "openssl-src");
    let (first, out) = baking_script(&fixture, "first", "", &[package]);
    assert_eq!(replay_or_run(&real(&out), Ok(first), &store), ("miss".to_string(), 0));
    assert!(store.join("out/openssl-src.fixed-package").is_file());
    assert_eq!(runs(&fixture).len(), 2);

    let variables = [package, ("CARGO_FEATURE_STD", "1")];
    let (second, second_out) = baking_script(&fixture, "second", "", &variables);
    assert_eq!(replay_or_run(&real(&second_out), Ok(second), &store), ("miss".to_string(), 0));
    let ran = runs(&fixture);
    assert_eq!(ran.len(), 3, "another key of the package runs only at the fixed OUT_DIR");
    assert!(ran[2].starts_with(&store.join("out").display().to_string()), "{ran:?}");
    assert!(!contains(&second_out.join("lib/libcrypto.a"), &second_out));
    assert_eq!((marked(&store, "fixed"), marked(&store, "never")), (1, 0));
}

#[test]
fn key_whose_fixed_run_is_refused_under_a_package_marker_runs_in_cargos_environment() {
    let fixture = fixture("script-fixed-package-refused", "");
    let store = fixture.root.join("store");
    fs::create_dir_all(store.join("out")).unwrap();
    fs::write(store.join("out/foo-sys.fixed-package"), "").unwrap();
    let body = format!(
        "echo \"$OUT_DIR\" >> {}\ncase $OUT_DIR in {}*) exit 1;; esac\necho x > \"$OUT_DIR/x\"",
        fixture.root.join("runs").display(),
        store.display()
    );
    let variables = [("CARGO_PKG_NAME", "foo-sys")];
    let (run, out) = fixture.script("first", &body, &variables);
    assert_eq!(replay_or_run(&real(&out), Ok(run), &store), ("miss".to_string(), 0));
    let ran = runs(&fixture);
    assert!(ran[0].starts_with(&store.display().to_string()), "{ran:?}");
    assert_eq!(ran[1..], [out.display().to_string()], "the script runs in cargo's environment");
    assert_eq!(fs::read_to_string(out.join("x")).unwrap(), "x\n");
    assert_eq!(marked(&store, "never"), 1, "the refused fixed run marks its key");
    assert!(entry_of_scripts_exists(&store), "the run in cargo's environment is stored");

    fs::remove_dir_all(entry_of_scripts(&store)).unwrap();
    let (again, out) = fixture.script("second", &body, &variables);
    assert_eq!(replay_or_run(&real(&out), Ok(again), &store), ("miss".to_string(), 0));
    assert_eq!(runs(&fixture).len(), 3, "the key's never wins over the package marker");
}

#[test]
fn script_of_a_links_package_that_bakes_out_dir_runs_once_at_a_fixed_out_dir() {
    let fixture = fixture("script-links-fixed", "");
    let store = fixture.root.join("store");
    let variables = [("CARGO_PKG_NAME", "openssl-sys"), ("CARGO_MANIFEST_LINKS", "openssl")];
    let (first, out) = baking_script(&fixture, "first", "", &variables);
    assert_eq!(replay_or_run(&real(&out), Ok(first), &store), ("miss".to_string(), 0));
    let ran = runs(&fixture);
    assert_eq!(ran.len(), 1, "{ran:?}");
    assert!(ran[0].ends_with("/0/target/debug/build/foo/run/out"), "{ran:?}");
    assert!(!contains(&out.join("lib/libcrypto.a"), &fixture.root.join("first")));
    let pc = fs::read_to_string(out.join("lib/openssl.pc")).unwrap();
    assert_eq!(pc, format!("prefix={}\n", out.display()));
    assert_eq!((marked(&store, "fixed"), marked(&store, "never")), (1, 0));
    assert!(store.join("out/openssl-sys.fixed-package").is_file());
    let stdout = fs::read_to_string(entry_of_scripts(&store).join("stdout")).unwrap();
    assert_eq!(
        stdout,
        format!("cargo:rustc-link-search=native={TARGET}debug/build/foo/run/out/lib\n")
    );

    let (second, second_out) = baking_script(&fixture, "second", "", &variables);
    assert_eq!(replay_or_run(&real(&second_out), Ok(second), &store), ("hit".to_string(), 0));
    assert_eq!(runs(&fixture).len(), 1);
}

#[test]
fn script_of_a_links_package_whose_fixed_run_is_refused_runs_in_cargos_environment() {
    let fixture = fixture("script-links-refused", "");
    let store = fixture.root.join("store");
    let cases = [
        "printf x > \"$OUT_DIR/../../../escaped\"",
        // The run is adopted, but a file outside CARGO_HOME and OUT_DIR keeps it from being stored.
        "echo cargo:rerun-if-changed=/usr/include/zlib.h",
    ];
    for (case, extra) in cases.into_iter().enumerate() {
        let name = format!("case-{case}");
        let variables = [("CARGO_MANIFEST_LINKS", "foo")];
        let (run, out) =
            baking_script(&fixture, &name, &format!("{extra}\ntouch \"$OUT_DIR/x\""), &variables);
        let outcome = replay_or_run(&real(&out), Ok(run), &store);
        let reason = ["script-escaped", "unkeyed-input"][case];
        assert_eq!(outcome, (format!("passthrough {reason}"), 0), "{extra}");
        let ran = runs(&fixture);
        assert!(ran[2 * case].starts_with(&store.join("out").display().to_string()), "{ran:?}");
        assert_eq!(ran[2 * case + 1], out.display().to_string(), "{ran:?}");
        assert!(contains(&out.join("lib/libcrypto.a"), &out), "cargo gets the normal run");
        assert!(out.join("x").is_file());
        assert_eq!(marked(&store, "never"), case + 1);
    }
    assert_eq!(marked(&store, "fixed"), 0);
    assert!(!entry_of_scripts_exists(&store));
}

#[test]
fn script_of_a_links_package_that_bakes_nothing_stores_what_its_normal_run_would() {
    let fixture = fixture("script-links-equal", "");
    let store = |name: &str| fixture.root.join(name).join("store");
    let body = r#"mkdir -p "$OUT_DIR/lib" "$OUT_DIR/include"
printf '\377archive' > "$OUT_DIR/lib/libz.a"
printf '#define Z 1\n' > "$OUT_DIR/include/zlib.h"
printf 'include!("%s/data.rs");' "$OUT_DIR" > "$OUT_DIR/generated.rs"
echo "cargo:rustc-link-search=native=$OUT_DIR/lib"
echo "cargo:include=$OUT_DIR/include"
echo "built in $OUT_DIR" >&2"#;
    let mut entries = Vec::new();
    for (name, variables) in [("normal", &[][..]), ("links", &[("CARGO_MANIFEST_LINKS", "z")][..])]
    {
        let (run, out) = fixture.script(name, "", variables);
        let runs = fixture.root.join(name).join("runs");
        executable(&real(&out), &format!("#!/bin/sh\necho ran >> {}\n{body}\n", runs.display()));
        assert_eq!(replay_or_run(&real(&out), Ok(run), &store(name)), ("miss".to_string(), 0));
        assert_eq!(fs::read_to_string(runs).unwrap(), "ran\n", "{name}");
        let entry = entry_of_scripts(&store(name));
        let tree = store::tree(&entry).unwrap().into_iter().map(|(path, _)| path);
        entries.push(Vec::from_iter(tree.map(|path| (fs::read(entry.join(&path)).ok(), path))));
    }
    assert_eq!(marked(&store("normal"), "fixed"), 0);
    assert_eq!(marked(&store("links"), "fixed"), 1);
    assert_eq!(entries[0], entries[1]);
}

#[test]
fn script_of_a_package_without_links_runs_in_cargos_environment_first() {
    let fixture = fixture("script-no-links", "");
    let store = fixture.root.join("store");
    let (run, out) = baking_script(&fixture, "first", "", &[("CARGO_PKG_NAME", "foo")]);
    assert_eq!(replay_or_run(&real(&out), Ok(run), &store), ("miss".to_string(), 0));
    let ran = runs(&fixture);
    assert_eq!(ran[0], out.display().to_string(), "{ran:?}");
    assert!(ran[1].starts_with(&store.join("out").display().to_string()), "{ran:?}");
}

fn entry_of_scripts_exists(store: &Path) -> bool {
    fs::read_dir(store.join("scripts")).unwrap().any(|entry| entry.unwrap().path().is_dir())
}

#[test]
fn stand_in_runs_the_real_script_where_a_run_cannot_be_keyed() {
    let fixture = fixture("script-fallback", "");
    let store = fixture.root.join("store");
    let (run, out) = fixture.script("only", "", &[("RUSTC", "/nonexistent/rustc")]);
    // The real script runs with the environment of this process, in which OUT_DIR is unset.
    let runs = fixture.root.join("runs");
    executable(&real(&out), &format!("#!/bin/sh\necho ran >> {}\n", runs.display()));
    let outcome = replay_or_run(&real(&out), Ok(run), &store);
    assert_eq!(outcome, ("passthrough unreadable-input".to_string(), 0));
    let elsewhere = BTreeMap::from([("OUT_DIR".to_string(), "/nonexistent/out".to_string())]);
    let run = Run::model(&real(&out), fixture.root.clone(), elsewhere, fixture.cargo_home.clone());
    assert_eq!(
        replay_or_run(&real(&out), run, &store),
        ("passthrough no-target-dir".to_string(), 0)
    );
    assert_eq!(fs::read_to_string(runs).unwrap(), "ran\nran\n");
}

#[test]
fn install_moves_the_real_script_aside_unless_the_store_fails() {
    let fixture = fixture("install", "");
    let script = fixture.root.join("build_script_build");
    fs::write(&script, "real").unwrap();
    assert!(script::install(&script, Path::new("/dev/null/store"), Files::Clones).is_err());
    assert_eq!(fs::read_to_string(&script).unwrap(), "real");
    assert!(!script.with_extension("real").exists());

    script::install(&script, &fixture.root.join("store"), Files::Clones).unwrap();
    assert_eq!(fs::read_to_string(script.with_extension("real")).unwrap(), "real");
    let wrapper = fs::metadata(env::current_exe().unwrap()).unwrap();
    assert_eq!(fs::metadata(&script).unwrap().len(), wrapper.len());
}

#[test]
fn install_that_fails_is_logged_as_a_passthrough_of_the_script() {
    let fixture = fixture("install-logged", "");
    let (script, store) = (fixture.root.join("build_script_build"), fixture.root.join("store"));
    fs::write(&script, "real").unwrap();
    fs::write(store.join("stand-in"), "not a directory").unwrap();
    assert!(script::install(&script, &store, Files::Clones).is_err());
    assert_eq!(fs::read_to_string(&script).unwrap(), "real");
    let log = fs::read_to_string(store.join("log")).unwrap();
    assert!(log.contains("/build-script ") && log.ends_with(" passthrough no-stand-in\n"), "{log}");
}

// gc runs while builds install stand-ins, and a copy carries the old mtime of the wrapper where it is a clone.
#[test]
fn gc_keeps_a_staging_copy_of_the_stand_in_that_may_still_be_written() {
    let fixture = fixture("gc-stand-in", "");
    let store = fixture.root.join("store");
    let script = fixture.root.join("build_script_build");
    fs::write(&script, "real").unwrap();
    script::install(&script, &store, Files::Clones).unwrap();
    let (staging, other) = (store.join("stand-in/other.123"), store.join("stand-in/other"));
    for path in [&staging, &other] {
        fs::write(path, "a wrapper").unwrap();
        age(path, 30 * 86400);
    }
    gc::collect(&store, crate::now() - 3 * 86400, u64::MAX, false, || (100, 100));
    assert!(staging.exists(), "a staging copy whose inode is new stays");
    assert!(!other.exists(), "the copy of another wrapper goes");
    assert_eq!(fs::read_dir(store.join("stand-in")).unwrap().count(), 2);
}

/// Writes an entry of the store at `root` whose record is `fields`, followed by the arguments of `foo` and `extra`.
fn recorded(root: &Path, fields: &str, extra: &str) {
    let hash = blake3::hash(format!("{fields}{extra}").as_bytes()).to_hex();
    let args = format!(
        "--crate-name\nfoo\n{CARGO_HOME}registry/src/index/foo/src/lib.rs\n-C\nopt-level=0\n-C\nmetadata={hash}\n\
         --extern\ndep={TARGET}debug/deps/libdep-{hash}.rmeta\n{extra}"
    );
    fs::create_dir_all(root.join("entries").join(hash.as_str())).unwrap();
    fs::write(
        root.join("entries").join(hash.as_str()).join(store::RECORD),
        format!("{fields}\n{args}"),
    )
    .unwrap();
}

#[test]
fn duplicates_are_reported_only_between_workspaces_and_for_features_or_compatible_versions() {
    let root = fixture("duplicates", "").root.join("store");
    fs::write(root.join("log"), "100 foo 2000 miss\n100 foo 500 hit\n100 foo 800 hit waited\n")
        .unwrap();
    let before = stats(&[], &root).unwrap();
    let rows = [
        "      1       0.5 s  hit",
        "      1       0.8 s  hit waited",
        "      1       2.0 s  miss",
    ];
    let counts = format!("{}\n66.7% of the cached invocations hit\ncap: ", rows.join("\n"));
    assert!(before.starts_with(&counts), "{before}");
    let features = "--cfg\nfeature=\"std\"\n";
    recorded(&root, "foo\n1.0.0\nengine/target\n3000", features);
    recorded(
        &root,
        "foo\n1.0.0\nmarket/target\n2000",
        &format!("{features}--cfg\nfeature=\"alloc\"\n"),
    );
    recorded(&root, "foo\n1.0.0\nmarket/target\n900", "-C\nopt-level=3\n");
    recorded(&root, "foo\n1.0.0\nmarket/target\n800", "--cfg\nfeature=\"serde\"\n--test\n");
    recorded(&root, "foo\n1.0.0\nmarket/target\n700", "--cfg\nfeature=\"derive\"\n--test\n");
    recorded(&root, "foo-cli\n0.3.1\nengine/target\n400", "");
    recorded(&root, "foo-cli\n0.4.0\nmarket/target\n300", "");
    recorded(&root, "foo-cli\n0.4.2\nengine/target\n200", "");
    let report = stats(&["--duplicates".to_string()], &root).unwrap();
    let rows = [
        "8.3 s compiled, of which 2.2 s for other features or versions",
        "     2.0 s  foo 1.0.0  features alloc  in engine/target, market/target",
        "     0.2 s  foo-cli foo 0.4  versions 0.4.0, 0.4.2  in engine/target, market/target",
    ];
    assert_eq!(report, format!("{}\n", rows.join("\n")));
    assert_eq!(stats(&[], &root).unwrap(), before);
    assert_eq!(
        stats(&["--duplicates".to_string(), "--since".to_string(), "1d".to_string()], &root)
            .unwrap(),
        report
    );
    assert!(stats(&["--since".to_string()], &root).is_none());
}

#[test]
fn concurrent_compiles_of_one_key_run_rustc_once() {
    let fixture = slow("concurrent");
    let store = fixture.root.join("store");
    let (first, second) =
        (fixture.worktree("first", "metadata"), fixture.worktree("second", "metadata"));
    let out = |target: &Path| target.join("debug/build/foo/abc/out");
    let build = |target: &Path| fixture.build(&fixture.args(target), &store, None);
    let outcomes = thread::scope(|scope| {
        let compiler = scope.spawn(|| build(&first));
        wait_for(&out(&first).join("runs"));
        let waiter = build(&second);
        (compiler.join().unwrap(), waiter)
    });
    assert_eq!(outcomes, (Ok(("miss".to_string(), 0)), Ok(("hit waited".to_string(), 0))));
    assert!(!out(&second).join("runs").exists(), "rustc must run once");
    let rlib = |target: &Path| fs::read(out(target).join("libfoo-abc.rlib")).unwrap();
    assert_eq!(rlib(&first), rlib(&second));
}

// The kernel releases the lock of a process that dies, so a killed build does not make the next one wait.
#[test]
fn lock_of_a_killed_process_does_not_delay_the_compile() {
    let fixture = fixture("killed", "");
    let (store, target) = (fixture.root.join("store"), fixture.worktree("only", "metadata"));
    let key = fixture.key(&fixture.args(&target));
    let holder = store::lock(&store, &key, "foo").unwrap();
    let mut holder = Command::new("sleep").arg("60").stdin(holder).spawn().unwrap();
    let file = fs::File::open(store::shard(&store.join("locks"), &key)).unwrap();
    assert!(file.try_lock().is_err(), "the child process shares the lock");
    holder.kill().and_then(|()| holder.wait()).unwrap();
    let started = Instant::now();
    let outcome = fixture.build(&fixture.args(&target), &store, None);
    assert_eq!(outcome, Ok(("miss".to_string(), 0)));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn waiter_compiles_once_its_patience_runs_out() {
    let fixture = fixture("patience", "");
    let (store, target) = (fixture.root.join("store"), fixture.worktree("only", "metadata"));
    let _lock = store::lock(&store, &fixture.key(&fixture.args(&target)), "foo").unwrap();
    let started = Instant::now();
    let outcome = fixture.build(&fixture.args(&target), &store, None);
    assert_eq!(outcome, Ok(("miss".to_string(), 0)));
    assert!(started.elapsed() >= Duration::from_secs(3));
}

#[test]
fn concurrent_runs_of_one_script_key_run_it_once() {
    let fixture = fixture("script-concurrent", "");
    let store = fixture.root.join("store");
    let (first, first_out) = fixture.script("first", "sleep 1", &[]);
    let (second, second_out) = fixture.script("second", "sleep 1", &[]);
    let outcomes = thread::scope(|scope| {
        let runner = scope.spawn(|| replay_or_run(&real(&first_out), Ok(first), &store));
        wait_for(&first_out.join("../runs"));
        let waiter = replay_or_run(&real(&second_out), Ok(second), &store);
        (runner.join().unwrap(), waiter)
    });
    assert_eq!(outcomes, (("miss".to_string(), 0), ("hit waited".to_string(), 0)));
    assert!(!second_out.join("../runs").exists(), "the script must run once");
}

fn age(path: &Path, seconds: u64) {
    let file = fs::File::open(path).unwrap();
    file.set_modified(SystemTime::now() - Duration::from_secs(seconds)).unwrap();
}

#[test]
fn gc_removes_what_no_build_used_for_three_days_and_keeps_a_hit_entry() {
    let fixture = fixture("gc", "");
    let (store, entries) = (fixture.root.join("store"), fixture.root.join("store/entries"));
    let (first, second) =
        (fixture.worktree("first", "metadata"), fixture.worktree("second", "metadata"));
    let build = |target: &Path| fixture.build(&fixture.args(target), &store, None);
    assert_eq!(build(&first), Ok(("miss".to_string(), 0)));
    let hit = fs::read_dir(&entries).unwrap().next().unwrap().unwrap().path();
    age(&hit, 30 * 86400);
    assert_eq!(build(&second), Ok(("hit".to_string(), 0)));
    let (unused, orphan, used) =
        (entries.join("unused"), entries.join("orphan.reads"), entries.join("used.reads"));
    let (staging, fresh) = (entries.join("unused.123"), entries.join("unused.456"));
    for directory in [&unused, &staging, &fresh] {
        fs::create_dir_all(directory.join("lib")).unwrap();
        fs::write(directory.join("lib/libfoo.rlib"), "rlib").unwrap();
    }
    std::os::unix::fs::symlink(&fixture.root, unused.join("root")).unwrap();
    fs::write(&orphan, "env HOME").and_then(|()| fs::write(&used, "env HOME")).unwrap();
    let ages = [(&unused, 4 * 86400), (&orphan, 4 * 86400), (&used, 86400), (&staging, 7200)];
    for (path, seconds) in ages.into_iter().chain([(&fresh, 60)]) {
        age(path, seconds);
    }
    let lock = |name: &str| store::shard(&store.join("locks"), name);
    for name in ["old", "held", "fresh"] {
        stored(&store, lock(name).strip_prefix(&store).unwrap().to_str().unwrap(), 0, 0);
        age(&lock(name), if name == "fresh" { 60 } else { 4 * 86400 });
    }
    let flat = stored(&store, "locks/flat", 0, 4 * 86400);
    let held = fs::File::open(lock("held")).unwrap();
    held.try_lock().unwrap();
    fs::write(fixture.root.join("build_script_build"), "real").unwrap();
    script::install(&fixture.root.join("build_script_build"), &store, Files::Clones).unwrap();
    fs::write(store.join("stand-in/other"), "an older wrapper").unwrap();
    fs::write(store.join("log"), format!("100 foo 2000 miss\n{} foo 1 hit\n", crate::now()))
        .unwrap();

    let collected = gc::collect(&store, crate::now() - 3 * 86400, u64::MAX, false, || (100, 100));
    assert_eq!(collected.entries, 1);
    let mut kept =
        Vec::from_iter(fs::read_dir(&entries).unwrap().map(|entry| entry.unwrap().file_name()));
    kept.sort();
    assert_eq!(kept, [hit.file_name().unwrap(), "unused.456".as_ref(), "used.reads".as_ref()]);
    assert!(fixture.root.join("rustc").is_file(), "removal must not follow a symlink");
    assert_eq!(fs::read_dir(store.join("stand-in")).unwrap().count(), 1);
    let locks = ["old", "held", "fresh"].map(|name| lock(name).exists());
    assert_eq!(locks, [false, true, true], "a held lock stays");
    assert!(!flat.exists(), "a lock from before the shards ages out");
    let report = stats(&[], &store).unwrap();
    let (counts, cap) = report.split_at(report.find("cap: ").unwrap());
    assert_eq!(counts, "      1       0.0 s  hit\n100.0% of the cached invocations hit\n");
    assert!(
        cap.ends_with(" GB (from the filesystem)\n") || cap.contains(gc::SIZE_VARIABLE),
        "{cap}"
    );
}

#[test]
fn gc_removes_the_fixed_out_dirs_and_markers_that_no_script_used_for_three_days() {
    let fixture = fixture("gc-out", "");
    let (store, pool) = (fixture.root.join("store"), fixture.root.join("store/out/key"));
    fs::create_dir_all(pool.join("0/target/debug")).unwrap();
    fs::create_dir_all(pool.join("1/target")).unwrap();
    for number in ["0", "1", "2"] {
        std::os::unix::fs::symlink(&fixture.root, pool.join(format!("{number}.target"))).unwrap();
        fs::write(pool.join(format!("{number}.lock")), "").unwrap();
        age(&pool.join(format!("{number}.lock")), if number == "2" { 60 } else { 4 * 86400 });
    }
    let held = fs::File::open(pool.join("1.lock")).unwrap();
    held.try_lock().unwrap();
    for (name, old) in [("old.fixed", true), ("old.never", true), ("new.never", false)]
        .into_iter()
        .chain([("old.fixed-package", true), ("new.fixed-package", false)])
        .chain([("-.old.library.baked", true), ("-.new.linked.baked", false)])
    {
        fs::write(store.join("out").join(name), "").unwrap();
        age(&store.join("out").join(name), if old { 4 * 86400 } else { 60 });
    }
    gc::collect(&store, crate::now() - 3 * 86400, u64::MAX, false, || (100, 100));
    let names = |directory: &Path| {
        let mut names = Vec::from_iter(
            fs::read_dir(directory)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap()),
        );
        names.sort();
        names
    };
    assert_eq!(names(&pool), ["1", "1.lock", "1.target", "2.lock", "2.target"]);
    let kept = ["-.new.linked.baked", "key", "new.fixed-package", "new.never"];
    assert_eq!(names(&store.join("out")), kept);
    assert!(fixture.root.join("rustc").is_file(), "removal must not follow a symlink");
    drop(held);
    fs::remove_dir_all(&pool).unwrap();
    gc::collect(&store, crate::now() - 3 * 86400, u64::MAX, false, || (100, 100));
    let kept = ["-.new.linked.baked", "new.fixed-package", "new.never"];
    assert_eq!(names(&store.join("out")), kept, "an empty pool is removed");
}

#[test]
fn gc_removes_the_least_recently_used_over_the_cap_or_the_free_space_however_recently_used() {
    let fixture = fixture("gc-cap", "");
    let store = fixture.root.join("store");
    let entry = |name: &str, seconds| {
        let entry = store.join("entries").join(name);
        fs::create_dir_all(&entry).and_then(|()| fs::write(entry.join("lib"), [0; 4096])).unwrap();
        age(&entry, seconds);
    };
    entry("oldest", 3 * 3600);
    entry("older", 2 * 3600);
    entry("recent", 1800);
    let size = gc::collect(&store, 0, u64::MAX, false, || (10_000, 10_000)).size;
    let collected = gc::collect(&store, 0, size - 1, false, || (10_000, 10_000));
    assert_eq!(
        collected,
        gc::Collected { entries: 1, slots: 0, freed: 4096, size: size - 4096, next: 0 }
    );
    assert!(!store.join("entries/oldest").exists() && store.join("entries/older").exists());
    let collected = gc::collect(&store, 0, 0, false, || (10_000, 10_000));
    assert_eq!((collected.entries, collected.size), (2, size - 3 * 4096));
    assert!(!store.join("entries/recent").exists(), "an entry used half an hour ago");

    entry("unused", 2 * 3600);
    entry("recent", 1800);
    let size = gc::collect(&store, 0, u64::MAX, false, || (100_000, 100_000)).size;
    // The free space is below a tenth of the filesystem until the first removal.
    let calls = std::cell::Cell::new(0);
    let space = || {
        calls.set(calls.get() + 1);
        (100_000, if calls.get() == 1 { 9_000 } else { 20_000 })
    };
    assert_eq!(gc::collect(&store, 0, size + 1, false, space).entries, 1);
    assert!(!store.join("entries/unused").exists() && store.join("entries/recent").exists());

    // A cap that the variable sets keeps the floor of free space.
    entry("overridden", 2 * 3600);
    let (cap, _) = gc::cap(100, Some("1T"));
    assert_eq!(gc::collect(&store, 0, cap, false, || (100, 9)).entries, 2);
    assert!(!store.join("entries/overridden").exists());
    entry("kept", 2 * 3600);
    let space = || (100, 20);
    assert_eq!(
        gc::collect(&store, 0, cap, false, space).entries,
        0,
        "a tenth and the margin are free"
    );
    assert!(store.join("entries/kept").exists());
}

#[test]
fn cap_is_a_tenth_of_the_filesystem_between_20_and_100_gib() {
    const GIB: u64 = 1 << 30;
    let cap = |total| gc::cap(total, None);
    assert_eq!(cap(0), (16 * GIB, false));
    assert_eq!(cap(80_000_000_000), (16 * GIB, false));
    assert_eq!(cap(200 * GIB), (20 * GIB, false));
    assert_eq!(cap(256_000_000_000), (25_600_000_000, false));
    assert_eq!(cap(512_000_000_000), (51_200_000_000, false));
    assert_eq!(cap(1000 * GIB), (100 * GIB, false));
    assert_eq!(cap(2_000_000_000_000), (100 * GIB, false));
}

#[test]
fn size_variable_replaces_the_cap_unless_it_is_not_a_size() {
    const GIB: u64 = 1 << 30;
    let cap = |setting| gc::cap(512_000_000_000, Some(setting));
    assert_eq!(cap("500000000"), (500_000_000, true));
    assert_eq!(cap("64k"), (64 << 10, true));
    assert_eq!(cap("5M"), (5 << 20, true));
    assert_eq!(cap("200G"), (200 * GIB, true));
    assert_eq!(cap("200g"), (200 * GIB, true));
    assert_eq!(cap("1.5T"), (1536 * GIB, true));
    assert_eq!(cap("3t"), (3072 * GIB, true), "above the computed maximum");
    for invalid in ["", "lots", "200GB", "1.5X", "-5G", "0", "G", "inf", "NaN"] {
        assert_eq!(cap(invalid), (51_200_000_000, false), "{invalid}");
    }
}

/// Whether [`gc::trigger`] starts a run in `store` within 10 s.
///
/// A child that another test forks shares the lock of the trigger until it runs exec, so a trigger may find it held.
fn triggered(store: &Path) -> bool {
    let waited = Instant::now();
    while waited.elapsed() < Duration::from_secs(10) {
        if gc::trigger(store, Path::new("/usr/bin/true")) {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn trigger_starts_gc_within_the_hour_once_the_store_gains_more_than_a_gib() {
    let fixture = fixture("gc-added", "");
    let store = fixture.root.join("store");
    assert!(triggered(&store));
    gc::add_bytes(&store, 700 << 20);
    gc::add_bytes(&store, 324 << 20);
    assert!(!gc::trigger(&store, Path::new("/usr/bin/true")), "exactly a GiB");
    gc::add_bytes(&store, 1);
    assert!(triggered(&store));
    assert!(!store.join("gc.added").exists());
    assert!(!gc::trigger(&store, Path::new("/usr/bin/true")), "the count starts again");

    // A store gains the size of each entry that a build stores.
    let entry = store.join("entries/key-variant");
    fs::create_dir_all(&entry).and_then(|()| fs::write(entry.join("output"), [0; 1000])).unwrap();
    gc::add(&store, &entry);
    assert_eq!(fs::read_to_string(store.join("gc.added")).unwrap(), "1000\n");
}

#[test]
fn trigger_starts_at_most_one_gc_an_hour_and_none_while_one_runs() {
    let fixture = fixture("gc-trigger", "");
    let store = fixture.root.join("store");
    assert!(triggered(&store));
    assert!(!gc::trigger(&store, Path::new("/usr/bin/true")));
    age(&store.join("gc.stamp"), 7200);
    assert!(triggered(&store));
    // A miss that ends while another decides leaves gc to that one.
    age(&store.join("gc.stamp"), 7200);
    let deciding = fs::File::create(store.join("gc.trigger")).unwrap();
    deciding.try_lock().unwrap();
    assert!(!gc::trigger(&store, Path::new("/usr/bin/true")));
    drop(deciding);
    assert!(triggered(&store));

    let lock = fs::File::create(store.join("gc.lock")).unwrap();
    lock.try_lock().unwrap();
    let started = Instant::now();
    let report = thread::scope(|scope| {
        scope.spawn(|| {
            thread::sleep(Duration::from_millis(300));
            drop(lock);
        });
        assert!(
            !gc::trigger(&store, Path::new("/usr/bin/true")),
            "a miss during a run starts no other"
        );
        assert_eq!(
            gc::run(&store, false, false),
            "",
            "a run that a miss started leaves it to the first"
        );
        gc::run(&store, false, true)
    });
    assert!(started.elapsed() >= Duration::from_millis(300), "a second gc waits for the first");
    assert!(report.starts_with("removed 0 entries and 0 slots"), "{report}");
}

/// Writes `bytes` zeros to `path` under `store`, in new directories where needed, and ages it by `seconds`.
/// The memos in the shards of `directory`, each of which holds no other directory.
fn memos_in(directory: &Path) -> Vec<PathBuf> {
    let shards = fs::read_dir(directory).unwrap().map(|shard| shard.unwrap().path());
    let shards = Vec::from_iter(shards.inspect(|shard| assert!(shard.is_dir(), "{shard:?}")));
    let memos = shards.iter().flat_map(|shard| fs::read_dir(shard).unwrap());
    Vec::from_iter(memos.map(|memo| memo.unwrap().path()))
}

fn stored(store: &Path, path: &str, bytes: usize, seconds: u64) -> PathBuf {
    let path = store.join(path);
    fs::create_dir_all(path.parent().unwrap())
        .and_then(|()| fs::write(&path, vec![0; bytes]))
        .unwrap();
    age(&path, seconds);
    path
}

#[test]
fn gc_over_its_cap_removes_recent_entries_and_idle_slots_least_recently_used_first_but_no_held_slot()
 {
    let fixture = fixture("gc-recent", "");
    let (store, pool) = (fixture.root.join("store"), fixture.root.join("store/slots/foo"));
    let entry = |name: &str, seconds| {
        stored(&store, &format!("entries/{name}/lib"), 4096, 0);
        age(&store.join("entries").join(name), seconds);
    };
    entry("first", 50 * 60);
    entry("third", 20 * 60);
    for (number, minutes) in [("0", 40), ("1", 10), ("2", 5)] {
        stored(&store, &format!("slots/foo/{number}.slot/lib"), 4096, 0);
        stored(&store, &format!("slots/foo/{number}.lock"), 0, minutes * 60);
    }
    let held = fs::File::open(pool.join("1.lock")).unwrap();
    held.try_lock().unwrap();

    let size = gc::collect(&store, 0, u64::MAX, false, || (100_000, 100_000)).size;
    // A cap whose target, a tenth below it, needs two items gone.
    let cap = (0..size).find(|cap| (4097..=8192).contains(&(size - (cap - cap / 10)))).unwrap();
    let collected = gc::collect(&store, 0, cap, false, || (100_000, 100_000));
    let expected = gc::Collected { entries: 1, slots: 1, freed: 8192, size: size - 8192, next: 0 };
    assert_eq!(collected, expected, "the entry used 50 minutes ago, then the slot used 40");
    assert!(!store.join("entries/first").exists() && !pool.join("0.slot").exists());
    assert!(store.join("entries/third").exists());

    let collected = gc::collect(&store, 0, 0, false, || (100_000, 100_000));
    assert_eq!((collected.entries, collected.slots, collected.freed), (1, 1, 8192));
    assert_eq!(collected.size, size - 4 * 4096, "the held slot still counts");
    assert!(pool.join("1.slot").exists() && pool.join("1.lock").exists(), "a held slot stays");
    assert!(!store.join("entries/third").exists() && !pool.join("2.slot").exists());
    assert!(collected.next >= crate::now() + 3599, "the held slot keeps the store over its cap");
}

#[test]
fn gc_removes_staging_left_by_a_killed_process_only_once_it_is_an_hour_old() {
    let fixture = fixture("gc-staging", "");
    let store = fixture.root.join("store");
    for (name, seconds) in [("young", 50 * 60), ("old", 3601)] {
        for path in [format!("entries/{name}.123"), format!("slots/foo/0.{name}.123")] {
            stored(&store, &format!("{path}/lib"), 1000, 0);
            age(&store.join(&path), seconds);
        }
    }
    gc::collect(&store, crate::now() - 3 * 86400, 0, false, || (100_000, 100_000));
    assert!(
        store.join("entries/young.123").exists() && store.join("slots/foo/0.young.123").exists()
    );
    assert!(!store.join("entries/old.123").exists() && !store.join("slots/foo/0.old.123").exists());
}

#[test]
fn gc_counts_slots_under_the_cap_and_removes_the_least_recently_used_of_entries_and_slots() {
    let fixture = fixture("gc-slots", "");
    let (store, pool) = (fixture.root.join("store"), fixture.root.join("store/slots/foo"));
    stored(&store, "entries/old/lib", 4096, 5 * 3600);
    age(&store.join("entries/old"), 5 * 3600);
    for (number, hours) in [("0", 4), ("1", 2), ("2", 3), ("3", 0)] {
        stored(&store, &format!("slots/foo/{number}.slot/.trucker-libraries/lib"), 4096, 0);
        stored(&store, &format!("slots/foo/{number}.source"), 0, 0);
        stored(&store, &format!("slots/foo/{number}.lock"), 0, hours * 3600 + 60);
    }
    stored(&store, "slots/foo/0.session/foo-1x/s-1-2-3/query", 12288, 0);
    // A library that a slot links counts nothing while its worktree links it too.
    let library = stored(&fixture.root, "worktree/libbar.rlib", 4096, 0);
    fs::hard_link(&library, pool.join("1.slot/.trucker-libraries/libbar.rlib")).unwrap();
    // What a killed run of gc doomed goes at once, whatever its age.
    stored(&store, "entries/doomed.gc/lib", 1000, 0);
    stored(&store, "slots/foo/3.session.gc/query", 1000, 0);
    let held = fs::File::open(pool.join("2.lock")).unwrap();
    held.try_lock().unwrap();

    let size = gc::collect(&store, 0, u64::MAX, false, || (10_000, 10_000)).size;
    assert!(!store.join("entries/doomed.gc").exists() && !pool.join("3.session.gc").exists());
    let collected = gc::collect(&store, 0, size - 4500, false, || (10_000, 10_000));
    let expected =
        gc::Collected { entries: 1, slots: 1, freed: 20480, size: size - 20480, next: 0 };
    assert_eq!(collected, expected, "the oldest entry, then the oldest slot");
    assert!(!store.join("entries/old").exists());
    for part in ["0.slot", "0.session", "0.source", "0.lock"] {
        assert!(!pool.join(part).exists(), "{part}");
    }
    let collected = gc::collect(&store, 0, 0, false, || (10_000, 10_000));
    assert_eq!((collected.slots, collected.freed), (2, 8192), "a held slot stays");
    assert_eq!(collected.size, size - 20480 - 8192, "the linked library counts nothing");
    assert!(pool.join("2.slot").exists() && !pool.join("1.slot").exists() && library.exists());
    assert!(!pool.join("3.slot").exists(), "a slot used in the last minute goes too");
    drop(held);
    let collected = gc::collect(&store, 0, 0, false, || (10_000, 10_000));
    assert_eq!(collected.slots, 1);
    assert!(!pool.join("2.slot").exists());
}

#[test]
fn gc_counts_a_clone_that_a_worktree_holds_as_nothing_and_one_that_only_slots_hold_once() {
    let fixture = fixture("gc-clones", "");
    let store = fixture.root.join("store");
    let library = stored(&fixture.root, "worktree/liba.rlib", 8192, 0);
    for (pool, hours) in [("foo", 3), ("bar", 2), ("baz", 1)] {
        let copy = store.join(format!("slots/{pool}/0.slot/.trucker-libraries/liba.rlib"));
        fs::create_dir_all(copy.parent().unwrap()).unwrap();
        match pool {
            "baz" => fs::write(&copy, vec![0; 8192]).unwrap(),
            _ if store::clone(&library, &copy).is_ok() => {}
            _ => return,
        }
        stored(&store, &format!("slots/{pool}/0.lock"), 0, hours * 3600);
    }
    let collect = |cap| gc::collect(&store, 0, cap, false, || (100_000, 100_000));
    let size = collect(u64::MAX).size;
    fs::remove_file(&library).unwrap();
    assert_eq!(collect(u64::MAX).size, size, "a measure holds for an hour");
    // A run measures again an eighth of what was measured over an hour before, which is one slot of three.
    let sizes = fs::read_to_string(store.join("gc.sizes")).unwrap();
    let aged = sizes.lines().map(|line| {
        let mut fields = Vec::from_iter(line.split('\t'));
        if fields.len() > 3 {
            fields[3] = "0";
        }
        fields.join("\t") + "\n"
    });
    fs::write(store.join("gc.sizes"), String::from_iter(aged)).unwrap();
    // On XFS and btrfs the two clones in slots counted once from the start, as FIEMAP does not count holders.
    let counted = if cfg!(target_os = "macos") { 8192 } else { 0 };
    assert_eq!(collect(u64::MAX).size, size + counted, "the newest measure counts the clones");
    let held = ["bar", "baz"]
        .map(|pool| fs::File::open(store.join(format!("slots/{pool}/0.lock"))).unwrap());
    held.iter().for_each(|lock| lock.try_lock().unwrap());
    let collected = collect(0);
    assert_eq!((collected.slots, collected.freed), (1, 0), "another slot still holds the clone");
}

#[test]
fn gc_removes_an_entry_that_no_build_hit_for_an_hour_before_what_builds_used_earlier() {
    let fixture = fixture("gc-unhit", "");
    let store = fixture.root.join("store");
    // A stored entry that no build hit has an mtime within a second of its birth, as one aged here does.
    stored(&store, "entries/unhit/lib", 4096, 0);
    age(&store.join("entries/unhit"), 2 * 3600);
    stored(&store, "slots/foo/0.slot/lib", 4096, 0);
    stored(&store, "slots/foo/0.lock", 0, 3 * 3600);
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::FileTimesExt;
        stored(&store, "entries/hit/lib", 4096, 0);
        let born =
            fs::FileTimes::new().set_created(SystemTime::now() - Duration::from_secs(5 * 3600));
        let used = born.set_modified(SystemTime::now() - Duration::from_secs(4 * 3600));
        fs::File::open(store.join("entries/hit")).unwrap().set_times(used).unwrap();
    }
    let size = gc::collect(&store, 0, u64::MAX, false, || (100_000, 100_000)).size;
    let cap = (0..size).find(|cap| (1..=4096).contains(&(size - (cap - cap / 10)))).unwrap();
    let collected = gc::collect(&store, 0, cap, false, || (100_000, 100_000));
    assert_eq!((collected.entries, collected.slots), (1, 0));
    assert!(!store.join("entries/unhit").exists() && store.join("slots/foo/0.slot").exists());
    #[cfg(target_os = "macos")]
    assert!(store.join("entries/hit").exists(), "an entry hit four hours ago");
}

#[test]
fn gc_does_not_prefer_an_entry_that_no_build_hit_but_that_is_younger_than_an_hour() {
    let fixture = fixture("gc-young", "");
    let store = fixture.root.join("store");
    stored(&store, "entries/young/lib", 4096, 0);
    age(&store.join("entries/young"), 10 * 60);
    stored(&store, "slots/foo/0.slot/lib", 4096, 0);
    stored(&store, "slots/foo/0.lock", 0, 30 * 60);
    let size = gc::collect(&store, 0, u64::MAX, false, || (100_000, 100_000)).size;
    let cap = (0..size).find(|cap| (1..=4096).contains(&(size - (cap - cap / 10)))).unwrap();
    let collected = gc::collect(&store, 0, cap, false, || (100_000, 100_000));
    assert_eq!((collected.entries, collected.slots), (0, 1), "the slot used 30 minutes ago");
    assert!(store.join("entries/young").exists());
}

#[test]
fn gc_orders_by_last_use_alone_where_the_filesystem_reports_no_birth_time() {
    let used = SystemTime::now() - Duration::from_secs(2 * 3600);
    assert!(gc::spent(Ok(used), Ok(used), crate::now()));
    let unknown = std::io::Error::from(std::io::ErrorKind::Unsupported);
    assert!(!gc::spent(Err(unknown), Ok(used), crate::now()));
}

#[test]
fn gc_of_all_removes_what_was_used_just_now_but_keeps_held_slots_staging_and_locks() {
    let fixture = fixture("gc-all", "");
    let (store, pool) = (fixture.root.join("store"), fixture.root.join("store/slots/foo"));
    stored(&store, "entries/recent/lib", 1000, 0);
    stored(&store, "entries/key.reads", 10, 0);
    stored(&store, "entries/staged.123/lib", 1000, 0);
    stored(&store, "hashes/ab/memo", 10, 0);
    stored(&store, "hashes/flat", 10, 0);
    stored(&store, "rustc.gc/ab/left", 10, 0);
    stored(&store, "out/key.never", 0, 0);
    stored(&store, "locks/inputs", 0, 0);
    for number in ["0", "1"] {
        stored(&store, &format!("slots/foo/{number}.slot/lib"), 1000, 0);
        stored(&store, &format!("slots/foo/{number}.lock"), 0, 0);
    }
    let held = fs::File::open(pool.join("1.lock")).unwrap();
    held.try_lock().unwrap();

    let collected = gc::collect(&store, 0, u64::MAX, true, || (10_000, 10_000));
    assert_eq!((collected.entries, collected.slots, collected.next), (1, 1, 0));
    let gone = ["entries/recent", "entries/key.reads", "hashes", "hashes.gc", "rustc.gc"];
    for gone in gone.into_iter().chain(["out/key.never", "slots/foo/0.slot"]) {
        assert!(!store.join(gone).exists(), "{gone}");
    }
    for kept in ["entries/staged.123", "locks/inputs", "slots/foo/1.slot"] {
        assert!(store.join(kept).exists(), "{kept}");
    }
}

#[test]
fn gc_measures_only_the_entries_and_slots_that_changed_since_its_last_run() {
    let fixture = fixture("gc-sizes", "");
    let store = fixture.root.join("store");
    let entry = stored(&store, "entries/entry/lib", 4096, 0);
    let idle = stored(&store, "slots/foo/0.slot/lib", 4096, 0);
    let busy = stored(&store, "slots/foo/1.slot/lib", 4096, 0);
    let held = stored(&store, "slots/foo/2.slot/lib", 4096, 0);
    let lock = stored(&store, "slots/foo/0.lock", 0, 7200);
    let busy_lock = stored(&store, "slots/foo/1.lock", 0, 60);
    let held_lock = fs::File::create(stored(&store, "slots/foo/2.lock", 0, 60)).unwrap();
    held_lock.try_lock().unwrap();
    let size = || gc::collect(&store, 0, u64::MAX, false, || (100, 100)).size;
    let first = size();
    for path in [&entry, &idle, &busy, &held] {
        fs::write(path, [0; 8192]).unwrap();
    }
    assert_eq!(size(), first + 4096, "only a slot that a compile holds is measured again");
    age(&busy_lock, 30);
    assert_eq!(
        size(),
        first + 8192,
        "a slot that a compile took in the last hour is measured again"
    );
    age(&lock, 7000);
    assert_eq!(size(), first + 12288, "a slot that a compile took is measured again");
    fs::remove_dir_all(entry.parent().unwrap()).unwrap();
    stored(&store, "entries/entry/lib", 12288, 0);
    assert_eq!(size(), first + 20480, "an entry stored again under its name is measured again");
}

#[test]
fn version_names_the_version_of_the_package_and_the_start_of_the_hash_of_the_binary() {
    let hash = unit::file_hash(&env::current_exe().unwrap()).unwrap().to_hex();
    let version = format!("trucker {} ({})\n", env!("CARGO_PKG_VERSION"), &hash[..12]);
    assert_eq!(crate::version(), version);
}

#[test]
fn gc_sweeps_the_memos_on_every_run_and_the_records_of_reads_once_a_day() {
    let fixture = fixture("gc-memos", "");
    let store = fixture.root.join("store");
    let collect = || gc::collect(&store, crate::now() - 3 * 86400, u64::MAX, false, || (100, 100));
    let kept = stored(&store, "hashes/ab/kept", 32, 3 * 3600);
    let reads = stored(&store, "entries/key.reads", 32, 4 * 86400);
    let size = collect().size;
    assert!(kept.exists() && !reads.exists());
    let old = stored(&store, "hashes/ab/old", 32, 5 * 3600);
    let flat = stored(&store, "hashes/flat", 32, 5 * 3600);
    let reads = stored(&store, "entries/key.reads", 32, 4 * 86400);
    assert_eq!(collect().size, size);
    assert!(!old.exists() && !flat.exists() && kept.exists(), "a memo goes at four hours");
    assert!(reads.exists(), "a sweep of the records of reads waits for a day");
    let sizes = fs::read_to_string(store.join("gc.sizes")).unwrap();
    let (records, items) = sizes.split_once('\n').unwrap();
    let bytes = records.rsplit('\t').next().unwrap();
    let swept = format!("records\t{}\t{bytes}\n{items}", crate::now() - 86400);
    fs::write(store.join("gc.sizes"), swept).unwrap();
    collect();
    assert!(!reads.exists());
}

#[test]
fn gc_counts_the_blocks_that_a_memo_takes_rather_than_its_length() {
    let fixture = fixture("gc-blocks", "");
    let store = fixture.root.join("store");
    let size = || gc::collect(&store, 0, u64::MAX, false, || (100, 100)).size;
    let empty = size();
    let memo = stored(&store, "hashes/ab/memo", 32, 0);
    let blocks = fs::metadata(&memo).unwrap().blocks() * 512;
    assert_eq!(size() - empty, blocks.max(32));
}

#[test]
fn gc_that_must_evict_evicts_a_tenth_of_the_cap_more_and_leaves_that_room_to_the_trigger() {
    let fixture = fixture("gc-margin", "");
    let store = fixture.root.join("store");
    for number in 0..20 {
        stored(&store, &format!("entries/{number}/lib"), 1000, 0);
        age(&store.join(format!("entries/{number}")), 7200 - number);
    }
    let size = gc::collect(&store, 0, u64::MAX, false, || (1 << 40, 1 << 40)).size;
    let cap = size - 1;
    let collected = gc::collect(&store, 0, cap, false, || (1 << 40, 1 << 40));
    assert_eq!(collected.entries, 2, "down to nine tenths of the cap");
    assert!(collected.size <= cap - cap / 100 * 10 && collected.next == 0);
    assert_eq!(gc::collect(&store, 0, cap, false, || (1 << 40, 1 << 40)).entries, 0);
    // Where the free space is short, a run frees a tenth of the cap beyond the floor, and asks again after each removal.
    let calls = std::cell::Cell::new(0);
    let space = || {
        calls.set(calls.get() + 1);
        (100_000, 8_999 + 1000 * calls.get())
    };
    assert_eq!(gc::collect(&store, 0, 100_000, false, space).entries, 11);

    fs::write(store.join("gc.stamp"), "0 5000").unwrap();
    gc::add_bytes(&store, 5000);
    assert!(!gc::trigger(&store, Path::new("/usr/bin/true")), "the room that the last run left");
    gc::add_bytes(&store, 1);
    assert!(triggered(&store));
}

// A store just below its cap once made each run remove nothing and leave the trigger a room of a few entries.
#[test]
fn gc_of_a_store_within_a_tenth_of_its_cap_evicts_down_to_that_tenth_below_it() {
    let fixture = fixture("gc-gap", "");
    let store = fixture.root.join("store");
    for number in 0..20 {
        stored(&store, &format!("entries/{number}/lib"), 1000, 0);
        age(&store.join(format!("entries/{number}")), 7200 - number);
    }
    let space = || (1 << 40, 1 << 40);
    let size = gc::collect(&store, 0, u64::MAX, false, space).size;
    assert_eq!(gc::collect(&store, 0, size * 2, false, space).entries, 0, "far below the cap");
    let cap = size + 1;
    let margin = cap / 100 * 10;
    let collected = gc::collect(&store, 0, cap, false, space);
    assert!(collected.entries > 0 && collected.next == 0);
    let room = cap - collected.size;
    assert!((margin..margin + 1000).contains(&room), "a room of {room} for a margin of {margin}");
    assert_eq!(gc::collect(&store, 0, cap, false, space).entries, 0, "a run inside the room");

    fs::write(store.join("gc.stamp"), format!("0 {room}")).unwrap();
    gc::add_bytes(&store, room);
    assert!(!gc::trigger(&store, Path::new("/usr/bin/true")), "the room that the run left");
    gc::add_bytes(&store, 1);
    assert!(triggered(&store));
}

#[test]
fn trigger_waits_for_the_next_run_after_one_that_left_the_store_over_its_bounds() {
    let fixture = fixture("gc-futile", "");
    let store = fixture.root.join("store");
    fs::write(store.join("gc.stamp"), (crate::now() + 100).to_string()).unwrap();
    gc::add_bytes(&store, 2 << 30);
    assert!(!gc::trigger(&store, Path::new("/usr/bin/true")));
    fs::write(store.join("gc.stamp"), (crate::now() - 1).to_string()).unwrap();
    assert!(triggered(&store));
}

#[test]
fn stats_gives_the_size_of_the_store_at_its_last_gc() {
    let fixture = fixture("gc-stats", "");
    let store = fixture.root.join("store");
    fs::write(store.join("log"), "100 - 5 gc 1 2 3000000000 4\n101 foo 1 hit\n").unwrap();
    let report = stats(&[], &store).unwrap();
    assert!(report.contains("last gc: 3.0 GB of a "), "{report}");
}

// A cap that cargo's `[env]` sets reaches the gc that a build starts, but not `trucker stats` in a shell.
#[test]
fn stats_gives_the_cap_that_the_last_gc_applied() {
    let fixture = fixture("gc-stats-cap", "");
    let store = fixture.root.join("store");
    fs::write(store.join("log"), "100 - 5 gc 1 2 3000000000 4 137438953472 1\n").unwrap();
    let report = stats(&[], &store).unwrap();
    let expected = format!("last gc: 3.0 GB of a 137.4 GB cap (from {})", gc::SIZE_VARIABLE);
    assert!(report.contains(&expected), "{report}");
}

#[test]
fn a_word_that_names_no_program_is_not_taken_for_rustc() {
    for word in ["status", "g", "--help", ""] {
        assert!(!is_program(word), "{word}");
    }
    assert!(is_program("sh") && is_program("/no/such/rustc") && is_program("./rustc"));
}

#[test]
fn restore_that_fails_part_way_compiles_as_on_a_miss() {
    let fixture = fixture("lost-race", "");
    let store = fixture.root.join("store");
    let (first, second) =
        (fixture.worktree("first", "metadata"), fixture.worktree("second", "metadata"));
    let build = |target: &Path| fixture.build(&fixture.args(target), &store, None);
    assert_eq!(build(&first), Ok(("miss".to_string(), 0)));
    let entry = fs::read_dir(store.join("entries")).unwrap().next().unwrap().unwrap().path();
    fs::write(entry.join("foo-abc.d"), b"\xff").unwrap();
    assert_eq!(build(&second), Ok(("miss".to_string(), 0)));
    let out = second.join("debug/build/foo/abc/out");
    assert_eq!(fs::read_to_string(out.join("runs")).unwrap(), "ran\n");
    assert_eq!(fs::read_to_string(out.join("libfoo-abc.rlib")).unwrap(), "rlib");
}

#[test]
fn replay_that_fails_part_way_runs_the_real_script_into_an_empty_out_dir() {
    let fixture = fixture("lost-race-script", "");
    let store = fixture.root.join("store");
    let body = r#"[ -e "$OUT_DIR/generated.rs" ] && exit 7
printf generated > "$OUT_DIR/generated.rs""#;
    let (first, first_out) = fixture.script("first", body, &[]);
    assert_eq!(replay_or_run(&real(&first_out), Ok(first), &store), ("miss".to_string(), 0));
    let mut entries =
        fs::read_dir(store.join("scripts")).unwrap().map(|entry| entry.unwrap().path());
    let entry = entries.find(|path| path.is_dir()).unwrap();
    // The stored tree is restored before the rewritten files, which are gone as after a removal by gc.
    fs::remove_dir_all(entry.join("text")).unwrap();
    let (second, out) = fixture.script("second", body, &[]);
    assert_eq!(replay_or_run(&real(&out), Ok(second), &store), ("miss".to_string(), 0));
    assert_eq!(fs::read_to_string(out.join("../runs")).unwrap(), "ran\n");
}

#[test]
fn script_key_is_moved_by_the_c_compiler_and_its_settings() {
    let fixture = fixture("script-c", "");
    let store = fixture.root.join("store");
    let key = |variables: &[(&str, &str)]| {
        let target = [("TARGET", "x86_64-unknown-linux-gnu"), ("HOST", "x86_64-unknown-linux-gnu")];
        fixture.script("only", "", &[&target[..], variables].concat()).0.key(&store).unwrap()
    };
    let compiler = fixture.root.join("fake-cc");
    let calls = fixture.root.join("calls");
    let install = |version: &str| {
        let script = format!("#!/bin/sh\necho called >> {}\necho {version}\n", calls.display());
        executable(&compiler, &script);
    };
    install("fake cc 1.0");
    let cc = compiler.display().to_string();
    let first = key(&[("CC", &cc)]);
    assert_eq!(first, key(&[("CC", &cc), ("RUST_LOG", "debug")]));
    assert_eq!(fs::read_to_string(&calls).unwrap(), "called\n", "the version is remembered");
    for setting in [
        "CFLAGS",
        "CFLAGS_x86_64-unknown-linux-gnu",
        "CFLAGS_x86_64_unknown_linux_gnu",
        "HOST_CFLAGS",
    ] {
        assert_ne!(first, key(&[("CC", &cc), (setting, "-O3")]), "{setting}");
    }
    assert_ne!(first, key(&[("HOST_CC", &cc)]), "HOST_CC is keyed as a variable of its own");
    install("fake cc 1.10");
    assert_ne!(first, key(&[("CC", &cc)]), "a compiler whose version changed moves the key");
}

/// A fixture whose stand-in names the source that it compiles, which `own_build` writes.
fn own(name: &str) -> Fixture {
    fixture(name, "$(echo \"$args\" | cut -d' ' -f3)")
}

/// Writes `source` at `path` with an mtime a minute ago, so that a compile that starts now may store it.
fn write_source(path: &Path, source: &str) {
    fs::write(path, source).unwrap();
    age(path, 60);
}

/// Compiles `foo` as an own crate of the worktree `name`, with `source` in `src/lib.rs`, and returns its output.
fn own_build(
    fixture: &Fixture,
    name: &str,
    source: &str,
) -> (Result<(String, i32), &'static str>, PathBuf) {
    own_compile(fixture, name, source, true)
}

/// [`own_build`] with a source written just now where it is not `settled`.
fn own_compile(
    fixture: &Fixture,
    name: &str,
    source: &str,
    settled: bool,
) -> (Result<(String, i32), &'static str>, PathBuf) {
    let (root, target) = (fixture.root.join(name), fixture.worktree(name, "metadata"));
    fs::create_dir_all(root.join(".git"))
        .and_then(|()| fs::create_dir_all(root.join("src")))
        .unwrap();
    match settled {
        true => write_source(&root.join("src/lib.rs"), source),
        false => fs::write(root.join("src/lib.rs"), source).unwrap(),
    }
    let mut args = fixture.args(&target);
    args[2] = root.join("src/lib.rs").display().to_string();
    args.extend([
        "-C".to_string(),
        format!("incremental={}/debug/incremental/{name}", target.display()),
    ]);
    let worktree = Some(root.display().to_string());
    let outcome = fixture.build(&args, &fixture.root.join("store"), worktree);
    (outcome, target.join("debug/build/foo/abc/out"))
}

/// A fixture whose stand-in, in a worktree that holds a file `gate`, waits up to five seconds for `gate.open` after it
/// writes `runs`.
fn gated(name: &str) -> Fixture {
    let fixture = own(name);
    let script = fs::read_to_string(&fixture.rustc).unwrap();
    let gate = r#"
root=${out%/target/*}
n=0
while [ -e "$root/gate" ] && [ ! -e "$root/gate.open" ] && [ $n -lt 500 ]; do sleep 0.01; n=$((n + 1)); done
printf rmeta"#;
    executable(Path::new(&fixture.rustc), &script.replacen("\nprintf rmeta", gate, 1));
    fixture
}

// Worktrees that edited the same own crate share its key, and each compile waited for the ones before it.
#[test]
fn own_crate_waits_only_for_a_compile_of_the_same_settled_sources() {
    let fixture = gated("same-inputs");
    // The reads that select an entry of the key are known once a compile stored them.
    own_build(&fixture, "seed", "pub fn seed() {}").0.unwrap();
    fs::create_dir_all(fixture.root.join("a")).unwrap();
    fs::write(fixture.root.join("a/gate"), "").unwrap();
    thread::scope(|scope| {
        let first = scope.spawn(|| own_build(&fixture, "a", "pub fn a() {}"));
        wait_for(&fixture.root.join("a/target/debug/build/foo/abc/out/runs"));
        let quick = |name, source, settled| {
            let started = Instant::now();
            let outcome = own_compile(&fixture, name, source, settled).0.unwrap().0;
            assert!(started.elapsed() < Duration::from_secs(3), "{name} waited");
            outcome
        };
        assert_eq!(quick("other", "pub fn b() {}", true), "miss");
        assert_eq!(quick("young", "pub fn a() {}", false), "passthrough changed-source");
        scope.spawn(|| {
            thread::sleep(Duration::from_millis(300));
            fs::write(fixture.root.join("a/gate.open"), "").unwrap();
        });
        let (outcome, out) = own_build(&fixture, "same", "pub fn a() {}");
        assert_eq!(outcome, Ok(("hit waited".to_string(), 0)));
        assert!(!out.join("runs").exists(), "rustc must run once");
        assert_eq!(first.join().unwrap().0, Ok(("miss".to_string(), 0)));
    });
}

#[test]
fn own_crate_is_shared_by_worktrees_until_a_source_changes() {
    let fixture = own("own");
    let (outcome, out) = own_build(&fixture, "a", "pub fn a() {}");
    assert_eq!(outcome, Ok(("miss".to_string(), 0)));
    let args = fs::read_to_string(out.join("args")).unwrap();
    assert!(
        args.contains(&format!(" --remap-path-prefix={}/a/=", fixture.root.display())),
        "{args}"
    );
    assert!(args.contains("/debug/incremental/a "), "{args}");

    // The path of the second worktree has another length, and its incremental directory has another name.
    let (outcome, out) = own_build(&fixture, "bbbb", "pub fn a() {}");
    assert_eq!(outcome, Ok(("hit".to_string(), 0)));
    assert!(!out.join("runs").exists(), "rustc must not run on a hit");
    let dep_info = fs::read_to_string(out.join("foo-abc.d")).unwrap();
    assert!(
        dep_info.contains(&format!(" {}/bbbb/src/lib.rs\n", fixture.root.display())),
        "{dep_info}"
    );

    // cargo rebuilds the dependents in a later build where an `.rmeta` is newer, and rustc leaves an unchanged one
    // alone.
    let rmeta = out.join("libfoo-abc.rmeta");
    age(&rmeta, 3600);
    let modified = fs::metadata(&rmeta).unwrap().modified().unwrap();
    assert_eq!(own_build(&fixture, "bbbb", "pub fn a() {}").0, Ok(("hit".to_string(), 0)));
    assert_eq!(fs::metadata(&rmeta).unwrap().modified().unwrap(), modified);

    let (outcome, out) = own_build(&fixture, "bbbb", "pub fn b() {}");
    assert_eq!(outcome, Ok(("miss".to_string(), 0)));
    assert_eq!(fs::read_to_string(out.join("runs")).unwrap(), "ran\n");
}

// cargo-llvm-cov matches the coverage map against absolute paths, so a remapped own crate kept its tests/ files.
/// Compiles `foo` of the package `package` from the root of the worktree `name`, which is its workspace, with the proc
/// macro `library` among its dependencies and `files` written in the worktree, and returns its outcome and output.
fn macro_build(
    fixture: &Fixture,
    name: &str,
    package: &str,
    library: &str,
    files: &[(&str, &str)],
) -> (Result<(String, i32), &'static str>, PathBuf) {
    let (root, target) = (fixture.root.join(name), fixture.worktree(name, "metadata"));
    let source = format!("{package}src/lib.rs");
    for (path, text) in [(source.as_str(), "pub fn a() {}")].iter().chain(files) {
        fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
        write_source(&root.join(path), text);
    }
    fs::create_dir_all(root.join(".git")).unwrap();
    let extension = if cfg!(target_os = "macos") { "dylib" } else { "so" };
    fs::write(target.join(format!("debug/build/dep/def/out/{library}.{extension}")), "").unwrap();
    let mut args = fixture.args(&target);
    args[2] = root.join(&source).display().to_string();
    let worktree = Some(root.display().to_string());
    let mut unit = Unit::model(&args, fixture.cargo_home.clone(), None, worktree).unwrap();
    (unit.package, unit.directory) = (Some(root.join(package)), root);
    let outcome =
        cached(&fixture.rustc, &args, &fixture.root.join("store"), &unit, &mut Vec::new());
    (outcome, target.join("debug/build/foo/abc/out"))
}

// sqlx reads `.sqlx` without declaring it, and a worktree whose offline data made a column nullable restored the
// output of another worktree with the same sources.
#[test]
fn own_crate_whose_proc_macro_reads_an_undeclared_file_of_its_package_is_selected_by_that_file() {
    let fixture = own("undeclared");
    let script = fs::read_to_string(&fixture.rustc).unwrap();
    let read = "cat \"${out%/target/*}/.sqlx/query.json\" >> \"$out/libfoo-abc.rlib\"";
    executable(
        Path::new(&fixture.rustc),
        &script.replace(
            "printf rlib > \"$out/libfoo-abc.rlib\"",
            &format!("printf rlib > \"$out/libfoo-abc.rlib\"\n{read}"),
        ),
    );
    let build_in = |package: &str, name: &str, query: &str, files: &[(&str, &str)]| {
        let files = [&[(".sqlx/query.json", query)], files].concat();
        let (outcome, out) = macro_build(&fixture, name, package, "libpm-123", &files);
        (outcome.unwrap().0, fs::read_to_string(out.join("libfoo-abc.rlib")).unwrap())
    };
    let build = |name: &str, query: &str, files: &[(&str, &str)]| build_in("", name, query, files);
    let ignored = [(".git/index", "a"), (".vscode/settings.json", "a"), ("src/.lib.rs.swp", "a")];
    assert_eq!(build("a", "nullable", &ignored), ("miss".to_string(), "rlibnullable".to_string()));
    assert_eq!(build("b", "not null", &[]), ("miss".to_string(), "rlibnot null".to_string()));
    assert_eq!(build("c", "nullable", &[]), ("hit".to_string(), "rlibnullable".to_string()));
    // Any other file of the package may be read too, though only a proc macro of the crate can read it.
    let readme = [("README.md", "changed")];
    assert_eq!(build("d", "nullable", &readme), ("miss".to_string(), "rlibnullable".to_string()));

    // Of a workspace root above the package, only the hidden files and directories count.
    let member = |name: &str, query: &str, readme: &str| {
        build_in("foo/", name, query, &[("README.md", readme), ("foo/Cargo.toml", "")])
    };
    assert_eq!(member("f", "nullable", "x"), ("miss".to_string(), "rlibnullable".to_string()));
    assert_eq!(member("g", "nullable", "y"), ("hit".to_string(), "rlibnullable".to_string()));
    assert_eq!(member("h", "not null", "y"), ("miss".to_string(), "rlibnot null".to_string()));

    let sqlx = macro_build(&fixture, "e", "", "libsqlx_macros-123", &[]).0;
    let offline = env::var("SQLX_OFFLINE").is_ok_and(|value| value == "true" || value == "1");
    if !offline {
        assert_eq!(sqlx, Err("undeclared-input"), "sqlx may query a database");
    }
}

// `sqlx::migrate!("../../migrations")` declares each migration but not the directory, and a worktree that added a
// migration restored the output of another worktree with one migration fewer.
#[test]
fn own_crate_whose_proc_macro_lists_a_directory_outside_its_package_is_selected_by_its_entries() {
    let fixture = own("listed");
    let script = fs::read_to_string(&fixture.rustc).unwrap();
    let list = r#"for file in "${out%/target/*}"/migrations/*; do
    cat "$file" >> "$out/libfoo-abc.rlib" && deps="$deps $file"
done
printf '%s/libfoo-abc.rmeta: "#;
    executable(Path::new(&fixture.rustc), &script.replace("printf '%s/libfoo-abc.rmeta: ", list));
    let build = |name: &str, migrations: &[(&str, &str)]| {
        let files = [&[("foo/Cargo.toml", "")], migrations].concat();
        let (outcome, out) = macro_build(&fixture, name, "foo/", "libpm-123", &files);
        (outcome.unwrap().0, fs::read_to_string(out.join("libfoo-abc.rlib")).unwrap())
    };
    let first = ("migrations/1_init.sql", "init;");
    let added = [first, ("migrations/2_email.sql", "email;")];
    assert_eq!(build("a", &[first]), ("miss".to_string(), "rlibinit;".to_string()));
    assert_eq!(build("b", &[first]), ("hit".to_string(), "rlibinit;".to_string()));
    assert_eq!(build("c", &added), ("miss".to_string(), "rlibinit;email;".to_string()));
    // A key keeps the reads of its last store, so a worktree like `c` hits, and one like `a` would miss.
    assert_eq!(build("d", &added), ("hit".to_string(), "rlibinit;email;".to_string()));
}

#[test]
fn own_crate_compiled_for_coverage_runs_as_plain_rustc_unless_a_later_flag_turns_it_off() {
    let fixture = own("coverage");
    let (root, target) = (fixture.root.join("a"), fixture.worktree("a", "metadata"));
    fs::create_dir_all(root.join(".git"))
        .and_then(|()| fs::create_dir_all(root.join("src")))
        .unwrap();
    write_source(&root.join("src/lib.rs"), "pub fn a() {}");
    let mut args = fixture.args(&target);
    args[2] = root.join("src/lib.rs").display().to_string();
    args.extend(["-C", "instrument-coverage"].map(str::to_string));
    let (store, out) = (fixture.root.join("store"), target.join("debug/build/foo/abc/out"));
    let wrap =
        |args: &[String]| crate::wrap(&fixture.rustc, args, &store, fixture.cargo_home.clone());
    assert_eq!(wrap(&args), 0);
    assert!(fs::read_to_string(store.join("log")).unwrap().ends_with(" passthrough coverage\n"));
    assert!(!fs::read_to_string(out.join("args")).unwrap().contains("--remap-path-prefix"));
    assert_eq!(fs::read_dir(store.join("entries")).unwrap().count(), 0);

    // A recent stamp keeps the miss from starting gc, which would run the test binary.
    fs::write(store.join("gc.stamp"), "").unwrap();
    args.push("-Cinstrument-coverage=off".to_string());
    assert_eq!(wrap(&args), 0);
    assert!(fs::read_to_string(store.join("log")).unwrap().contains(" miss\n"));
    assert!(fs::read_to_string(out.join("args")).unwrap().contains("--remap-path-prefix"));
}

#[test]
fn own_crate_whose_outputs_contain_the_worktree_path_is_never_stored() {
    let fixture = own("baked");
    let script = fs::read_to_string(&fixture.rustc).unwrap();
    executable(
        Path::new(&fixture.rustc),
        &script.replace("printf rlib >", "printf \"rlib ${out%/target/*}\" >"),
    );
    let first = own_build(&fixture, "a", "pub fn a() {}").0;
    assert_eq!(first, Ok(("passthrough baked-path".to_string(), 0)));
    // The scan of a rebuild runs when another build would promote the candidate, which it removes instead.
    assert_eq!(own_build(&fixture, "a", "pub fn b() {}").0, Ok(("miss".to_string(), 0)));
    rebuilt(&fixture, "b");
    assert_eq!(own_build(&fixture, "b", "pub fn b() {}").0, Ok(("miss".to_string(), 0)));
    let entries = fs::read_dir(fixture.root.join("store/entries")).unwrap();
    assert!(!entries.map(|entry| entry.unwrap().path()).any(|path| path.extension().is_none()));
    // cargo replays the stderr of a fresh crate in every build, so a notice there would recur.
    assert!(!fixture.root.join("store/notices").exists());
}

// A rename landed in a worktree while its build compiled the crate, and every later build restored the old outputs.
#[test]
fn own_crate_whose_source_changes_while_it_compiles_is_never_stored() {
    let fixture = own("changed");
    let script = fs::read_to_string(&fixture.rustc).unwrap();
    let rename = "\n[ -e \"${out%/target/*}/rename\" ] && echo 'pub fn b() {}' > \"$(echo \"$args\" | cut -d' ' -f3)\"";
    executable(
        Path::new(&fixture.rustc),
        &script.replace("\nprintf rmeta", &format!("{rename}\nprintf rmeta")),
    );
    fs::create_dir_all(fixture.root.join("a")).unwrap();
    fs::write(fixture.root.join("a/rename"), "").unwrap();
    let (outcome, _) = own_build(&fixture, "a", "pub fn a() {}");
    assert_eq!(outcome, Ok(("passthrough changed-source".to_string(), 0)));
    assert_eq!(fs::read_dir(fixture.root.join("store/entries")).unwrap().count(), 0);

    fs::remove_file(fixture.root.join("a/rename")).unwrap();
    let (outcome, out) = own_build(&fixture, "a", "pub fn b() {}");
    assert_eq!(outcome, Ok(("miss".to_string(), 0)));
    assert_eq!(fs::read_to_string(out.join("runs")).unwrap(), "ran\nran\n");

    // A source saved less than a second before the compile started may have changed after rustc read it.
    let root = fixture.root.join("a");
    let mut args = fixture.args(&fixture.worktree("a", "metadata"));
    args[2] = root.join("src/lib.rs").display().to_string();
    fs::write(&args[2], "pub fn c() {}").unwrap();
    let outcome =
        fixture.build(&args, &fixture.root.join("store"), Some(root.display().to_string()));
    assert_eq!(outcome, Ok(("passthrough changed-source".to_string(), 0)));
}

#[test]
fn walk_of_a_settled_package_is_remembered_until_one_of_its_files_changes() {
    let fixture = own("walk");
    let root = fixture.root.join("a");
    let args = fixture.args(&fixture.worktree("a", "metadata"));
    let worktree = Some(root.display().to_string());
    let mut unit = Unit::model(&args, fixture.cargo_home.clone(), None, worktree).unwrap();
    unit.package = Some(root.clone());
    fs::create_dir_all(root.join("tests/data")).unwrap();
    write_source(&root.join("lib.rs"), "pub fn a() {}");
    write_source(&root.join("tests/data/case.txt"), "one");
    let (memos, reads) =
        (fixture.root.join("hashes"), format!("file {WORKTREE}/lib.rs\ntree {WORKTREE}"));
    let variant = || unit.variant("key", &reads, None, &memos).unwrap();
    let first = variant();
    assert!(!memos.exists(), "a file whose ctime is new is not remembered");
    thread::sleep(Duration::from_secs(2));
    assert_eq!(variant(), first);
    let memo = memos_in(&memos);
    assert_eq!(memo.len(), 1);
    fs::write(&memo[0], [7; 32]).unwrap();
    let recalled = variant();
    assert_ne!(recalled, first, "a walk that finds the same entries reads its memo");

    // The write keeps the length and the mtime of a minute ago, but sets the ctime.
    write_source(&root.join("tests/data/case.txt"), "two");
    let changed = variant();
    assert!(changed != first && changed != recalled);
}

#[test]
fn own_crate_is_selected_only_by_sources_that_exist_and_were_read() {
    let fixture = own("unread");
    let root = fixture.root.join("a");
    let unit = Unit::model(
        &fixture.args(&fixture.worktree("a", "metadata")),
        fixture.cargo_home.clone(),
        None,
        Some(root.display().to_string()),
    )
    .unwrap();
    write_source(&root.join("lib.rs"), "pub fn a() {}");
    let source = format!("file {WORKTREE}/lib.rs");
    assert!(
        unit.variant("key", &source, Some(SystemTime::now()), &fixture.root.join("hashes")).is_ok()
    );
    assert_eq!(unit.variant("key", "", None, &fixture.root.join("hashes")), Err("no-source-read"));
    assert_eq!(
        unit.variant("key", "env PATH", None, &fixture.root.join("hashes")),
        Err("no-source-read")
    );
    let missing = format!("{source}\nfile {WORKTREE}/missing.rs");
    assert_eq!(
        unit.variant("key", &missing, None, &fixture.root.join("hashes")),
        Err("unreadable-read")
    );
    assert_eq!(
        unit.variant("key", &format!("{source}\nfile"), None, &fixture.root.join("hashes")),
        Err("unreadable-read")
    );

    // An entry stored under its bare key, as one that read no source is, is never restored for an own crate.
    assert_eq!(own_build(&fixture, "b", "pub fn a() {}").0, Ok(("miss".to_string(), 0)));
    let entries = fixture.root.join("store/entries");
    let reads = fs::read_dir(&entries).unwrap().map(|entry| entry.unwrap().path());
    let reads =
        Vec::from_iter(reads.filter(|path| path.extension().is_some_and(|kind| kind == "reads")));
    let entry = fs::read_dir(&entries)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir());
    fs::rename(entry.unwrap(), reads[0].with_extension("")).unwrap();
    fs::remove_file(&reads[0]).unwrap();
    assert_eq!(own_build(&fixture, "b", "pub fn a() {}").0, Ok(("miss".to_string(), 0)));
}

#[test]
fn relative_dep_info_paths_of_an_own_crate_resolve_against_the_working_directory() {
    let fixture = fixture("relative", "");
    let target = fixture.worktree("a", "metadata");
    let root = fixture.root.join("a");
    let mut unit = Unit::model(
        &fixture.args(&target),
        fixture.cargo_home.clone(),
        None,
        Some(root.display().to_string()),
    )
    .unwrap();
    unit.directory = root.join("crates");
    let out = target.join("debug/build/foo/abc/out").display().to_string();
    let reads =
        unit.portable_dep_info(&format!("{out}/libfoo-abc.rmeta: foo/lib.rs foo/../bar.rs\n"));
    assert_eq!(
        reads.unwrap().1,
        format!("file {WORKTREE}/crates/bar.rs\nfile {WORKTREE}/crates/foo/lib.rs")
    );
    assert_eq!(
        unit.portable_dep_info(&format!("{out}/libfoo-abc.rmeta: ../../outside.rs\n")),
        None
    );
    let read = unit.portable_dep_info(&format!("{out}/libfoo-abc.rmeta: foo\\ bar.rs\n"));
    assert_eq!(read.unwrap().1, format!("file {WORKTREE}/crates/foo bar.rs"));
    assert_eq!(unit.portable_dep_info(&format!("{out}/libfoo-abc.rmeta: foo\tbar.rs\n")), None);
}

#[test]
fn dep_info_with_spaces_in_its_roots_is_rewritten_as_rustc_escapes_it() {
    let fixture = fixture("spaces", "");
    let (target, root) = (fixture.worktree("a b", "metadata"), fixture.root.join("a b"));
    let args = Vec::from_iter(
        fixture.args(&fixture.root.join("a+b/target")).iter().map(|arg| arg.replace("a+b", "a b")),
    );
    let unit =
        Unit::model(&args, fixture.cargo_home.clone(), None, Some(root.display().to_string()));
    let unit = unit.unwrap();
    let escaped = |path: &Path| path.display().to_string().replace(' ', "\\ ");
    let (out, source) = (target.join("debug/build/foo/abc/out"), escaped(&root.join("src/lib.rs")));
    let dep_info = format!(
        "{}/libfoo-abc.rmeta: {source}\n\n{source}:\n# env-dep:CARGO_MANIFEST_DIR={}\n",
        out.display(),
        root.display()
    );
    let (portable, reads) = unit.portable_dep_info(&dep_info).unwrap();
    assert_eq!(reads, format!("file {WORKTREE}/src/lib.rs"));
    assert!(!portable.contains("a b") && !portable.contains("a\\ b"), "{portable}");
    assert_eq!(unit.roots.dep_info(&portable, Roots::localized), dep_info);
}

#[test]
fn rustc_older_than_1_100_is_refused_and_only_variables_that_cargo_sets_are_keyed() {
    assert!(unit::is_supported(b"rustc 1.100.0\nrelease: 1.100.0\n"));
    assert!(unit::is_supported(b"release: 1.102.0-nightly\n"));
    assert!(!unit::is_supported(b"release: 1.98.0\n") && !unit::is_supported(b"stand-in 1.0\n"));
    for name in ["CARGO", "CARGO_PKG_NAME", "CARGO_MANIFEST_DIR", "SDKROOT"] {
        assert!(unit::is_keyed(name), "{name}");
    }
    for name in ["CARGO_TARGET_DIR", "CARGO_HOME", "CARGO_TARGET_TMPDIR", "CARGO_BIN_EXE_app"] {
        assert!(!unit::is_keyed(name), "{name}");
    }
}

// The `[env]` table of cargo's own repository sets CARGO_RUSTC_CURRENT_DIR to the path of its checkout.
#[test]
fn variable_that_cargo_leaves_unset_selects_an_entry_only_where_the_crate_reads_it() {
    let fixture = fixture("env-table", "");
    let (store, variable) = (fixture.root.join("store"), "CARGO_RUSTC_CURRENT_DIR");
    let checkout = |name: &str| fixture.root.join(name).display().to_string();
    let key = |name: &str| {
        let args = fixture.args(&fixture.worktree(name, "metadata"));
        let environment = env::vars_os().chain([(variable.into(), checkout(name).into())]);
        fixture.unit(&args).unwrap().key(&fixture.rustc, &args, &store, environment).unwrap()
    };
    assert_eq!(key("first"), key("second"));
    let target = fixture.root.join("first/target");
    let out = target.join("debug/build/foo/abc/out").display().to_string();
    let source = format!("{}registry/src/index/foo-1.0.0/src/lib.rs", fixture.cargo_home);
    let dep_info = format!("{out}/libfoo-abc.rmeta: {source}\n# env-dep:{variable}=/work\n");
    let unit = fixture.unit(&fixture.args(&target)).unwrap();
    assert_eq!(unit.portable_dep_info(&dep_info).unwrap().1, format!("env {variable}"));

    let body = format!("echo cargo:rerun-if-env-changed={variable}");
    let run = |name: &str, value: &str| {
        let (run, out) = fixture.script(name, &body, &[(variable, &checkout(value))]);
        (run.key(&store).unwrap(), replay_or_run(&real(&out), Ok(run), &store).0)
    };
    let (first, outcome) = run("first", "first");
    assert_eq!(outcome, "miss");
    assert_eq!(run("second", "second"), (first.clone(), "miss".to_string()));
    assert_eq!(run("third", "first"), (first, "hit".to_string()));
}

#[test]
fn native_library_is_keyed_by_the_file_that_the_search_directories_resolve() {
    let fixture = fixture("native", "");
    let (target, system) = (fixture.worktree("only", "metadata"), fixture.root.join("system"));
    fs::create_dir_all(&system).unwrap();
    let search = ["-L".to_string(), format!("native={}", system.display())];
    let args = [&fixture.args(&target)[..], &search, &["-l".to_string(), "z".to_string()]].concat();
    let default = fixture.key(&args);
    let library = system.join(if cfg!(target_os = "macos") { "libz.dylib" } else { "libz.so" });
    fs::write(&library, "1").unwrap();
    let first = fixture.key(&args);
    assert_ne!(
        default, first,
        "a library that the directories do not contain is keyed by its name"
    );
    fs::write(system.join("libz.a"), "static").unwrap();
    assert_eq!(first, fixture.key(&args), "the linker prefers the dynamic library");
    fs::write(&library, "2").unwrap();
    assert_ne!(first, fixture.key(&args));
}

#[test]
fn own_crate_is_shared_from_the_repository_of_its_source_wherever_the_target_lies() {
    let fixture = own("repositories");
    let (outer, inner) = (fixture.root.join("outer"), fixture.root.join("outer/inner"));
    for directory in [outer.join(".git/x"), inner.join(".git/x"), outer.join("dep/src")] {
        fs::create_dir_all(directory).unwrap();
    }
    let worktree = |path: &Path| unit::worktree(path, &fixture.cargo_home);
    assert_eq!(worktree(&outer.join("dep/src/lib.rs")), Some(outer.display().to_string()));
    assert_eq!(worktree(&inner.join("src/lib.rs")), Some(inner.display().to_string()));
    let build = |name: &str| {
        let (root, target) =
            (fixture.root.join(name), fixture.worktree(&format!("{name}-out"), "metadata"));
        fs::create_dir_all(root.join(".git"))
            .and_then(|()| fs::create_dir_all(root.join("src")))
            .unwrap();
        write_source(&root.join("src/lib.rs"), "pub fn a() {}");
        let mut args = fixture.args(&target);
        args[2] = root.join("src/lib.rs").display().to_string();
        let unit =
            Unit::model(&args, fixture.cargo_home.clone(), None, worktree(Path::new(&args[2])));
        let remap = [
            format!("--remap-path-prefix={}/=target/", target.display()),
            format!("--remap-path-prefix={}/=", root.display()),
        ];
        assert_eq!(unit.unwrap().remap(), remap);
        fixture.build(&args, &fixture.root.join("store"), worktree(Path::new(&args[2])))
    };
    assert_eq!(build("a"), Ok(("miss".to_string(), 0)));
    assert_eq!(build("bbbb"), Ok(("hit".to_string(), 0)));
}

#[test]
fn own_crate_that_links_on_macos_leaves_out_the_paths_of_its_worktree() {
    let fixture = fixture("links", "");
    let (target, root) = (fixture.worktree("a", "metadata"), fixture.root.join("a"));
    let args =
        [&fixture.args(&target)[..], &["--crate-type".to_string(), "proc-macro".to_string()]]
            .concat();
    let unit =
        Unit::model(&args, fixture.cargo_home.clone(), None, Some(root.display().to_string()));
    let flags = unit.unwrap().remap().join(" ");
    let link = format!(
        "link-arg=-Wl,-oso_prefix,{}/ -C link-arg=-Wl,-install_name,@rpath/libfoo-abc.dylib",
        root.display()
    );
    assert_eq!(flags.contains(&link), cfg!(target_os = "macos"), "{flags}");
}

#[test]
fn uncached_compile_is_remapped_as_a_cached_one_is() {
    let fixture = fixture("uncached", "");
    let (target, store) = (fixture.worktree("only", "metadata"), fixture.root.join("store"));
    let args = [
        &fixture.args(&target)[..],
        &["--extern".to_string(), "gone=/gone/libgone.rmeta".to_string()],
    ]
    .concat();
    assert_eq!(crate::wrap(&fixture.rustc, &args, &store, fixture.cargo_home.clone()), 0);
    let ran = fs::read_to_string(target.join("debug/build/foo/abc/out/args")).unwrap();
    assert!(
        ran.ends_with(&format!(" --remap-path-prefix={}/=target/\n", target.display())),
        "{ran}"
    );
    assert!(
        fs::read_to_string(store.join("log")).unwrap().ends_with(" passthrough unreadable-input\n")
    );
}

/// Compiles `foo` as [`own_build`] does, but from the root of its worktree, as cargo does, with `extra` arguments.
fn carried_build(
    fixture: &Fixture,
    name: &str,
    source: &str,
    extra: &[&str],
) -> (Result<(String, i32), &'static str>, PathBuf) {
    workspace_build(fixture, name, ".", source, extra)
}

/// Compiles `foo` as [`carried_build`] does, from the cargo workspace at `workspace` in the worktree `name`.
///
/// The target directory lies in that workspace, and the source is named relative to it.
/// The stderr that cargo receives is written to `cargo` in the output directory.
fn workspace_build(
    fixture: &Fixture,
    name: &str,
    workspace: &str,
    source: &str,
    extra: &[&str],
) -> (Result<(String, i32), &'static str>, PathBuf) {
    let (root, directory) =
        (fixture.root.join(name), unit::lexical(&fixture.root.join(name).join(workspace)));
    let target = fixture.worktree(&format!("{name}/{workspace}"), "metadata");
    fs::create_dir_all(root.join(".git"))
        .and_then(|()| fs::create_dir_all(directory.join("src")))
        .unwrap();
    write_source(&directory.join("src/lib.rs"), source);
    let mut args = fixture.args(&unit::lexical(&target));
    args[2] = if workspace == "." {
        root.join("src/lib.rs").display().to_string()
    } else {
        "src/lib.rs".to_string()
    };
    let incremental = format!("incremental={}/debug/incremental", unit::lexical(&target).display());
    args.extend(["-C", &incremental].iter().chain(extra).map(|arg| arg.to_string()));
    let worktree = Some(root.display().to_string());
    let mut unit = Unit::model(&args, fixture.cargo_home.clone(), None, worktree).unwrap();
    unit.directory = directory;
    let mut cargo = Vec::new();
    let outcome = cached(&fixture.rustc, &args, &fixture.root.join("store"), &unit, &mut cargo);
    let out = target.join("debug/build/foo/abc/out");
    fs::write(out.join("cargo"), cargo).unwrap();
    (outcome, out)
}

/// The pool of slots of `foo` in the store of `fixture`, by its canonical path.
fn pool(fixture: &Fixture) -> PathBuf {
    let slots = fs::canonicalize(fixture.root.join("store/slots")).unwrap();
    fs::read_dir(slots).unwrap().next().unwrap().unwrap().path()
}

/// The newest session of the worktree that the slot `number` in `pool` recorded.
fn kept_session(pool: &Path, number: &str) -> PathBuf {
    let record = fs::read_to_string(pool.join(format!("{number}.source"))).unwrap();
    PathBuf::from(record.lines().next().unwrap())
}

/// Waits until the slot `number` of `pool` is free, which a child that another test forks holds until it runs exec.
///
/// A child forked while this process holds the lock to look holds it too, so the lock is then left alone for a while.
fn free(pool: &Path, number: usize) {
    while session::choose(pool, vec![number]).is_none_or(|(taken, _)| taken != number) {}
    thread::sleep(Duration::from_millis(50));
}

/// Takes the slot at `path` for `unit` once it is [`free`].
///
/// It holds every other slot that it takes meanwhile, because the next take prefers a slot that the worktree records.
fn take(unit: &Unit, args: &[String], store: &Path, files: Files, path: &Path) -> session::Slot {
    let number = path.file_stem().and_then(|stem| stem.to_str()?.parse().ok()).unwrap();
    free(path.parent().unwrap(), number);
    let mut others = Vec::new();
    loop {
        let taken = session::Slot::take(unit, args, store, files).unwrap();
        if Path::new(&taken.path) == path {
            return taken;
        }
        others.push(taken);
    }
}

/// Whether `done` holds within five seconds, as a lock does once each child that another test forked while this process
/// held it has run exec.
fn eventually(mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done() {
        if Instant::now() > deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
    true
}

fn sessions(out: &Path) -> String {
    fs::read_to_string(out.join("sessions")).unwrap()
}

#[test]
fn slot_is_the_preferred_one_when_free_else_the_lowest_free_else_a_new_one() {
    let pool = fixture("slots", "").root.join("pool");
    fs::create_dir_all(&pool).unwrap();
    let started = Instant::now();
    let (zero, _zero) = session::choose(&pool, vec![]).unwrap();
    let (two, _two) = session::choose(&pool, vec![2]).unwrap();
    let (one, _one) = session::choose(&pool, vec![2, 0]).unwrap();
    let (three, _three) = session::choose(&pool, vec![]).unwrap();
    assert_eq!([zero, two, one, three], [0, 2, 1, 3]);
    assert!(started.elapsed() < Duration::from_secs(1), "a compile never waits for a slot");
}

#[test]
fn carried_compile_runs_in_a_slot_and_names_only_the_worktree_to_cargo_and_the_store() {
    let fixture = own("carried");
    rebuilt(&fixture, "a");
    let (outcome, out) = carried_build(&fixture, "a", "pub fn a() {}", &[]);
    assert_eq!(outcome, Ok(("miss".to_string(), 0)));
    let (root, slot) =
        (fixture.root.join("a").display().to_string(), pool(&fixture).join("0.slot"));
    let slot = slot.display().to_string();
    assert_eq!(sessions(&out), format!("none {slot}\n"));
    let args = fs::read_to_string(out.join("args")).unwrap();
    assert!(args.contains(&format!(" --remap-path-prefix={slot}/=")), "{args}");
    assert!(args.contains(&format!("incremental={slot}/target/debug/incremental/foo-trucker.")));
    assert!(!args.contains(&root), "{args}");
    let dep_info = fs::read_to_string(out.join("foo-abc.d")).unwrap();
    assert!(dep_info.contains(&format!(" {root}/src/lib.rs\n")) && !dep_info.contains(&slot));
    // A dep-info that cannot be rewritten in place compiles the crate again in the worktree, which writes its own.
    let staging = out.join(format!(".foo-abc.d.{}", std::process::id()));
    fs::create_dir_all(&staging).unwrap();
    let (outcome, _) = carried_build(&fixture, "a", "pub fn a() { let _ = 1; }", &[]);
    fs::remove_dir(&staging).unwrap();
    assert_eq!(outcome, Ok(("passthrough dep-info".to_string(), 0)));
    let dep_info = fs::read_to_string(out.join("foo-abc.d")).unwrap();
    assert!(dep_info.contains(&format!(" {root}/src/lib.rs\n")) && !dep_info.contains(&slot));
    let (promoted, z) = carried_build(&fixture, "z", "pub fn a() {}", &[]);
    assert_eq!(promoted, Ok(("hit promoted".to_string(), 0)));
    let artifact = format!("{{\"artifact\":\"{}/libfoo-abc.rmeta\"}}", unit::lexical(&z).display());
    assert_eq!(
        cargo_lines(&z).into_iter().collect::<Vec<_>>(),
        [(artifact, 1)],
        "stderr reaches cargo"
    );
    let entries = fs::read_dir(fixture.root.join("store/entries")).unwrap();
    let entry = entries.map(|entry| entry.unwrap().path()).find(|path| path.extension().is_none());
    let entry = entry.unwrap();
    let stderr = fs::read_to_string(entry.join(".stderr")).unwrap();
    assert!(stderr.starts_with(&format!("{{\"artifact\":\"{TARGET}debug/")), "{stderr}");
    let record = fs::read_to_string(entry.join(store::RECORD)).unwrap();
    assert!(record.ends_with("\n-C\nincremental"), "an incremental compile is recorded as one");

    // An incremental directory outside the worktree is not carried.
    let (store, mut args) =
        (fixture.root.join("store"), fixture.args(&fixture.root.join("a/target")));
    args[2] = format!("{root}/src/lib.rs");
    let mut unit = Unit::model(&args, String::new(), None, Some(root.clone())).unwrap();
    unit.directory = fixture.root.join("a");
    let with = |incremental: String| {
        session::Slot::take(&unit, &[&args[..], &[incremental]].concat(), &store, Files::Clones)
    };
    assert!(with("incremental=/elsewhere".to_string()).is_none());
    let taken = with(format!("incremental={root}/target/debug/incremental"));
    // A child that another test forks shares the lock of slot 0 until it runs exec, so the slot may be another one.
    let taken = taken.unwrap();
    let slot = &taken.path.clone();
    assert_eq!(&taken.rewrite(&root), slot);
    assert_eq!(
        taken.rewrite(&format!("x={root}/src {root}-old/src")),
        format!("x={slot}/src {root}-old/src")
    );
    assert_eq!(taken.localized(&format!("{slot}/src")), format!("{root}/src"));
    drop(taken);
    // A compile that links reads clones of its libraries in the slot, whose real paths every worktree shares.
    let libraries = format!("{root}/target/debug/build/dep/def/out");
    fs::create_dir_all(&libraries).unwrap();
    fs::write(format!("{libraries}/libdep-def.rmeta"), "rmeta").unwrap();
    fs::write(format!("{libraries}/dep-def.d"), "").unwrap();
    let incremental = format!("incremental={root}/target/debug/incremental");
    let args = [&args[..], &["--crate-type".to_string(), "bin".to_string(), incremental]].concat();
    let taken = session::Slot::take(&unit, &args, &store, Files::Clones).unwrap();
    let clones = format!("{}/.trucker-libraries/target/debug/build/dep/def/out", taken.path);
    let linked = taken.args(&args).join(" ");
    assert!(
        linked.contains(&format!(" dependency={clones} --extern dep={clones}/libdep-def.rmeta"))
    );
    assert_eq!(fs::read_to_string(format!("{clones}/libdep-def.rmeta")).unwrap(), "rmeta");
    assert!(!Path::new(&format!("{clones}/dep-def.d")).exists(), "only libraries are cloned");
    assert_eq!(taken.localized(&format!("{clones}/x")), format!("{libraries}/x"));
    assert_eq!(
        session::linked(&format!("{clones}/x"), &root, &taken.path),
        format!("{}/target/debug/build/dep/def/out/x", taken.path)
    );
    drop(taken);
    // On macOS the clones stay. A later compile clones again only a library whose contents changed, and a library
    // replaced by the same contents keeps its clone.
    let rmeta = format!("{clones}/libdep-def.rmeta");
    let linking = clones.split("/.trucker-libraries/").next().unwrap();
    let retaken = || drop(take(&unit, &args, &store, Files::Clones, Path::new(linking)));
    if !cfg!(target_os = "linux") {
        assert_eq!(fs::read_to_string(&rmeta).unwrap(), "rmeta");
        thread::sleep(Duration::from_secs(2));
        let settled = unit::identity(Path::new(&rmeta)).unwrap();
        retaken();
        assert_eq!(
            unit::identity(Path::new(&rmeta)).unwrap(),
            settled,
            "a clone of the same contents stays"
        );
        let manifest = fs::read_to_string(linking.replace(".slot", ".libraries")).unwrap();
        assert!(manifest.ends_with("\t\n"), "{manifest}");
        let library = format!("{libraries}/libdep-def.rmeta");
        fs::remove_file(&library).and_then(|()| fs::write(&library, "rmeta")).unwrap();
        retaken();
        assert_eq!(
            unit::identity(Path::new(&rmeta)).unwrap(),
            settled,
            "a restored library keeps its clone"
        );
        // A library is remembered only once it is over a second old, and a new clone only where its library is.
        thread::sleep(Duration::from_secs(2));
        fs::write(format!("{clones}/libdep-def.rmeta"), "other").unwrap();
        retaken();
        assert_eq!(
            fs::read_to_string(&rmeta).unwrap(),
            "rmeta",
            "a clone that differs is cloned again"
        );
        let remembered =
            unit::recalled(&store.join("hashes"), &unit::identity(Path::new(&rmeta)).unwrap());
        assert_eq!(
            remembered,
            Some(blake3::hash(b"rmeta").as_bytes().to_vec()),
            "a new clone has a memo"
        );
    }
    // A later compile removes the libraries that are gone and those that no file describes.
    fs::write(format!("{libraries}/libdep-def.rmeta"), "RMETA").unwrap();
    fs::write(format!("{clones}/libstale.rlib"), "").unwrap();
    retaken();
    assert_eq!(fs::read_to_string(&rmeta).unwrap(), "RMETA");
    assert!(!Path::new(&format!("{clones}/libstale.rlib")).exists());
    let unnamed = format!("{linking}/.trucker-libraries/elsewhere");
    fs::create_dir_all(&unnamed).unwrap();
    fs::remove_file(linking.replace(".slot", ".libraries")).unwrap();
    retaken();
    assert!(!Path::new(&unnamed).exists() && Path::new(&rmeta).exists());
    // Where the store links, a library stays while it is the same inode, and is linked again once replaced.
    let linked = || drop(take(&unit, &args, &store, Files::Links, Path::new(linking)));
    let library = format!("{libraries}/libdep-def.rmeta");
    let inode = |path: &str| fs::metadata(path).unwrap().ino();
    fs::remove_file(linking.replace(".slot", ".libraries")).unwrap();
    linked();
    assert_eq!(inode(&rmeta), inode(&library));
    let ctime = fs::metadata(&library).unwrap().ctime_nsec();
    linked();
    assert_eq!(
        fs::metadata(&library).unwrap().ctime_nsec(),
        ctime,
        "a kept link is not made again"
    );
    fs::remove_file(&library).and_then(|()| fs::write(&library, "new")).unwrap();
    linked();
    assert_eq!(
        (inode(&rmeta), fs::read_to_string(&rmeta).unwrap()),
        (inode(&library), "new".into())
    );
    // On Linux a link keeps the memo of a library, under a lease that an open for writing refuses.
    if cfg!(target_os = "linux") {
        let memos = store.join("hashes");
        let recalled =
            |path: &str| unit::recalled(&memos, &unit::identity(Path::new(path)).unwrap());
        for (contents, writing) in [("settled", false), ("written", true)] {
            fs::remove_file(&library).and_then(|()| fs::write(&library, contents)).unwrap();
            thread::sleep(Duration::from_secs(2));
            let hash = unit::remembered_hash(Path::new(&library), &memos).unwrap();
            assert_eq!(hash, blake3::hash(contents.as_bytes()));
            let writer =
                writing.then(|| fs::OpenOptions::new().append(true).open(&library).unwrap());
            retaken();
            drop(writer);
            assert_eq!(inode(&rmeta), inode(&library));
            let kept = (!writing).then(|| hash.as_bytes().to_vec());
            assert_eq!(recalled(&library), kept, "{contents}");
        }
    }
    // A link on macOS runs through trucker, which names each library in the slot.
    let (_, out) = carried_build(&fixture, "a", "pub fn b() {}", &["--crate-type", "bin"]);
    let linker = format!(" -C linker={}", env::current_exe().unwrap().display());
    let linked = fs::read_to_string(out.join("args")).unwrap();
    assert_eq!(linked.contains(&linker), cfg!(target_os = "macos"), "{linked}");
    assert_eq!(
        session::linked(&format!("{root}/target/libx.rlib"), &root, slot),
        format!("{slot}/target/libx.rlib")
    );
}

#[test]
fn session_is_kept_per_slot_and_restored_only_where_the_worktree_has_none_for_that_slot() {
    let fixture = own("sessions");
    rebuilt(&fixture, "a");
    assert_eq!(carried_build(&fixture, "a", "pub fn a() {}", &[]).0, Ok(("miss".to_string(), 0)));
    let pool = pool(&fixture);
    let kept =
        |slot: &str| kept_session(&pool, slot).file_name().unwrap().to_str().unwrap().to_string();
    let first = kept("0");
    let added = fs::read_to_string(fixture.root.join("store/gc.added")).unwrap();
    assert!(
        pool.join("0.session").exists() && added.lines().count() == 1,
        "a slot keeps a snapshot"
    );
    free(&pool, 0);
    let (outcome, out) = carried_build(&fixture, "b", "pub fn b() {}", &[]);
    assert_eq!(outcome, Ok(("miss session".to_string(), 0)));
    let zero = pool.join("0.slot").display().to_string();
    assert_eq!(sessions(&out), format!("{first} {zero}\n"), "rustc finds the session and its lock");
    let second = kept("0");
    free(&pool, 0);
    let (outcome, out) = carried_build(&fixture, "b", "pub fn c() {}", &[]);
    assert_eq!(outcome, Ok(("miss".to_string(), 0)), "the worktree's own session is used");
    assert!(sessions(&out).ends_with(&format!("{second} {zero}\n")));

    // A compile that finds slot 0 locked takes slot 1, which has no session yet.
    // A child that another test forks may share a lock for a moment, so the lock of slot 0 is taken until it is 0.
    let lock = loop {
        if let Some((0, lock)) = session::choose(&pool, vec![0]) {
            break lock;
        }
    };
    free(&pool, 1);
    let (outcome, out) = carried_build(&fixture, "c", "pub fn d() {}", &[]);
    let one = pool.join("1.slot").display().to_string();
    assert_eq!((outcome, sessions(&out)), (Ok(("miss".to_string(), 0)), format!("none {one}\n")));
    // The worktree's session belongs to the locked slot 0, so it starts from the session of slot 1.
    let third = kept("1");
    free(&pool, 1);
    let (outcome, out) = carried_build(&fixture, "b", "pub fn e() {}", &[]);
    assert_eq!(outcome, Ok(("miss session".to_string(), 0)));
    assert!(sessions(&out).ends_with(&format!("{third} {one}\n")));
    drop(lock);

    // The worktree that slot 0 recorded has since named slot 1, so slot 0 starts from the snapshot in its pool, which
    // the first restore took.
    let lock = loop {
        if let Some((1, lock)) = session::choose(&pool, vec![1]) {
            break lock;
        }
    };
    free(&pool, 0);
    let (outcome, out) = carried_build(&fixture, "d", "pub fn f() {}", &[]);
    let snapshot = (Ok(("miss session".to_string(), 0)), format!("{first} {zero}\n"));
    assert_eq!((outcome, sessions(&out)), snapshot);
    drop(lock);
    // rustc deletes a session only under an exclusive lock of its lock file, which keeps it from being cloned.
    let session = kept_session(&pool, "0");
    let locked = fs::File::open(
        session.with_file_name(format!("{}.lock", kept("0").rsplit_once('-').unwrap().0)),
    );
    locked.as_ref().unwrap().lock().unwrap();
    free(&pool, 0);
    let (outcome, out) = carried_build(&fixture, "e", "pub fn g() {}", &[]);
    assert_eq!((outcome, sessions(&out)), snapshot);
    assert!(session.exists());
    // A worktree removes its sessions, which another slot may be cloning, only under the same lock.
    let sessions = session.parent().unwrap().parent().unwrap();
    locked.as_ref().unwrap().unlock().unwrap();
    locked.as_ref().unwrap().lock_shared().unwrap();
    assert!(session::remove_sessions(sessions).is_err() && session.exists());
    drop(locked);
    assert!(eventually(|| session::remove_sessions(sessions).is_ok()) && !sessions.exists());
}

// New worktrees compiled the dependents of an edited crate at once, and all but the first started cold in new slots.
// Then all of them waited for the one slot with a session, each for as long as a cold compile took, also in the next
// edit, and those after the first waited for longer than a cold compile.
#[test]
fn compile_waits_for_a_busy_slot_with_a_session_only_alone_and_as_long_as_it_saves() {
    let fixture = own("slot-wait");
    rebuilt(&fixture, "a");
    assert_eq!(carried_build(&fixture, "a", "pub fn a() {}", &[]).0, Ok(("miss".to_string(), 0)));
    let pool = pool(&fixture);
    let cold = fs::read_to_string(pool.join("0.cold")).unwrap();
    assert!(cold.parse::<u64>().is_ok(), "a compile without a session records its time");
    let slot = |number: usize| pool.join(format!("{number}.slot")).display().to_string();
    let held = |number: usize| loop {
        if let Some((taken, lock)) = session::choose(&pool, vec![number])
            && taken == number
        {
            break lock;
        }
    };
    // Where the pool has no record of a carried compile, a compile does not wait.
    fs::write(pool.join("0.cold"), "5000").unwrap();
    let lock = held(0);
    let started = Instant::now();
    let (outcome, out) = carried_build(&fixture, "b", "pub fn b() {}", &[]);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "waited with no record of a carried compile"
    );
    assert_eq!(
        (outcome, sessions(&out)),
        (Ok(("miss".to_string(), 0)), format!("none {}\n", slot(1)))
    );
    // Where another compile waits in the pool, a compile does not wait either.
    let _one = held(1);
    fs::write(pool.join("0.warm"), "100").unwrap();
    let waiter = fs::File::create(pool.join("wait.lock")).unwrap();
    waiter.lock().unwrap();
    let started = Instant::now();
    let (outcome, out) = carried_build(&fixture, "c", "pub fn c() {}", &[]);
    assert!(started.elapsed() < Duration::from_secs(4), "waited behind another waiting compile");
    assert_eq!(
        (outcome, sessions(&out)),
        (Ok(("miss".to_string(), 0)), format!("none {}\n", slot(2)))
    );
    drop(waiter);
    // Otherwise it waits, and takes the slot once it frees up.
    let _two = held(2);
    thread::scope(|scope| {
        scope.spawn(move || {
            thread::sleep(Duration::from_millis(300));
            drop(lock);
        });
        let (outcome, out) = carried_build(&fixture, "d", "pub fn d() {}", &[]);
        assert_eq!(outcome, Ok(("miss session".to_string(), 0)));
        assert!(sessions(&out).ends_with(&format!(" {}\n", slot(0))), "{}", sessions(&out));
    });
    let warm = fs::read_to_string(pool.join("0.warm")).unwrap();
    assert!(warm != "100" && warm.parse::<u64>().is_ok(), "a carried compile records its time");
    // The worktree that waited compiles its next edit from its own session.
    free(&pool, 0);
    let (outcome, out) = carried_build(&fixture, "d", "pub fn e() {}", &[]);
    assert_eq!(outcome, Ok(("miss".to_string(), 0)));
    assert!(!sessions(&out).lines().last().unwrap().starts_with("none "));

    // Once what a carried compile saves has passed, the compile starts cold in a free slot.
    for number in [1, 2] {
        fs::remove_file(pool.join(format!("{number}.cold"))).unwrap();
    }
    fs::write(pool.join("0.cold"), "400").unwrap();
    fs::write(pool.join("0.warm"), "100").unwrap();
    let _zero = held(0);
    let started = Instant::now();
    let (outcome, out) = carried_build(&fixture, "e", "pub fn f() {}", &[]);
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert_eq!(
        (outcome, sessions(&out)),
        (Ok(("miss".to_string(), 0)), format!("none {}\n", slot(3)))
    );
}

#[test]
fn session_in_a_store_that_links_is_copied_from_the_recorded_worktree_so_that_a_write_in_place_reaches_no_other()
 {
    let fixture = own("links-sessions");
    let script = fs::read_to_string(&fixture.rustc).unwrap();
    // The stand-in writes an object into each session, and records the object of the session that it starts from.
    let (poison, lock) =
        ("[ -e \"$inc/foo-1x/$old/poison\" ]", "touch \"$inc/foo-1x/s-$$-r.lock\"");
    let read = format!("cat \"$inc/foo-1x/$old/0.o\" > \"$out/object\" 2> /dev/null\n    {poison}");
    let write = format!("{lock} && printf $$ > \"$inc/foo-1x/s-$$-r-svh/0.o\"");
    let script = script.replace(poison, &read).replace(lock, &write);
    executable(Path::new(&fixture.rustc), &script);
    fs::write(fixture.root.join("store/links"), "").unwrap();
    assert_eq!(carried_build(&fixture, "a", "pub fn a() {}", &[]).0, Ok(("miss".to_string(), 0)));
    let kept = kept_session(&pool(&fixture), "0").join("0.o");
    let original = fs::read_to_string(&kept).unwrap();
    free(&pool(&fixture), 0);
    let (outcome, out) = carried_build(&fixture, "b", "pub fn b() {}", &[]);
    assert_eq!(outcome, Ok(("miss session".to_string(), 0)));
    assert_eq!(fs::read_to_string(out.join("object")).unwrap(), original);
    let inodes = fs::read_to_string(out.join("inodes")).unwrap();
    let inode = fs::metadata(&kept).unwrap().ino().to_string();
    assert!(!inodes.split_whitespace().any(|word| word == inode), "restore copies the session");

    // LLVM truncates and writes an object in place, so a session whose object changed after it was recorded is
    // not shared, and the next worktree starts from the snapshot of the pool, which no write reaches.
    let kept = kept_session(&pool(&fixture), "0").join("0.o");
    fs::write(&kept, "rewritten").unwrap();
    free(&pool(&fixture), 0);
    let (outcome, out) = carried_build(&fixture, "c", "pub fn c() {}", &[]);
    assert_eq!(outcome, Ok(("miss session".to_string(), 0)));
    assert_eq!(fs::read_to_string(out.join("object")).unwrap(), original);
}

#[test]
fn compile_from_a_workspace_below_the_worktree_root_runs_in_a_nested_mirror_and_is_carried() {
    let fixture = own("nested");
    let built = |name, source| workspace_build(&fixture, name, "crates/ws", source, &[]);
    assert_eq!(built("a", "pub fn a() {}").0, Ok(("miss".to_string(), 0)));
    free(&pool(&fixture), 0);
    let (outcome, out) = built("b", "pub fn b() {}");
    assert_eq!(outcome, Ok(("miss session".to_string(), 0)));
    let slot = pool(&fixture).join("0.slot");
    assert!(sessions(&out).ends_with(&format!(" {}/crates/ws\n", slot.display())));
    let dep_info = fs::read_to_string(out.join("foo-abc.d")).unwrap();
    assert!(dep_info.contains(" src/lib.rs\n") && !dep_info.contains("/slots/"), "{dep_info}");

    // The path down to the working directory is real, and every entry beside it is a symlink into the worktree.
    let root = fixture.root.join("b");
    fs::write(root.join("crates/sibling"), "").unwrap();
    let (store, mut args) =
        (fixture.root.join("store"), fixture.args(&root.join("crates/ws/target")));
    args[2] = "src/lib.rs".to_string();
    args.push(format!("incremental={}/crates/ws/target/debug/incremental", root.display()));
    let mut unit =
        Unit::model(&args, String::new(), None, Some(root.display().to_string())).unwrap();
    unit.directory = root.join("crates/ws");
    let taken = session::Slot::take(&unit, &args, &store, Files::Clones).unwrap();
    let slot = PathBuf::from(&taken.path);
    let link = |path: &str| fs::symlink_metadata(slot.join(path)).map(|meta| meta.is_symlink());
    assert_eq!((taken.directory.clone(), link("crates").unwrap()), (slot.join("crates/ws"), false));
    assert!(
        link(".git").unwrap() && link("crates/sibling").unwrap() && link("crates/ws/src").unwrap()
    );
    assert!(!link("crates/ws").unwrap());
    // A slot that mirrors the worktree is not written again. A symlink that names another place, an entry that the
    // worktree lacks and one that it gained are put right.
    let retaken = || take(&unit, &args, &store, Files::Clones, &slot);
    let modified = |path: &str| fs::metadata(slot.join(path)).unwrap().modified().unwrap();
    let before = (modified("."), modified("crates"));
    drop(taken);
    let taken = retaken();
    assert_eq!((modified("."), modified("crates")), before);
    drop(taken);
    fs::remove_file(slot.join("crates/sibling")).unwrap();
    std::os::unix::fs::symlink(root.join(".git"), slot.join("crates/sibling")).unwrap();
    fs::write(slot.join("stray"), "").unwrap();
    fs::write(root.join("gained"), "").unwrap();
    let taken = retaken();
    assert_eq!(fs::read_link(slot.join("crates/sibling")).unwrap(), root.join("crates/sibling"));
    assert!(!slot.join("stray").exists() && link("gained").unwrap());
    // The slot reused from the root of the worktree drops the real directories and the siblings that left.
    fs::remove_file(root.join("crates/sibling")).unwrap();
    (unit.directory, args[2]) = (root.clone(), format!("{}/crates/ws/src/lib.rs", root.display()));
    drop(taken);
    let taken = session::Slot::take(&unit, &args, &store, Files::Clones).unwrap();
    let slot = PathBuf::from(&taken.path);
    assert!(fs::symlink_metadata(slot.join("crates")).unwrap().is_symlink());
    // Removing a slot never follows its symlinks into the worktree.
    drop(taken);
    fs::remove_dir_all(&slot).unwrap();
    assert!(root.join("crates/ws/src/lib.rs").exists());
    unit.directory = fixture.root.clone();
    assert!(
        session::Slot::take(&unit, &args, &store, Files::Clones).is_none(),
        "a directory above the worktree"
    );
}

/// A fixture whose stand-in bakes its working directory into the rlib after it notifies the metadata.
///
/// It bakes the path as `env!("CARGO_MANIFEST_DIR")` bakes the rewritten manifest directory, and also writes a warning.
/// Where `tracked`, the dep-info records the variable before its notification, as rustc writes them.
fn baking(name: &str, tracked: bool) -> Fixture {
    let bakes =
        "$(echo \"$args\" | cut -d' ' -f3)$(pwd -P | tr -d '\\n' > \"$out/libfoo-abc.rlib\")";
    let fixture = fixture(name, bakes);
    let script = fs::read_to_string(&fixture.rustc).unwrap();
    let variable = if tracked { "# env-dep:CARGO_MANIFEST_DIR='\"$(pwd -P)\"'\\n" } else { "" };
    let script = script
        .replace("=foo\\n'", &format!("=foo\\n{variable}'"))
        .replace(
            "printf '{\"artifact\"",
            "printf '{\"artifact\":\"%s/foo-abc.d\",\"emit\":\"dep-info\"}\\n' \"$out\" >&2\n\
             echo '{\"$message_type\":\"diagnostic\",\"message\":\"unused\"}' >&2\nprintf '{\"artifact\"",
        )
        .replace("libfoo-abc.rmeta\"}", "libfoo-abc.rmeta\",\"emit\":\"metadata\"}");
    executable(Path::new(&fixture.rustc), &script);
    fixture
}

/// The lines of the stderr that cargo received in `out`, each with the number of times that it came.
fn cargo_lines(out: &Path) -> BTreeMap<String, usize> {
    let mut lines = BTreeMap::new();
    for line in fs::read_to_string(out.join("cargo")).unwrap().lines() {
        *lines.entry(line.to_string()).or_default() += 1;
    }
    lines
}

#[test]
fn output_that_names_the_slot_is_compiled_again_in_the_worktree_and_never_carried_again() {
    let fixture = baking("slot-baked", true);
    let (outcome, out) = carried_build(&fixture, "a", "pub fn a() {}", &[]);
    assert_eq!(outcome, Ok(("passthrough baked-slot".to_string(), 0)));
    let root = fs::canonicalize(fixture.root.join("a")).unwrap();
    assert_eq!(fs::read_to_string(out.join("libfoo-abc.rlib")).unwrap(), root.to_str().unwrap());
    assert_eq!(fs::read_to_string(out.join("runs")).unwrap(), "ran\nran\n");
    // cargo marks the metadata ready at each notification of it, and panics at a second one.
    let lines = cargo_lines(&out);
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(lines.values().all(|count| *count == 1), "{lines:?}");
    assert!(lines.keys().any(|line| line.contains("rmeta\",\"emit\":\"metadata")), "{lines:?}");
    let (outcome, out) = carried_build(&fixture, "b", "pub fn b() {}", &[]);
    assert_eq!(outcome, Ok(("passthrough baked-path".to_string(), 0)));
    assert!(!sessions(&out).contains("/slots/") && !pool(&fixture).join("0.source").exists());
    // A change of a dependency or a flag gives the crate another pool, which the marker of the crate still covers.
    let marker = fs::read_dir(fixture.root.join("store/out")).unwrap().next().unwrap().unwrap();
    assert!(marker.file_name().to_str().unwrap().ends_with(".foo.library.baked"));
    age(&marker.path(), 2 * 86400);
    let (outcome, out) = carried_build(&fixture, "c", "pub fn c() {}", &["-C", "metadata=other"]);
    assert_eq!(outcome, Ok(("passthrough baked-path".to_string(), 0)));
    assert!(!sessions(&out).contains("/slots/"));
    let pools = fs::read_dir(fixture.root.join("store/slots")).unwrap().count();
    assert_eq!(pools, 1, "a baked crate gets no pool");
    let modified = fs::metadata(marker.path()).unwrap().modified().unwrap();
    assert!(modified.elapsed().unwrap() < Duration::from_secs(60), "a use refreshes the marker");
}

// A dependent that started from the metadata of the compile in the slot would not link with the library of the
// worktree.
#[test]
fn output_that_names_the_slot_after_cargo_saw_its_metadata_fails_once_and_is_compiled_in_the_worktree_next()
 {
    let fixture = baking("slot-baked-late", false);
    let (outcome, out) = carried_build(&fixture, "a", "pub fn a() {}", &[]);
    assert_eq!(outcome, Ok(("passthrough baked-slot-late".to_string(), 1)));
    assert_eq!(fs::read_to_string(out.join("runs")).unwrap(), "ran\n");
    assert!(!out.join("libfoo-abc.rlib").exists() && !out.join("libfoo-abc.rmeta").exists());
    let lines = cargo_lines(&out);
    assert!(lines.values().all(|count| *count == 1), "{lines:?}");
    assert!(lines.keys().any(|line| line.contains("rmeta\",\"emit\":\"metadata")), "{lines:?}");
    assert!(lines.keys().any(|line| line.starts_with("trucker failed foo,")), "{lines:?}");
    let (outcome, out) = carried_build(&fixture, "a", "pub fn a() {}", &[]);
    let refused = Ok(("passthrough baked-path".to_string(), 0));
    assert_eq!(outcome, refused, "a compile whose outputs were removed is stored at once");
    assert!(!sessions(&out).lines().last().unwrap().contains("/slots/"));
}

#[test]
fn local_thin_lto_is_not_carried_and_a_slot_that_cannot_be_set_up_compiles_as_before() {
    let codegen =
        |args: &str| session::local_thin_lto(&Vec::from_iter(args.split(' ').map(str::to_string)));
    assert!(codegen("-C opt-level=2") && codegen("-Copt-level=s -C codegen-units=16"));
    assert!(!codegen("-C opt-level=0") && !codegen("-C opt-level=2 -C lto=off"));
    assert!(!codegen("-C opt-level=3 -C codegen-units=1") && !codegen("-C opt-level=3 -C lto"));

    let fixture = own("not-carried");
    let (outcome, out) = carried_build(&fixture, "a", "pub fn a() {}", &["-C", "opt-level=2"]);
    assert_eq!(outcome, Ok(("miss".to_string(), 0)));
    assert!(!sessions(&out).contains("/slots/") && !fixture.root.join("store/slots").exists());
    fs::write(fixture.root.join("store/slots"), "not a directory").unwrap();
    let (outcome, out) = carried_build(&fixture, "a", "pub fn b() {}", &[]);
    assert_eq!(outcome, Ok(("miss".to_string(), 0)));
    assert!(!sessions(&out).contains("/slots/"));
    fs::remove_file(fixture.root.join("store/slots")).unwrap();
    let (_, out) =
        carried_build(&fixture, "a", "pub fn c() {}", &["-C", "opt-level=2", "-C", "lto=off"]);
    assert!(sessions(&out).contains("/slots/"));
}

// An ICE that a restored session causes must not fail the build or recur in the next worktree.
#[test]
fn crash_after_a_restored_session_deletes_it_and_compiles_again() {
    let fixture = own("crash");
    let poisoned = carried_build(&fixture, "a", "pub fn a() {} // poison", &[]);
    assert_eq!(poisoned.0, Ok(("miss".to_string(), 0)));
    assert!(kept_session(&pool(&fixture), "0").join("poison").exists());
    free(&pool(&fixture), 0);
    let (outcome, out) = carried_build(&fixture, "b", "pub fn b() {}", &[]);
    assert_eq!(outcome, Ok(("miss".to_string(), 0)));
    assert_eq!(fs::read_to_string(out.join("runs")).unwrap(), "ran\nran\n");
    assert!(
        sessions(&out).ends_with(&format!("none {}\n", pool(&fixture).join("0.slot").display()))
    );
    let kept = kept_session(&pool(&fixture), "0");
    let snapshot = pool(&fixture).join("0.session/foo-1x").join(kept.file_name().unwrap());
    assert!(!kept.join("poison").exists() && snapshot.exists(), "the snapshot is taken again");

    // The restored session is deleted only under an exclusive lock of its lock file, as rustc deletes one, and where
    // that fails no compile takes it as the slot's again.
    let root = fixture.root.join("a").display().to_string();
    let mut args = fixture.args(&fixture.root.join("a/target"));
    args[2] = format!("{root}/src/lib.rs");
    args.push(format!("incremental={root}/target/debug/incremental"));
    let mut unit = Unit::model(&args, String::new(), None, Some(root.clone())).unwrap();
    unit.directory = fixture.root.join("a");
    let incremental = fixture.root.join("a/target/debug/incremental");
    let sessions = fs::read_dir(&incremental).unwrap().next().unwrap().unwrap().path();
    fs::remove_dir_all(&sessions).unwrap();
    let store = fixture.root.join("store");
    let mut slot = take(&unit, &args, &store, Files::Clones, &pool(&fixture).join("0.slot"));
    assert!(slot.restored);
    let locks = fs::read_dir(sessions.join("foo-1x")).unwrap().flatten().map(|entry| entry.path());
    let lock = fs::File::open(locks.into_iter().find(|path| path.is_file()).unwrap()).unwrap();
    lock.lock_shared().unwrap();
    assert!(slot.discard().is_err());
    assert!(sessions.join("foo-1x").exists() && !sessions.join("slot").exists());
    drop(lock);
    assert!(eventually(|| slot.discard().is_ok()));
    assert_eq!(fs::read_dir(&sessions).unwrap().count(), 1, "only the file that names the slot");
}

// rustc writes in place an object that it keeps beside its outputs and links into its session, so a session whose file
// changed after the slot recorded it is not shared.
#[test]
fn session_whose_file_was_written_after_it_was_recorded_is_not_restored() {
    let fixture = own("rewritten");
    assert_eq!(carried_build(&fixture, "a", "pub fn a() {}", &[]).0, Ok(("miss".to_string(), 0)));
    fs::write(kept_session(&pool(&fixture), "0").join("dep-graph.bin"), "graph").unwrap();
    fs::remove_dir_all(pool(&fixture).join("0.session")).unwrap();
    free(&pool(&fixture), 0);
    let (outcome, out) = carried_build(&fixture, "b", "pub fn b() {}", &[]);
    assert_eq!(outcome, Ok(("miss".to_string(), 0)));
    assert_eq!(sessions(&out), format!("none {}\n", pool(&fixture).join("0.slot").display()));
}

// A worktree that is removed takes with it the session that a slot recorded there, so the next worktree starts from the
// snapshot of the pool, which an edit in one worktree does not take again.
#[test]
fn worktree_after_the_recorded_one_was_removed_starts_from_the_snapshot_of_the_pool() {
    let fixture = own("removed");
    assert_eq!(carried_build(&fixture, "a", "pub fn a() {}", &[]).0, Ok(("miss".to_string(), 0)));
    let pool = pool(&fixture);
    let first = kept_session(&pool, "0").file_name().unwrap().to_str().unwrap().to_string();
    free(&pool, 0);
    let carried = carried_build(&fixture, "b", "pub fn b() {}", &[]).0;
    assert_eq!(carried, Ok(("miss session".to_string(), 0)));
    let snapshot = || fs::metadata(pool.join("0.session")).unwrap().ino();
    let taken = snapshot();
    for edit in ["pub fn c() {}", "pub fn d() {}"] {
        free(&pool, 0);
        assert_eq!(carried_build(&fixture, "b", edit, &[]).0, Ok(("miss".to_string(), 0)));
    }
    assert_eq!(snapshot(), taken, "an edit in the worktree takes no snapshot");
    assert!(kept_session(&pool, "0").starts_with(fixture.root.join("b")));
    fs::remove_dir_all(fixture.root.join("a")).unwrap();
    fs::remove_dir_all(fixture.root.join("b")).unwrap();
    free(&pool, 0);
    let (outcome, out) = carried_build(&fixture, "c", "pub fn e() {}", &[]);
    assert_eq!(outcome, Ok(("miss session".to_string(), 0)));
    assert_eq!(sessions(&out), format!("{first} {}\n", pool.join("0.slot").display()));
    assert_eq!(fs::read_to_string(out.join("runs")).unwrap(), "ran\n");
    assert_eq!(fs::read_to_string(out.join("libfoo-abc.rlib")).unwrap(), "rlib");
}

#[test]
fn gc_removes_the_slots_that_no_compile_used_for_three_days_with_their_snapshots() {
    let fixture = fixture("gc-sessions", "");
    let (store, pool) = (fixture.root.join("store"), fixture.root.join("store/slots/foo"));
    for (path, seconds) in [
        ("entries/old", 3 * 3600),
        ("slots/foo/0.session.1", 2 * 3600),
        ("slots/foo/1.session.1", 60),
    ] {
        fs::create_dir_all(store.join(path))
            .and_then(|()| fs::write(store.join(path).join("lib"), [0; 4096]))
            .unwrap();
        age(&store.join(path), seconds);
    }
    let (_, lock) = session::choose(&pool, vec![1]).unwrap();
    fs::create_dir_all(pool.join("2.slot")).unwrap();
    for slot in ["0", "1"] {
        fs::create_dir_all(pool.join(format!("{slot}.slot/.trucker-libraries"))).unwrap();
    }
    for name in ["0.lock", "0.source", "2.lock", "2.source", "2.libraries"] {
        fs::write(pool.join(name), "").unwrap();
    }
    for slot in ["0", "2"] {
        fs::create_dir_all(pool.join(format!("{slot}.session/foo-1x"))).unwrap();
    }
    age(&pool.join("2.lock"), 4 * 86400);
    let cut = crate::now() - 3 * 86400;
    let size = gc::collect(&store, 0, u64::MAX, false, || (100_000, 100_000)).size;
    assert!(!pool.join("0.session.1").exists(), "a snapshot left in staging is removed");
    assert!(pool.join("1.session.1").exists(), "a leftover of the last hour stays");
    let collected = gc::collect(&store, cut, size - 1, false, || (100_000, 100_000));
    assert_eq!((collected.entries, collected.freed), (1, 4096));
    assert!(!store.join("entries/old").exists());
    assert!(pool.join("0.source").exists(), "a record stays with its slot");
    assert!(pool.join("0.session").exists(), "a snapshot stays with its slot");
    assert!(pool.join("0.slot/.trucker-libraries").exists(), "clones stay with their slot");
    assert!(!pool.join("2.slot").exists() && !pool.join("2.lock").exists());
    assert!(!pool.join("2.libraries").exists() && !pool.join("2.source").exists());
    assert!(!pool.join("2.session").exists());
    drop(lock);
    for path in ["1.session.1", "1.lock", "0.lock"] {
        age(&pool.join(path), 7200);
    }
    gc::collect(&store, crate::now(), 0, false, || (100, 100));
    assert!(!store.join("slots/foo").exists(), "an empty pool goes with its last slot");
}

#[test]
fn own_crate_that_runs_thin_lto_in_a_slow_incremental_compile_is_reported_and_no_other() {
    let root = fixture("thin-lto", "").root.join("store");
    let compile = |name: &str, fields: &str, extra: &str| {
        let input = match name {
            "slow" => format!("{WORKTREE}/model/src/lib.rs"),
            "registry" => format!("{CARGO_HOME}registry/src/index/model-0.1.0/src/lib.rs"),
            _ => "src/lib.rs".to_string(),
        };
        let args = format!(
            "--crate-name\nmodel\n{input}\n--emit=dep-info,metadata,link\n-C\nopt-level=2\n{extra}"
        );
        fs::create_dir_all(root.join("entries").join(name)).unwrap();
        let record = format!("model\n0.1.0\n{fields}\n{args}");
        fs::write(root.join("entries").join(name).join(store::RECORD), record).unwrap();
    };
    compile("slow", "model/target\n24000", "-C\nincremental");
    compile("slower", "model/target\n26000", "-C\nincremental\n--test");
    compile("explicit", "target\n30000", "-C\nincremental\n-C\nlto=off");
    compile("quick", "quick/target\n9000", "-C\nincremental");
    compile("release", "release/target\n60000", "");
    compile("single", "single/target\n60000", "-C\nincremental\n-C\ncodegen-units=1");
    compile("registry", "registry/target\n60000", "-C\nincremental");
    let report = stats(&["--duplicates".to_string()], &root).unwrap();
    let section = "unless their profile sets lto = \"off\":\n    26.0 s  model  in model/target\n";
    assert!(report.ends_with(section), "{report}");
}

#[test]
fn any_contains_finds_any_root_anywhere_in_any_file() {
    let fixture = own("contains");
    let path = fixture.root.join("output");
    fs::write(&path, b"\0/a/worktree\0/b/slot").unwrap();
    let contains = |texts: &[&str]| store::any_contains(&[&path], texts);
    assert!(contains(&["/x", "/b/slot"]));
    assert!(contains(&["/a/worktree"]));
    assert!(!contains(&["/a/worktree/", "/b/slots"]));
    assert!(store::any_contains(&[fixture.root.join("missing")], &["/a"]));
    assert!(!store::any_contains(&[] as &[PathBuf], &["/a"]));

    // Files above the size that is mapped and not read. Only the last of them contains the root, at its end.
    let paths = Vec::from_iter((0..40).map(|n| fixture.root.join(format!("object{n}.o"))));
    for path in &paths {
        fs::write(path, vec![b'/'; 1 << 17]).unwrap();
    }
    assert!(!store::any_contains(&paths, &["/a/worktree"]));
    let last = paths.last().unwrap();
    fs::write(last, [vec![b'/'; 1 << 17], b"a/worktree".to_vec()].concat()).unwrap();
    assert!(store::any_contains(&paths, &["/x", "/a/worktree"]));
}

// The scan for baked paths compared each root at every byte of each output, so the debug info of an executable delayed
// every edit that relinked it, and a run of bytes that repeats the start of a root made the scan quadratic.
#[test]
fn contains_scans_a_large_output_in_linear_time() {
    let fixture = own("large");
    let path = fixture.root.join("output");
    let root = format!("{}b", "a".repeat(4095));
    fs::write(&path, vec![b'a'; 8 << 20]).unwrap();
    let started = Instant::now();
    assert!(!store::any_contains(&[path], &[&root]));
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
}

#[test]
fn file_hashes_keep_the_order_of_their_paths_and_fail_on_a_missing_file() {
    let fixture = own("hashes");
    // More files than one chunk, and some longer than one segment, which are hashed as subtrees on every core.
    let paths = Vec::from_iter((0..150).map(|n| fixture.root.join(format!("file{n}"))));
    for (n, path) in paths.iter().enumerate() {
        let long = Vec::from_iter((0..(3 << 20) + n).map(|at| (at * 31 + n) as u8));
        fs::write(path, if n % 40 == 0 { long } else { n.to_string().into_bytes() }).unwrap();
    }
    let sequential = Vec::from_iter(paths.iter().map(|path| unit::file_hash(path).unwrap()));
    let memos = fixture.root.join("memos");
    assert_eq!(unit::file_hashes(&paths, &memos).unwrap(), sequential);
    assert_eq!(unit::file_hashes(&paths, &memos).unwrap(), sequential);
    assert!(unit::file_hashes(&[], &memos).unwrap().is_empty());
    assert!(unit::file_hashes(&[fixture.root.join("missing")], &memos).is_err());
}

#[test]
fn digests_hash_each_file_in_order_and_find_a_root_across_the_seams_of_the_scan() {
    let fixture = own("digests");
    // Sizes around the mebibyte that is one subtree, and counts of subtrees on both sides of powers of two.
    let sizes = [0, 3, 1 << 16, 1 << 20, (1 << 20) + 5, 2 << 20, 3 << 20, (5 << 20) + 1, 9 << 20];
    let paths = Vec::from_iter(sizes.iter().map(|size| fixture.root.join(format!("output{size}"))));
    for (path, size) in paths.iter().zip(sizes) {
        fs::write(path, Vec::from_iter((0..size).map(|at| (at % 251) as u8))).unwrap();
    }
    let hashes = Vec::from_iter(paths.iter().map(|path| unit::file_hash(path).unwrap().to_hex()));
    let (digests, found) = lazy::digests(&paths, &["/a/slot"]).unwrap();
    assert!(
        digests.iter().map(String::as_str).eq(hashes.iter().map(|hash| hash.as_str())) && !found
    );
    // A root that straddles the end of the first mebibyte, then one at the very end of a file.
    let mut bytes = vec![0; 3 << 20];
    for at in [(1 << 20) - 3, (3 << 20) - 7] {
        bytes[at..at + 7].copy_from_slice(b"/a/slot");
        fs::write(&paths[6], &bytes).unwrap();
        assert!(lazy::digests(&paths, &["/x", "/a/slot"]).unwrap().1);
        bytes[at..at + 7].fill(0);
    }
    assert!(lazy::digests(&[fixture.root.join("missing")], &[]).is_none());
    assert_eq!(lazy::digests(&[], &["/a"]), Some((Vec::new(), false)));
}

#[test]
fn a_remembered_hash_is_read_from_its_memo_until_the_file_changes() {
    let fixture = own("remembered");
    let (path, memos) = (fixture.root.join("library"), fixture.root.join("memos"));
    fs::write(&path, "one").unwrap();
    assert_eq!(unit::remembered_hash(&path, &memos).unwrap(), blake3::hash(b"one"));
    assert!(!memos.exists(), "a file that changed in the last second is not remembered");
    thread::sleep(Duration::from_secs(2));
    assert_eq!(unit::remembered_hash(&path, &memos).unwrap(), blake3::hash(b"one"));
    let memo = memos_in(&memos).remove(0);
    fs::write(&memo, blake3::hash(b"remembered").as_bytes()).unwrap();
    assert_eq!(unit::remembered_hash(&path, &memos).unwrap(), blake3::hash(b"remembered"));
    // Linux sets file times from a clock that advances every few milliseconds.
    thread::sleep(Duration::from_millis(20));
    fs::write(&path, "two").unwrap();
    assert_eq!(unit::remembered_hash(&path, &memos).unwrap(), blake3::hash(b"two"));
    for memo in memos_in(&memos) {
        fs::write(memo, "not a hash").unwrap();
    }
    assert_eq!(unit::remembered_hash(&path, &memos).unwrap(), blake3::hash(b"two"));
}

#[test]
fn a_tree_hashes_alike_under_its_roots_and_is_remembered_under_its_listing() {
    let fixture = own("tree");
    let tree_in = |target: &Path| {
        let tree = target.join("debug/build/dep/out");
        fs::create_dir_all(tree.join("lib")).unwrap();
        fs::write(tree.join("lib/libnative.a"), [0xff_u8, 0, 1, 2]).unwrap();
        fs::write(tree.join("lib/native.pc"), format!("prefix={}", tree.display())).unwrap();
        std::os::unix::fs::symlink("lib/libnative.a", tree.join("link")).unwrap();
        let roots = Roots {
            target: format!("{}/", target.display()),
            cargo_home: "/home/dev/.cargo/".to_string(),
            worktree: None,
        };
        (tree, roots)
    };
    let fed = |(tree, roots): &(PathBuf, Roots), memos: &Path| {
        let mut fields = Vec::new();
        unit::hash_tree(roots, tree, memos, &mut |bytes| fields.push(bytes.to_vec())).unwrap();
        fields
    };
    let (first, other) =
        (tree_in(&fixture.root.join("target")), tree_in(&fixture.root.join("other")));
    let memos = fixture.root.join("memos");
    let hashed = fed(&first, &memos);
    assert_eq!(hashed.len(), 1);
    assert!(!memos.exists(), "a tree that changed in the last second is not remembered");
    assert_eq!(
        fed(&other, &fixture.root.join("other-memos")),
        hashed,
        "the roots enter as placeholders"
    );
    thread::sleep(Duration::from_secs(2));
    assert_eq!(fed(&first, &memos), hashed);
    let remembered = memos_in(&memos);
    assert_eq!(remembered.len(), 2, "the tree and the file that is not UTF-8 are remembered");
    for memo in &remembered {
        fs::write(memo, blake3::hash(b"remembered").as_bytes()).unwrap();
    }
    assert_eq!(fed(&first, &memos), [blake3::hash(b"remembered").as_bytes()]);
    thread::sleep(Duration::from_millis(20));
    fs::write(first.0.join("lib/native.pc"), "prefix=/elsewhere").unwrap();
    let changed = fed(&first, &memos);
    assert_ne!(changed, [blake3::hash(b"remembered").as_bytes()]);
    assert_ne!(changed, hashed);
}

/// A fixture whose stand-in writes the source that it compiles into the `.rlib`.
fn lazy(name: &str) -> Fixture {
    let fixture = own(name);
    let script = fs::read_to_string(&fixture.rustc).unwrap();
    let copies = "cat \"$(echo \"$args\" | cut -d' ' -f3)\" >";
    executable(Path::new(&fixture.rustc), &script.replace("printf rlib >", copies));
    fixture
}

/// Leaves the `.rlib` of an earlier compile of `foo` in the worktree `name`, so that its next compile is a rebuild.
fn rebuilt(fixture: &Fixture, name: &str) {
    let out = fixture.worktree(name, "metadata").join("debug/build/foo/abc/out");
    fs::write(out.join("libfoo-abc.rlib"), "earlier").unwrap();
}

#[test]
fn own_crate_compiled_first_in_a_removed_worktree_is_restored_in_a_new_one() {
    let fixture = lazy("removed-first");
    assert_eq!(own_build(&fixture, "a", "pub fn a() {}").0, Ok(("miss".to_string(), 0)));
    assert_eq!((entries(&fixture).len(), candidates(&fixture).len()), (1, 0));
    fs::remove_dir_all(fixture.root.join("a")).unwrap();
    let (outcome, out) = own_build(&fixture, "b", "pub fn a() {}");
    assert_eq!(outcome, Ok(("hit".to_string(), 0)));
    assert!(!out.join("runs").exists());
    assert_eq!(fs::read_to_string(out.join("libfoo-abc.rlib")).unwrap(), "pub fn a() {}");
}

#[test]
fn rebuild_after_an_edit_records_a_candidate_and_stores_nothing() {
    let fixture = lazy("edited");
    own_build(&fixture, "a", "pub fn a() {}").0.unwrap();
    let stored = entries(&fixture);
    let (outcome, out) = own_build(&fixture, "a", "pub fn b() {}");
    assert_eq!(outcome, Ok(("miss".to_string(), 0)));
    assert_eq!(fs::read_to_string(out.join("runs")).unwrap(), "ran\nran\n");
    assert_eq!((entries(&fixture), candidates(&fixture).len()), (stored, 1));
}

/// The entries in the store of `fixture`, without staging directories and candidates.
fn entries(fixture: &Fixture) -> Vec<PathBuf> {
    let entries = fs::read_dir(fixture.root.join("store/entries")).unwrap();
    let entries = entries.map(|entry| entry.unwrap().path());
    Vec::from_iter(entries.filter(|path| path.is_dir() && path.extension().is_none()))
}

/// The candidate files in the store of `fixture`.
fn candidates(fixture: &Fixture) -> Vec<PathBuf> {
    let directories = entries_with(fixture, "cand");
    Vec::from_iter(
        directories.iter().flat_map(|directory| {
            fs::read_dir(directory).unwrap().map(|file| file.unwrap().path())
        }),
    )
}

fn entries_with(fixture: &Fixture, kind: &str) -> Vec<PathBuf> {
    let entries = fs::read_dir(fixture.root.join("store/entries")).unwrap();
    let entries = entries.map(|entry| entry.unwrap().path());
    Vec::from_iter(
        entries.filter(|path| path.extension().is_some_and(|extension| extension == kind)),
    )
}

/// Records a candidate in the worktree `a` and applies `mutation` to its `.rlib`.
///
/// A rebuild of the same source in the worktree `b` must then compile and store nothing.
fn mutated(name: &str, mutation: impl FnOnce(&Fixture, &Path)) {
    let fixture = lazy(name);
    rebuilt(&fixture, "a");
    let (outcome, out) = own_build(&fixture, "a", "pub fn a() {}");
    assert_eq!(outcome, Ok(("miss".to_string(), 0)));
    assert!(entries(&fixture).is_empty(), "an own crate records a candidate");
    mutation(&fixture, &out.join("libfoo-abc.rlib"));
    rebuilt(&fixture, "b");
    let (outcome, out) = own_build(&fixture, "b", "pub fn a() {}");
    assert_eq!(outcome, Ok(("miss".to_string(), 0)));
    assert_eq!(fs::read_to_string(out.join("runs")).unwrap(), "ran\n");
    assert_eq!(fs::read_to_string(out.join("libfoo-abc.rlib")).unwrap(), "pub fn a() {}");
    assert!(entries(&fixture).is_empty());
}

// cargo hard-links each output that it uplifts once rustc ends, and sets back the mtime of a check's `.rmeta`.
#[test]
fn candidate_whose_outputs_gained_a_link_and_another_mtime_is_promoted() {
    let fixture = lazy("linked");
    rebuilt(&fixture, "a");
    let (_, out) = own_build(&fixture, "a", "pub fn a() {}");
    fs::hard_link(out.join("libfoo-abc.rlib"), fixture.root.join("a/target/libfoo.rlib")).unwrap();
    age(&out.join("libfoo-abc.rmeta"), 60);
    let (outcome, out) = own_build(&fixture, "b", "pub fn a() {}");
    assert_eq!(outcome, Ok(("hit promoted".to_string(), 0)));
    assert_eq!(fs::read_to_string(out.join("libfoo-abc.rlib")).unwrap(), "pub fn a() {}");
}

fn overwrite(rlib: &Path, at: u64, bytes: &[u8]) {
    use std::io::{Seek, Write};
    let mut file = fs::OpenOptions::new().write(true).open(rlib).unwrap();
    file.seek(std::io::SeekFrom::Start(at)).and_then(|_| file.write_all(bytes)).unwrap();
}

#[test]
fn candidate_is_promoted_by_a_build_of_the_same_sources_with_the_bytes_that_were_compiled() {
    let fixture = lazy("promoted");
    rebuilt(&fixture, "a");
    assert_eq!(own_build(&fixture, "a", "pub fn a() {}").0, Ok(("miss".to_string(), 0)));
    let (outcome, out) = own_build(&fixture, "b", "pub fn a() {}");
    assert_eq!(outcome, Ok(("hit promoted".to_string(), 0)));
    assert!(!out.join("runs").exists());
    assert_eq!(fs::read_to_string(out.join("libfoo-abc.rlib")).unwrap(), "pub fn a() {}");
    assert_eq!(own_build(&fixture, "c", "pub fn a() {}").0, Ok(("hit".to_string(), 0)));
    assert!(candidates(&fixture).is_empty(), "a promoted candidate is removed");
}

#[test]
fn candidate_in_a_store_that_cannot_clone_is_promoted_by_copies_that_each_restore_verifies() {
    let fixture = lazy("promoted-links");
    rebuilt(&fixture, "a");
    fs::write(fixture.root.join("store/links"), "").unwrap();
    let (_, a) = own_build(&fixture, "a", "pub fn a() {}");
    assert_eq!(own_build(&fixture, "b", "pub fn a() {}").0, Ok(("hit promoted".to_string(), 0)));
    let [entry] = &entries(&fixture)[..] else { panic!("one entry") };
    let mut names =
        Vec::from_iter(fs::read_dir(entry).unwrap().map(|file| file.unwrap().file_name()));
    names.sort();
    let eager =
        [".hashes", ".record", ".stderr", "foo-abc.d", "libfoo-abc.rlib", "libfoo-abc.rmeta"];
    assert_eq!(
        names,
        eager.map(std::ffi::OsString::from),
        "a promoted entry is laid out as a linked one"
    );
    let ino = |path: PathBuf| fs::metadata(path).unwrap().ino();
    assert_ne!(ino(entry.join("libfoo-abc.rlib")), ino(a.join("libfoo-abc.rlib")));
    let hash = blake3::hash(b"pub fn a() {}").to_hex();
    let hashes = fs::read_to_string(entry.join(".hashes")).unwrap();
    assert!(hashes.contains(&format!("{hash} libfoo-abc.rlib\n")), "{hashes}");

    fs::write(entry.join("libfoo-abc.rlib"), "changed").unwrap();
    assert_eq!(own_build(&fixture, "c", "pub fn a() {}").0, Ok(("miss evicted".to_string(), 0)));
}

#[test]
fn own_crate_that_is_only_checked_records_a_candidate_too() {
    let fixture = lazy("checked");
    rebuilt(&fixture, "a");
    let (root, target) = (fixture.root.join("a"), fixture.worktree("a", "metadata"));
    fs::create_dir_all(root.join(".git"))
        .and_then(|()| fs::create_dir_all(root.join("src")))
        .unwrap();
    write_source(&root.join("src/lib.rs"), "pub fn a() {}");
    let mut args = fixture.args(&target);
    (args[2], args[3]) =
        (root.join("src/lib.rs").display().to_string(), "--emit=dep-info,metadata".into());
    let worktree = Some(root.display().to_string());
    let outcome = fixture.build(&args, &fixture.root.join("store"), worktree);
    assert_eq!(outcome, Ok(("miss".to_string(), 0)));
    assert_eq!((entries(&fixture).len(), candidates(&fixture).len()), (0, 1));
}

#[test]
fn candidate_written_in_place_at_the_same_size_is_dropped() {
    mutated("in-place", |_, rlib| overwrite(rlib, 0, b"q"));
}

#[test]
fn candidate_truncated_is_dropped() {
    mutated("truncated", |_, rlib| {
        fs::File::options().write(true).open(rlib).unwrap().set_len(0).unwrap()
    });
}

#[test]
fn candidate_appended_to_is_dropped() {
    mutated("appended", |_, rlib| overwrite(rlib, 13, b"\n"));
}

#[test]
fn candidate_deleted_is_dropped() {
    mutated("deleted", |_, rlib| fs::remove_file(rlib).unwrap());
}

#[test]
fn candidate_renamed_over_is_dropped() {
    mutated("renamed-over", |_, rlib| {
        fs::write(rlib.with_extension("new"), "pub fn q() {}").unwrap();
        fs::rename(rlib.with_extension("new"), rlib).unwrap();
    });
}

#[test]
fn candidate_written_then_given_back_its_mtime_is_dropped() {
    mutated("touched", |_, rlib| {
        let modified = fs::metadata(rlib).unwrap().modified().unwrap();
        overwrite(rlib, 0, b"q");
        fs::File::options().write(true).open(rlib).unwrap().set_modified(modified).unwrap();
        assert_eq!(fs::metadata(rlib).unwrap().modified().unwrap(), modified);
    });
}

#[test]
fn candidate_whose_mode_changed_is_dropped() {
    mutated("chmod", |_, rlib| {
        fs::set_permissions(rlib, fs::Permissions::from_mode(0o600)).unwrap()
    });
}

#[test]
fn candidate_whose_outputs_a_compile_outside_trucker_replaced_is_dropped() {
    mutated("recompiled", |fixture, rlib| {
        let args = fs::read_to_string(rlib.with_file_name("args")).unwrap();
        let source = args.split(' ').nth(2).unwrap();
        fs::write(source, "pub fn q() {}").unwrap();
        let status = Command::new(&fixture.rustc).args(args.trim_end().split(' ')).status();
        assert!(status.unwrap().success());
        write_source(Path::new(source), "pub fn a() {}");
    });
}

#[test]
fn candidate_whose_worktree_was_removed_is_dropped() {
    mutated("removed", |fixture, _| fs::remove_dir_all(fixture.root.join("a")).unwrap());
}

#[test]
fn concurrent_builds_promote_a_candidate_once() {
    let fixture = lazy("concurrent-promotion");
    rebuilt(&fixture, "a");
    assert_eq!(own_build(&fixture, "a", "pub fn a() {}").0, Ok(("miss".to_string(), 0)));
    let mut outcomes = thread::scope(|scope| {
        let fixture = &fixture;
        let build = |name| scope.spawn(move || own_build(fixture, name, "pub fn a() {}"));
        ["b", "c"].map(build).map(|build| {
            let (outcome, out) = build.join().unwrap();
            assert!(!out.join("runs").exists());
            outcome.unwrap().0
        })
    });
    outcomes.sort_by_key(|outcome| outcome != "hit promoted");
    assert_eq!(outcomes[0], "hit promoted");
    assert!(["hit", "hit waited"].contains(&outcomes[1].as_str()), "{outcomes:?}");
    assert_eq!(entries(&fixture).len(), 1);
}

#[test]
fn gc_removes_a_candidate_whose_outputs_changed_or_that_is_three_days_old() {
    let fixture = lazy("gc-candidates");
    let (store, cut) = (fixture.root.join("store"), crate::now() - 3 * 86400);
    own_build(&fixture, "a", "pub fn a() {}").0.unwrap();
    own_build(&fixture, "a", "pub fn b() {}").0.unwrap();
    assert_eq!(candidates(&fixture).len(), 1, "a compile replaces the candidate of its outputs");
    fs::remove_file(fixture.root.join("a/target/debug/build/foo/abc/out/libfoo-abc.rlib")).unwrap();
    // A run checks the candidates where it sweeps, as it does once a day and where `gc.sizes` is missing.
    let _ = fs::remove_file(store.join("gc.sizes"));
    gc::collect(&store, cut, u64::MAX, false, || (100, 100));
    assert!(candidates(&fixture).is_empty());
    rebuilt(&fixture, "b");
    own_build(&fixture, "b", "pub fn c() {}").0.unwrap();
    fs::remove_file(store.join("gc.sizes")).unwrap();
    gc::collect(&store, cut, u64::MAX, false, || (100, 100));
    let candidate = candidates(&fixture).pop().expect("a current candidate stays");
    age(&candidate, 4 * 86400);
    fs::remove_file(store.join("gc.sizes")).unwrap();
    gc::collect(&store, cut, u64::MAX, false, || (100, 100));
    assert!(candidates(&fixture).is_empty());
}

#[test]
fn store_is_marked_once_as_a_cache_that_backups_skip() {
    let fixture = fixture("mark", "");
    let store = fixture.root.join("store");
    fs::create_dir_all(&store).unwrap();
    store::mark(&store);
    let tag = fs::read_to_string(store.join("CACHEDIR.TAG")).unwrap();
    assert!(tag.starts_with("Signature: 8a477f597d28d172789f06886806bc55\n"), "{tag}");
    #[cfg(target_os = "macos")]
    {
        let excluded = Command::new("tmutil").arg("isexcluded").arg(&store).output().unwrap();
        let excluded = String::from_utf8_lossy(&excluded.stdout);
        assert!(excluded.contains("[Excluded]"), "{excluded}");
    }
    fs::write(store.join("CACHEDIR.TAG"), "kept").unwrap();
    store::mark(&store);
    assert_eq!(
        fs::read_to_string(store.join("CACHEDIR.TAG")).unwrap(),
        "kept",
        "a marked store is left alone"
    );
}

// cargo names the source of a package outside the workspace root by its absolute path, as the fixture does.
#[test]
fn miss_of_an_own_crate_outside_the_workspace_root_is_noted() {
    let fixture = own("outside");
    let (root, target) = (fixture.root.join("a"), fixture.worktree("a", "metadata"));
    fs::create_dir_all(root.join(".git"))
        .and_then(|()| fs::create_dir_all(root.join("src")))
        .unwrap();
    write_source(&root.join("src/lib.rs"), "pub fn a() {}");
    let mut args = fixture.args(&target);
    args[2] = root.join("src/lib.rs").display().to_string();
    let store = fixture.root.join("store");
    // A recent stamp keeps the miss from starting gc, which would run the test binary.
    fs::write(store.join("gc.stamp"), "").unwrap();
    assert_eq!(crate::wrap(&fixture.rustc, &args, &store, fixture.cargo_home.clone()), 0);
    let log = fs::read_to_string(store.join("log")).unwrap();
    let outcomes = Vec::from_iter(log.lines().map(|line| line.splitn(4, ' ').nth(3).unwrap()));
    assert_eq!(outcomes, ["miss", "note outside-workspace"]);

    let note = |source, own, outcome| note(Some(source), own, outcome);
    assert_eq!(note("a/src/lib.rs", true, "miss"), None);
    assert_eq!(note("/repo/other/b/src/lib.rs", true, "hit"), None);
    assert_eq!(note("/home/cargo/registry/src/index/b-1.0.0/src/lib.rs", false, "miss"), None);
}

#[test]
fn a_clone_counts_once_where_the_store_holds_all_of_its_clones() {
    let fixture = fixture("usage-clones", "");
    let (inside, outside) = (fixture.root.join("inside"), fixture.root.join("outside"));
    fs::create_dir_all(&inside).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(inside.join("a"), vec![7u8; 1 << 20]).unwrap();
    if store::clone(&inside.join("a"), &inside.join("b")).is_err() {
        return;
    }
    let usage = crate::usage::measure(&inside);
    assert_eq!(usage.shared.len(), 2, "{usage:?}");
    assert!(usage.shared[0].freed_by(2) && usage.shared[0].key == usage.shared[1].key, "{usage:?}");
    store::clone(&inside.join("a"), &outside.join("c")).unwrap();
    // FIEMAP does not count the holders of an extent, so on XFS and btrfs two files that share one seem to hold it
    // alone.
    let usage = crate::usage::measure(&inside);
    assert_eq!(usage.shared[0].freed_by(2), !cfg!(target_os = "macos"), "{usage:?}");
    let usage = crate::usage::measure(&inside.join("a"));
    assert!(!usage.shared[0].freed_by(1), "{usage:?}");
}
