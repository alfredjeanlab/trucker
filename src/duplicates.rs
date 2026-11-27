//! The report of the crates that workspaces sharing the store compile again for other features or versions.
//!
//! Each rustc entry records the package, version, workspace, compile time and arguments of its compile. A comparison
//! of two compiles leaves out `-C metadata`, `-C extra-filename`, `--extern`, `-L` and `--check-cfg`, because they
//! differ in every compile. A difference is reported only between workspaces, because one workspace resolves other
//! features for `cargo test -p` of one package. The report also names the own crates that rustc compiles slowly with
//! ThinLTO between their codegen units.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::path::Path;
use std::time::UNIX_EPOCH;
use std::{env, fs};

use crate::session;
use crate::store::RECORD;
use crate::unit::{self, Unit};

/// The least time in milliseconds of the slowest compile of an own crate for its ThinLTO to be reported.
const THIN_LTO_REPORTED: u64 = 10_000;

/// The lines of the record of a compile of `unit` with `args` that took `milliseconds`.
///
/// The workspace is the target directory relative to the nearest ancestor with a `.git`, so worktrees of one
/// repository share it. An incremental compile is recorded with `-C incremental` and without its directory.
pub fn record(unit: &Unit, args: &[String], milliseconds: u128) -> String {
    let target = Path::new(unit.roots.target.trim_end_matches('/'));
    let repository = target.parent().and_then(unit::repository);
    let workspace = repository.and_then(|root| target.strip_prefix(root).ok()).unwrap_or(target);
    let package = |name| env::var(name).unwrap_or_default();
    let fields = [package("CARGO_PKG_NAME"), package("CARGO_PKG_VERSION")];
    let fields =
        fields.into_iter().chain([workspace.display().to_string(), milliseconds.to_string()]);
    let incremental = args.iter().any(|arg| arg.starts_with("incremental="));
    let incremental = incremental.then(|| ["-C".to_string(), "incremental".to_string()]);
    Vec::from_iter(
        fields.chain(unit.normalized_args(args)).chain(incremental.into_iter().flatten()),
    )
    .join("\n")
}

/// A recorded compile whose arguments are split into its features and the rest.
struct Compile {
    /// The package, followed by the crate where it is not the library, such as a build script.
    package: String,
    version: String,
    workspace: String,
    milliseconds: u64,
    rest: Vec<String>,
    features: BTreeSet<String>,
    args: Vec<String>,
}

fn compile(record: &str) -> Option<Compile> {
    let mut lines = record.lines().peekable();
    let (package, version, workspace) = (lines.next()?, lines.next()?, lines.next()?);
    let milliseconds = lines.next()?.parse().ok()?;
    let args = Vec::from_iter(lines.clone().map(str::to_string));
    let (mut rest, mut features, mut name) = (Vec::new(), BTreeSet::new(), "");
    while let Some(flag) = lines.next() {
        let paired = flag.starts_with('-') && !flag.contains('=');
        let value = lines.next_if(|value| paired && !value.starts_with('-')).unwrap_or("");
        let feature = value.strip_prefix("feature=").filter(|_| flag == "--cfg");
        if let Some(feature) = feature {
            features.insert(feature.trim_matches('"').to_string());
        } else if !(["-L", "--extern", "--check-cfg"].contains(&flag)
            || flag == "-C"
                && (value.starts_with("metadata=")
                    || value.starts_with("extra-filename=")
                    || value == "incremental"))
        {
            name = if flag == "--crate-name" { value } else { name };
            rest.push(format!("{flag} {value}"));
        }
    }
    let library = name == package.replace('-', "_");
    Some(Compile {
        package: if library { package.to_string() } else { format!("{package} {name}") },
        version: version.to_string(),
        workspace: workspace.to_string(),
        milliseconds,
        rest,
        features,
        args,
    })
}

/// Whether `compile` is an incremental one of an own crate that links and runs ThinLTO between its codegen units.
fn thin_lto(compile: &Compile) -> bool {
    let args = &compile.args;
    let input = args.iter().find(|arg| arg.ends_with(".rs") && !arg.starts_with('-'));
    let own = input.is_some_and(|input| !input.starts_with(unit::CARGO_HOME));
    let emit = args.iter().filter_map(|arg| arg.strip_prefix("--emit="));
    let linked = emit.flat_map(|kinds| kinds.split(',')).any(|kind| kind == "link");
    let incremental = args.windows(2).any(|pair| pair == ["-C", "incremental"]);
    own && linked && incremental && session::local_thin_lto(args)
}

/// The version that a semver-compatible version starts with, such as `1` for 1.4.2 and `0.3` for 0.3.7.
fn compatible(version: &str) -> String {
    let parts = Vec::from_iter(version.split('.'));
    parts[..(1 + usize::from(parts[0] == "0")).min(parts.len())].join(".")
}

