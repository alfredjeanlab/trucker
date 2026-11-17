use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use std::{env, fs};

use crate::unit::{CARGO_HOME, Roots, TARGET, Unit};
use crate::{cached, store};

/// A directory of one test, with a stand-in rustc and a CARGO_HOME, on the filesystem of the temporary directory.
struct Fixture {
    root: PathBuf,
    rustc: String,
    cargo_home: String,
}

/// The stand-in writes the outputs of a crate `foo`, a dep-info that names `$extra`, and one line of stderr.
fn fixture(name: &str, extra: &str) -> Fixture {
    let root = env::temp_dir().join(format!("crate-cache-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let cargo_home = format!("{}/cargo/", root.display());
    fs::create_dir_all(root.join("store/entries")).unwrap();
    let script = format!(
        r#"#!/bin/sh
[ "$1" = -vV ] && {{ echo "stand-in 1.0"; exit 0; }}
while [ $# -gt 0 ]; do [ "$1" = --out-dir ] && out=$2; shift; done
echo ran >> "$out/runs"
printf rmeta > "$out/libfoo-abc.rmeta"
printf rlib > "$out/libfoo-abc.rlib"
printf '%s/libfoo-abc.rmeta: {cargo_home}registry/src/index/foo-1.0.0/src/lib.rs {extra}\n# env-dep:CARGO_PKG_NAME=foo\n' "$out" > "$out/foo-abc.d"
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
        fs::write(target.join("CACHEDIR.TAG"), "").unwrap();
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
        Unit::model(args, self.cargo_home.clone(), None)
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
        (Unit::model(&self.args(target), self.cargo_home.clone(), Some(out_dir)).unwrap(), dep_info)
    }

    fn key(&self, args: &[String]) -> String {
        self.unit(args).unwrap().key(&self.rustc, args, &self.root.join("store")).unwrap()
    }
}

#[test]
fn roots_round_trip_and_leave_sibling_directories_alone() {
    let roots = Roots {
        target: "/work/a/target/".to_string(),
        cargo_home: "/home/dev/.cargo/".to_string(),
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

#[test]
fn key_is_shared_by_worktrees_and_changes_with_each_input() {
    let fixture = fixture("key", "");
    let key = |name: &str, dependency: &str| {
        fixture.key(&fixture.args(&fixture.worktree(name, dependency)))
    };
    let first = key("first", "metadata");
    assert_eq!(first, key("second", "metadata"));
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
    assert_eq!(with(&["--print=cfg"]).err(), Some("query"));
    assert_eq!(with(&["-"]).err(), Some("stdin-input"));
    assert_eq!(with(&["-C", "incremental=/tmp/incremental"]).err(), Some("incremental"));
    assert_eq!(with(&["-L", "native=/usr/local/lib"]).err(), Some("search-path"));
    assert_eq!(fixture.unit(&["-vV".to_string()]).err(), Some("query"));
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
    let build = |target: &Path| {
        cached(&fixture.rustc, &fixture.args(target), &store, fixture.cargo_home.clone())
    };

    assert_eq!(build(&first), Ok(("miss".to_string(), 0)));
    let entry = fs::read_dir(store.join("entries")).unwrap().next().unwrap().unwrap().path();
    let stored = entry.join("libfoo-abc.rlib");
    // The store's file is older than the dependency in the second worktree, as one from an earlier build is.
    fs::File::open(&stored)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(3600))
        .unwrap();

    assert_eq!(build(&second), Ok(("hit".to_string(), 0)));
    assert!(!out(&second).join("runs").exists(), "rustc must not run on a hit");
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
    let build =
        || cached(&fixture.rustc, &fixture.args(&target), &store, fixture.cargo_home.clone());
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

    let build = || {
        cached(
            &fixture.rustc,
            &fixture.args(&target),
            Path::new("/dev"),
            fixture.cargo_home.clone(),
        )
    };
    assert_eq!(build(), Err("other-filesystem"));
    assert_eq!(build(), Err("other-filesystem"));
    let is_notice =
        |name: &std::ffi::OsStr| name.to_string_lossy().starts_with(".crate-cache-notice-");
    let notices = fs::read_dir(&target)
        .unwrap()
        .filter(|entry| is_notice(&entry.as_ref().unwrap().file_name()));
    assert_eq!(notices.count(), 1);
    assert!(
        !target.join("debug/build/foo/abc/out/runs").exists(),
        "the caller runs rustc, uncached"
    );
}
