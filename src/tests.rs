use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};
use std::{env, fs, thread};

use crate::script::{self, Run, replay_or_run};
use crate::unit::{self, CARGO_HOME, Roots, TARGET, Unit, WORKTREE};
use crate::{cached, gc, stats, store};

/// A directory of one test, with a stand-in rustc and a CARGO_HOME, on the filesystem of the temporary directory.
struct Fixture {
    root: PathBuf,
    rustc: String,
    cargo_home: String,
}

/// The stand-in writes the outputs of a crate `foo`, a dep-info naming `$extra`, its arguments and a line of stderr.
fn fixture(name: &str, extra: &str) -> Fixture {
    let root = env::temp_dir().join(format!("trucker-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let cargo_home = format!("{}/cargo/", root.display());
    fs::create_dir_all(root.join("store/entries")).unwrap();
    let script = format!(
        r#"#!/bin/sh
[ "$1" = -vV ] && {{ printf 'stand-in\nrelease: 1.100.0\n'; exit 0; }}
args="$*"
while [ $# -gt 0 ]; do [ "$1" = --out-dir ] && out=$2; shift; done
echo ran >> "$out/runs"
echo "$args" > "$out/args"
printf rmeta > "$out/libfoo-abc.rmeta"
printf rlib > "$out/libfoo-abc.rlib"
deps="{cargo_home}registry/src/index/foo-1.0.0/src/lib.rs {extra}"
printf '%s/libfoo-abc.rmeta: %s\n# env-dep:CARGO_PKG_NAME=foo\n' "$out" "$deps" > "$out/foo-abc.d"
printf '{{"artifact":"%s/libfoo-abc.rmeta"}}\n' "$out" >&2
"#
    );
    let rustc = root.join("rustc");
    fs::write(&rustc, script).unwrap();
    fs::set_permissions(&rustc, fs::Permissions::from_mode(0o755)).unwrap();
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
        )
    }

    fn key(&self, args: &[String]) -> String {
        self.unit(args).unwrap().key(&self.rustc, args, &self.root.join("store")).unwrap()
    }

    /// A run of the build script `body` in a worktree, with `variables` added, and its OUT_DIR.
    ///
    /// The real script is `build_script_build.real` next to OUT_DIR, and each run appends to `runs` there.
    fn script(&self, name: &str, body: &str, variables: &[(&str, &str)]) -> (Run, PathBuf) {
        let out_dir = self.worktree(name, "metadata").join("debug/build/foo/run/out");
        let directory = PathBuf::from(format!("{}registry/src/index/foo-1.0.0", self.cargo_home));
        fs::create_dir_all(&out_dir).and_then(|()| fs::create_dir_all(&directory)).unwrap();
        let real = out_dir.with_file_name("build_script_build.real");
        fs::write(&real, format!("#!/bin/sh\necho ran >> \"$OUT_DIR/../runs\"\n{body}\n")).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();
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

fn real(out_dir: &Path) -> PathBuf {
    out_dir.with_file_name("build_script_build.real")
}

/// A fixture whose stand-in rustc takes a second after it writes `runs`.
fn slow(name: &str) -> Fixture {
    let fixture = fixture(name, "");
    let script = fs::read_to_string(&fixture.rustc).unwrap();
    fs::write(&fixture.rustc, script.replace("\nprintf rmeta", "\nsleep 1\nprintf rmeta")).unwrap();
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
    assert_eq!(unit.variant("key", ""), "key");
    assert_ne!(unit.variant("key", "env HOME\nenv PATH"), unit.variant("key", "env PATH"));
    assert_eq!(unit.variant("key", "env PATH"), unit.variant("key", "env PATH"));
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
    assert_eq!(unit.variant("key", &reads), same.variant("key", &reads));
    let (changed, _) = fixture.generated(&second, "const N: u8 = 2;", "the second worktree");
    assert_ne!(unit.variant("key", &reads), changed.variant("key", &reads));
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
    // The store's file is older than the dependency in the second worktree, as one from an earlier build is.
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
        (r#"printf '\377%s' "$OUT_DIR" > "$OUT_DIR/foo.o""#, "binary-target-path", 0),
        ("exit 3", "script-failed", 3),
    ];
    for (case, (body, reason, code)) in cases.into_iter().enumerate() {
        let (run, out) = fixture.script(&format!("case-{case}"), body, &[]);
        let outcome = (format!("passthrough {reason}"), code);
        assert_eq!(replay_or_run(&real(&out), Ok(run), &store), outcome, "{body}");
        assert_eq!(fs::read_to_string(out.join("../runs")).unwrap(), "ran\n");
    }
    assert_eq!(fs::read_dir(store.join("scripts")).unwrap().count(), 0);
}

#[test]
fn stand_in_runs_the_real_script_where_a_run_cannot_be_keyed() {
    let fixture = fixture("script-fallback", "");
    let store = fixture.root.join("store");
    let (run, out) = fixture.script("only", "", &[("RUSTC", "/nonexistent/rustc")]);
    // The real script runs with the environment of this process, in which OUT_DIR is unset.
    let runs = fixture.root.join("runs");
    fs::write(real(&out), format!("#!/bin/sh\necho ran >> {}\n", runs.display())).unwrap();
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
    assert!(script::install(&script, Path::new("/dev/null/store")).is_err());
    assert_eq!(fs::read_to_string(&script).unwrap(), "real");
    assert!(!script.with_extension("real").exists());

    script::install(&script, &fixture.root.join("store")).unwrap();
    assert_eq!(fs::read_to_string(script.with_extension("real")).unwrap(), "real");
    let wrapper = fs::metadata(env::current_exe().unwrap()).unwrap();
    assert_eq!(fs::metadata(&script).unwrap().len(), wrapper.len());
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
    assert_eq!(before, format!("{}\n66.7% of the cached invocations hit\n", rows.join("\n")));
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
    let file = fs::File::open(store.join("locks").join(&key)).unwrap();
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
    fs::create_dir_all(store.join("locks")).unwrap();
    for name in ["old", "held", "fresh"] {
        fs::write(store.join("locks").join(name), "").unwrap();
        age(&store.join("locks").join(name), if name == "fresh" { 60 } else { 4 * 86400 });
    }
    let held = fs::File::open(store.join("locks/held")).unwrap();
    held.try_lock().unwrap();
    fs::write(fixture.root.join("build_script_build"), "real").unwrap();
    script::install(&fixture.root.join("build_script_build"), &store).unwrap();
    fs::write(store.join("stand-in/other"), "an older wrapper").unwrap();
    fs::write(store.join("log"), format!("100 foo 2000 miss\n{} foo 1 hit\n", crate::now()))
        .unwrap();

    let collected = gc::collect(&store, crate::now() - 3 * 86400, u64::MAX, || (100, 100));
    assert_eq!(collected.entries, 1);
    let mut kept =
        Vec::from_iter(fs::read_dir(&entries).unwrap().map(|entry| entry.unwrap().file_name()));
    kept.sort();
    assert_eq!(kept, [hit.file_name().unwrap(), "unused.456".as_ref(), "used.reads".as_ref()]);
    assert!(fixture.root.join("rustc").is_file(), "removal must not follow a symlink");
    assert_eq!(fs::read_dir(store.join("stand-in")).unwrap().count(), 1);
    let locks = ["old", "held", "fresh"].map(|name| store.join("locks").join(name).exists());
    assert_eq!(locks, [false, true, true], "a held lock stays");
    assert_eq!(
        stats(&[], &store).unwrap(),
        "      1       0.0 s  hit\n100.0% of the cached invocations hit\n"
    );
}

#[test]
fn gc_removes_the_least_recently_used_over_the_cap_or_the_free_space_but_none_used_in_the_last_hour()
 {
    let fixture = fixture("gc-cap", "");
    let store = fixture.root.join("store");
    let entry = |name: &str, seconds| {
        let entry = store.join("entries").join(name);
        fs::create_dir_all(&entry).and_then(|()| fs::write(entry.join("lib"), [0; 1000])).unwrap();
        age(&entry, seconds);
    };
    entry("oldest", 3 * 3600);
    entry("older", 2 * 3600);
    entry("recent", 1800);
    let size = gc::collect(&store, 0, u64::MAX, || (100, 100)).size;
    let collected = gc::collect(&store, 0, size - 1, || (100, 100));
    assert_eq!(collected, gc::Collected { entries: 1, freed: 1000, size: size - 1000 });
    assert!(!store.join("entries/oldest").exists() && store.join("entries/older").exists());
    let collected = gc::collect(&store, 0, 0, || (100, 100));
    assert_eq!((collected.entries, collected.size), (1, size - 2000));

    entry("unused", 2 * 3600);
    let collected = gc::collect(&store, 0, u64::MAX, || (100, 9));
    assert_eq!(collected.entries, 1, "the free space is below a tenth of the filesystem");
    assert!(store.join("entries/recent").exists());
}

#[test]
fn trigger_starts_at_most_one_gc_an_hour_and_a_second_gc_does_not_run() {
    let fixture = fixture("gc-trigger", "");
    let store = fixture.root.join("store");
    assert!(gc::trigger(&store, Path::new("/usr/bin/true")));
    assert!(!gc::trigger(&store, Path::new("/usr/bin/true")));
    age(&store.join("gc.stamp"), 7200);
    assert!(gc::trigger(&store, Path::new("/usr/bin/true")));

    let lock = fs::File::create(store.join("gc.lock")).unwrap();
    lock.try_lock().unwrap();
    assert_eq!(gc::run(&store), "another trucker gc is running\n");
    drop(lock);
    // A child that another test forks shares the descriptor of the lock until it runs exec.
    let mut reports = std::iter::repeat_with(|| gc::run(&store));
    let report = reports.find(|report| !report.starts_with("another")).unwrap();
    assert!(report.starts_with("removed 0 entries"), "{report}");
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
        fs::write(&compiler, script).unwrap();
        fs::set_permissions(&compiler, fs::Permissions::from_mode(0o755)).unwrap();
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

/// Compiles `foo` as an own crate of the worktree `name`, with `source` in `src/lib.rs`, and returns its output.
fn own_build(
    fixture: &Fixture,
    name: &str,
    source: &str,
) -> (Result<(String, i32), &'static str>, PathBuf) {
    let (root, target) = (fixture.root.join(name), fixture.worktree(name, "metadata"));
    fs::create_dir_all(root.join(".git"))
        .and_then(|()| fs::create_dir_all(root.join("src")))
        .unwrap();
    fs::write(root.join("src/lib.rs"), source).unwrap();
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

    // The second worktree's path has another length, and its incremental directory another name.
    let (outcome, out) = own_build(&fixture, "bbbb", "pub fn a() {}");
    assert_eq!(outcome, Ok(("hit".to_string(), 0)));
    assert!(!out.join("runs").exists(), "rustc must not run on a hit");
    let dep_info = fs::read_to_string(out.join("foo-abc.d")).unwrap();
    assert!(
        dep_info.contains(&format!(" {}/bbbb/src/lib.rs\n", fixture.root.display())),
        "{dep_info}"
    );

    // cargo rebuilds the dependents in a later build where an `.rmeta` is newer, which rustc leaves when unchanged.
    let rmeta = out.join("libfoo-abc.rmeta");
    age(&rmeta, 3600);
    let modified = fs::metadata(&rmeta).unwrap().modified().unwrap();
    assert_eq!(own_build(&fixture, "bbbb", "pub fn a() {}").0, Ok(("hit".to_string(), 0)));
    assert_eq!(fs::metadata(&rmeta).unwrap().modified().unwrap(), modified);

    let (outcome, out) = own_build(&fixture, "bbbb", "pub fn b() {}");
    assert_eq!(outcome, Ok(("miss".to_string(), 0)));
    assert_eq!(fs::read_to_string(out.join("runs")).unwrap(), "ran\n");
}

#[test]
fn own_crate_whose_outputs_contain_the_worktree_path_is_never_stored() {
    let fixture = own("baked");
    let script = fs::read_to_string(&fixture.rustc).unwrap();
    fs::write(
        &fixture.rustc,
        script.replace("printf rlib >", "printf \"rlib ${out%/target/*}\" >"),
    )
    .unwrap();
    let (outcome, _) = own_build(&fixture, "a", "pub fn a() {}");
    assert_eq!(outcome, Ok(("passthrough baked-path".to_string(), 0)));
    assert_eq!(fs::read_dir(fixture.root.join("store/entries")).unwrap().count(), 0);
    // cargo replays the stderr of a fresh crate in every build, so a notice there would recur.
    assert!(!fixture.root.join("store/notices").exists());
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
    for name in
        ["CARGO", "CARGO_PKG_NAME", "CARGO_MANIFEST_DIR", "CARGO_CFG_UNIX", "CARGO_BIN_EXE_app"]
    {
        assert!(unit::is_keyed(name), "{name}");
    }
    for name in ["CARGO_TARGET_DIR", "CARGO_HOME", "CARGO_MAKEFLAGS", "CARGO_BUILD_JOBS"] {
        assert!(!unit::is_keyed(name), "{name}");
    }
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
        fs::write(root.join("src/lib.rs"), "pub fn a() {}").unwrap();
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