/// The report of `trucker missed --duplicates` on the compiles in `entries` that were stored at or after `start`.
///
/// One crate is reported where different workspaces compiled it with other features and otherwise equal arguments,
/// or at semver-compatible versions with equal arguments other than its features and sources. Its extra time is that
/// of every variant except the costliest. Two variants come from different workspaces where neither one's workspaces
/// include all of the other's.
///
/// A second section names each own crate whose slowest stored compile took [`THIN_LTO_REPORTED`] or more under
/// [`thin_lto`]. Its profile avoids that with `lto = "off"`. Only an incremental compile counts, because a release
/// build is not incremental.
pub fn report(entries: &Path, start: u64) -> String {
    let stored =
        |path: &Path| fs::metadata(path).ok()?.modified().ok()?.duration_since(UNIX_EPOCH).ok();
    let records = fs::read_dir(entries)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path().join(RECORD));
    let records =
        records.filter(|path| stored(path).is_some_and(|stored| stored.as_secs() >= start));
    let compiles =
        Vec::from_iter(records.filter_map(|path| compile(&fs::read_to_string(path).ok()?)));
    let mut groups = BTreeMap::<_, Vec<(BTreeSet<String>, &Compile)>>::new();
    for compile in &compiles {
        let group = ("features", &compile.package, compile.version.clone(), compile.rest.clone());
        groups.entry(group).or_default().push((compile.features.clone(), compile));
        let flags = Vec::from_iter(compile.rest.iter().filter(|arg| arg.starts_with('-')).cloned());
        let group = ("versions", &compile.package, compatible(&compile.version), flags);
        groups.entry(group).or_default().push((BTreeSet::from([compile.version.clone()]), compile));
    }
    let mut rows = Vec::new();
    for ((class, package, version, _), group) in groups {
        let mut variants = BTreeMap::<_, (u64, BTreeSet<&str>)>::new();
        for (variant, compile) in group {
            let (milliseconds, workspaces) = variants.entry(variant).or_default();
            *milliseconds = compile.milliseconds.max(*milliseconds);
            workspaces.insert(&compile.workspace);
        }
        let apart = |a: &BTreeSet<&str>, b: &BTreeSet<&str>| !a.is_subset(b) && !b.is_subset(a);
        if !variants.values().any(|(_, a)| variants.values().any(|(_, b)| apart(a, b))) {
            continue;
        }
        let costs = variants.values().map(|(milliseconds, _)| *milliseconds);
        let extra = costs.clone().sum::<u64>() - costs.max().unwrap_or(0);
        let names = BTreeSet::from_iter(variants.keys().flatten().map(String::as_str));
        let differing =
            names.into_iter().filter(|name| !variants.keys().all(|names| names.contains(*name)));
        let differing = Vec::from_iter(differing).join(", ");
        let workspaces =
            BTreeSet::from_iter(variants.values().flat_map(|(_, names)| names.iter().copied()));
        let workspaces = Vec::from_iter(workspaces).join(", ");
        let class = if class == "features" { "with features" } else { "at versions" };
        rows.push((extra, format!("{package} {version}  {class} {differing}  in {workspaces}")));
    }
    rows.sort_by(|a, b| b.cmp(a));
    let seconds = |milliseconds: u64| milliseconds as f64 / 1000.0;
    let total = seconds(compiles.iter().map(|compile| compile.milliseconds).sum());
    let extra = seconds(rows.iter().map(|(extra, _)| extra).sum());
    let mut text = format!(
        "{total:.1} s went to the saved compiles, and {extra:.1} s of it to other features or versions\n"
    );
    for (extra, row) in rows {
        let _ = writeln!(text, "{:>8.1} s  {row}", seconds(extra));
    }
    let mut slowest = BTreeMap::<_, u64>::new();
    for compile in compiles.iter().filter(|compile| thin_lto(compile)) {
        let time = slowest.entry((&compile.package, &compile.workspace)).or_default();
        *time = compile.milliseconds.max(*time);
    }
    let mut slow =
        Vec::from_iter(slowest.into_iter().filter(|(_, time)| *time >= THIN_LTO_REPORTED));
    slow.sort_by_key(|(_, time)| std::cmp::Reverse(*time));
    if !slow.is_empty() {
        text.push_str(
            "\nYour crates that run ThinLTO between their codegen units, which slows every build and edit of them, \
             unless their profile sets lto = \"off\":\n",
        );
    }
    for ((package, workspace), time) in slow {
        let _ = writeln!(text, "{:>8.1} s  {package}  in {workspace}", seconds(time));
    }
    text
}
