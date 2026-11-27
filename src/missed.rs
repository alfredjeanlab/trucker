//! The note that says why a compile missed, and the report of `trucker missed` that ranks the causes in those notes.
//!
//! A stored compile remembers a digest of each part of its key in a memo under `hashes`, named by its normalized
//! arguments, which gc removes as it removes every memo. A later miss with those arguments names the parts whose
//! digests differ. For a tree that differs, it names the files that differ from the earlier tree, where that tree is
//! still on disk with the recorded hash. A note names a path only through the placeholders of the roots, because the
//! bench commits its logs.
//!
//! A miss follows from the notes of the libraries and the trees of other packages that differ, where those notes name
//! every other part that differs too. Any other miss is a cause.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::unit::{self, Parts, Roots, Unit};

/// The start of the outcome field of a note of this module in the log.
pub const NOTE: &str = "note missed ";

/// The most stems that a note names among the libraries that differ, which keeps the line of a large binary short.
const NAMED_LIBRARIES: usize = 8;

/// The most files that a note names among those that differ in one tree.
const NAMED_FILES: usize = 3;

/// The most causes that the report lists, because a person or an agent reads every line.
const LISTED_CAUSES: usize = 5;

/// The most crates that the report names among the dependents of a cause.
const NAMED_DEPENDENTS: usize = 3;

/// The parts of the key of one compile, as a memo of [`Miss::remember`] holds them.
struct Record {
    roots: Roots,
    /// The name of the entry that the reads of the key selected, or an empty string.
    variant: String,
    /// The digest of each part other than the libraries, by the name of the part.
    parts: BTreeMap<String, String>,
    /// The digest of the libraries of each stem, by the [`fnv`] hash of the stem.
    libraries: BTreeMap<u32, u32>,
}

impl Record {
    fn text(&self) -> String {
        let Roots { target, cargo_home, worktree } = &self.roots;
        let mut text = format!(
            "target\t{target}\ncargo-home\t{cargo_home}\nworktree\t{}\nvariant\t{}\n",
            worktree.as_deref().unwrap_or_default(),
            self.variant
        );
        for (name, digest) in &self.parts {
            text.push_str(&format!("part\t{digest}\t{name}\n"));
        }
        let libraries =
            self.libraries.iter().map(|(stem, digest)| format!("{stem:08x}{digest:08x}"));
        text.push_str(&format!("libraries\t{}\n", String::from_iter(libraries)));
        text
    }

    fn parse(text: &str) -> Option<Record> {
        let mut fields = BTreeMap::new();
        let (mut parts, mut libraries) = (BTreeMap::new(), BTreeMap::new());
        for line in text.lines() {
            let (field, value) = line.split_once('\t')?;
            match field {
                "part" => {
                    let (digest, name) = value.split_once('\t')?;
                    parts.insert(name.to_string(), digest.to_string());
                }
                "libraries" => {
                    for pair in value.as_bytes().chunks(16) {
                        let pair =
                            std::str::from_utf8(pair).ok().filter(|pair| pair.len() == 16)?;
                        let number = |hex| u32::from_str_radix(hex, 16).ok();
                        libraries.insert(number(&pair[..8])?, number(&pair[8..])?);
                    }
                }
                _ => {
                    fields.insert(field, value.to_string());
                }
            }
        }
        let mut field = |name| fields.remove(name);
        let roots = Roots {
            target: field("target")?,
            cargo_home: field("cargo-home")?,
            worktree: field("worktree").filter(|worktree| !worktree.is_empty()),
        };
        Some(Record { roots, variant: field("variant")?, parts, libraries })
    }
}

/// The parts of the key of a compile that missed, with what [`Miss::note`] needs to name them.
pub struct Miss<'a> {
    /// The identity of the memo, which is the working directory, arguments and remap flags of the key.
    family: String,
    record: Record,
    /// The stem of each library, by its hash in [`Record::libraries`].
    stems: BTreeMap<u32, &'a str>,
}

/// The first 16 hex digits of the hash of `fields`, each after its length.
fn digest<T: AsRef<[u8]>>(fields: impl IntoIterator<Item = T>) -> String {
    let mut hasher = blake3::Hasher::new();
    for field in fields {
        let field = field.as_ref();
        hasher.update(&(field.len() as u64).to_le_bytes()).update(field);
    }
    hasher.finalize().to_hex()[..16].to_string()
}

/// The crate name and extra filename of `library`, which a note of that crate names as its own stem.
fn stem(library: &Path) -> &str {
    let name = library.file_name().and_then(|name| name.to_str()).unwrap_or_default();
    let name = name.strip_prefix("lib").unwrap_or(name);
    name.rsplit_once('.').map_or(name, |(stem, _)| stem)
}

/// The 32-bit FNV-1a hash of `text`, which names a stem in a memo in 8 hex digits.
///
/// A large binary links hundreds of libraries, so the memo of a stem is short and costs no hasher of its own.
fn fnv(text: &str) -> u32 {
    text.bytes().fold(0x811c_9dc5, |hash, byte| (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193))
}

impl<'a> Miss<'a> {
    /// The `parts` of a key of `unit`, whose reads selected the entry named `variant`.
    pub fn of(unit: &Unit, parts: &'a Parts, variant: &str) -> Miss<'a> {
        let family = format!("missed\n{}", parts.arguments.join("\n"));
        let mut named = BTreeMap::from([("rustc".to_string(), digest([&parts.compiler]))]);
        for (name, value) in &parts.variables {
            named.insert(format!("env {name}"), digest([value]));
        }
        for (tree, hash) in &parts.trees {
            named.insert(
                format!("tree {tree}"),
                blake3::Hash::from_bytes(*hash).to_hex().to_string(),
            );
        }
        for (path, hash) in &parts.named {
            named.insert(
                format!("named {path}"),
                blake3::Hash::from_bytes(*hash).to_hex().to_string(),
            );
        }
        for (link, hash) in &parts.links {
            let hash = hash.as_ref().map_or(&b"default"[..], |hash| hash.as_bytes());
            named.insert(format!("link {link}"), digest([hash]));
        }
        // The libraries of a stem are digested by the start of each one's hash, which the key computed already.
        let (mut libraries, mut stems) = (BTreeMap::new(), BTreeMap::new());
        for (library, hash) in parts.libraries.iter().zip(&parts.hashes) {
            let (stem, start) = (stem(library), hash.as_bytes());
            stems.insert(fnv(stem), stem);
            let start = u32::from_le_bytes([start[0], start[1], start[2], start[3]]);
            *libraries.entry(fnv(stem)).or_insert(0) ^= start;
        }
        let roots = Roots {
            target: unit.roots.target.clone(),
            cargo_home: unit.roots.cargo_home.clone(),
            worktree: unit.roots.worktree.clone(),
        };
        let record = Record { roots, variant: variant.to_string(), parts: named, libraries };
        Miss { family, record, stems }
    }

    /// Remembers these parts in `memos`, for the next miss of the same crate and arguments.
    pub fn remember(&self, memos: &Path) {
        let _ = unit::remember(memos, &self.family, self.record.text().as_bytes());
    }

    /// The note that names what differs from the compile that `memos` remembers for the same crate and arguments.
    ///
    /// It is `None` where no compile of them is remembered. A miss whose key equals the remembered one names `sources`
    /// where its reads select another entry, and `gone` where they select the remembered one, which was then removed.
    pub fn note(&self, stem: &str, memos: &Path) -> Option<String> {
        let earlier = String::from_utf8(unit::recalled(memos, &self.family)?).ok();
        let earlier = Record::parse(&earlier?)?;
        let (current, rank) = (&self.record, |name: &str| match name.split(' ').next() {
            Some("rustc") => 0,
            Some("env") => 2,
            Some("tree") => 4,
            _ => 5,
        });
        let names = BTreeSet::from_iter(current.parts.keys().chain(earlier.parts.keys()));
        let mut names = Vec::from_iter(
            names.into_iter().filter(|name| current.parts.get(*name) != earlier.parts.get(*name)),
        );
        names.sort_by_key(|name| rank(name));
        let mut reasons = Vec::new();
        for name in names {
            reasons.push(match name.strip_prefix("tree ") {
                Some(tree) => self.tree(tree, &earlier, memos),
                None => name.clone(),
            });
        }
        let changed = current
            .libraries
            .iter()
            .filter(|(name, digest)| earlier.libraries.get(*name) != Some(*digest));
        let mut changed =
            Vec::from_iter(changed.filter_map(|(name, _)| self.stems.get(name).copied()));
        changed.sort();
        let removed =
            earlier.libraries.keys().filter(|name| !current.libraries.contains_key(*name));
        let named = Vec::from_iter(changed.iter().take(NAMED_LIBRARIES).copied());
        let more = changed.len() - named.len() + removed.count();
        if named.len() + more > 0 {
            let at = reasons.iter().position(|reason| rank(reason) > 2).unwrap_or(reasons.len());
            reasons.insert(at, format!("libraries {}", joined(&named, more)));
        }
        if reasons.is_empty() {
            reasons.push(
                if current.variant == earlier.variant { "gone" } else { "sources" }.to_string(),
            );
        }
        let origin = origin(&current.roots, &earlier.roots);
        Some(format!("{NOTE}{stem} against {origin}: {}", reasons.join("; ")))
    }

    /// The reason that names the files of `tree` that differ from those of the earlier compile.
    ///
    /// The earlier tree is compared only where it is still on disk and hashes as recorded, because a build in its
    /// target may have written it since.
    fn tree(&self, tree: &str, earlier: &Record, memos: &Path) -> String {
        let current = &self.record.roots;
        let (here, there) = (current.localized(tree), earlier.roots.localized(tree));
        if here == there {
            return format!("tree {tree}, which this target rewrote since");
        }
        let mut hash = None;
        let mut fed = |bytes: &[u8]| hash = Some(blake3::Hash::from_slice(bytes).ok());
        if unit::hash_tree(&earlier.roots, Path::new(&there), memos, &mut fed).is_err() {
            return format!("tree {tree}, whose earlier copy is gone");
        }
        let recorded = earlier.parts.get(&format!("tree {tree}"));
        if hash.flatten().map(|hash| hash.to_hex().to_string()).as_ref() != recorded {
            return format!("tree {tree}, whose earlier copy changed since");
        }
        let entries = |roots, root: &str| unit::tree_entries(roots, Path::new(root), memos);
        let (Ok(now), Ok(then)) = (entries(current, &here), entries(&earlier.roots, &there)) else {
            return format!("tree {tree}");
        };
        let paths = BTreeSet::<&PathBuf>::from_iter(now.keys().chain(then.keys()));
        let differing = paths.into_iter().filter(|path| now.get(*path) != then.get(*path));
        // A control character in the name of a file would end the line of the note.
        let name = |path: &PathBuf| path.to_string_lossy().replace(char::is_control, "?");
        let differing = Vec::from_iter(differing.map(name));
        let named = Vec::from_iter(differing.iter().take(NAMED_FILES).map(String::as_str));
        match differing.len() {
            0 => format!("tree {tree}"),
            count => format!("tree {tree} in {}", joined(&named, count - named.len())),
        }
    }
}

/// `names` joined by commas, followed by the count of `more` that are left out.
fn joined(names: &[&str], more: usize) -> String {
    match (names.join(", "), more) {
        (names, 0) => names,
        (names, more) if names.is_empty() => format!("{more} more"),
        (names, more) => format!("{names} and {more} more"),
    }
}

/// Where the earlier compile ran, as seen from the target directory of `current`.
fn origin(current: &Roots, earlier: &Roots) -> &'static str {
    let repository = |roots: &Roots| {
        let target = Path::new(roots.target.trim_end_matches(std::path::MAIN_SEPARATOR));
        unit::repository(target).map(Path::to_path_buf)
    };
    match (repository(current), repository(earlier)) {
        _ if current.target == earlier.target => "this target",
        (Some(a), Some(b)) if a == b => "another target of this worktree",
        (Some(_), Some(_)) => "another worktree",
        _ => "another target",
    }
}

/// A note of the log.
struct Note<'a> {
    time: u64,
    name: &'a str,
    milliseconds: u64,
    stem: &'a str,
    origin: &'a str,
    reasons: Vec<&'a str>,
}

impl<'a> Note<'a> {
    fn parse(line: &'a str) -> Option<Note<'a>> {
        let mut fields = line.splitn(4, ' ');
        let (time, name) = (fields.next()?.parse().ok()?, fields.next()?);
        let milliseconds = fields.next()?.parse().ok()?;
        let (stem, rest) = fields.next()?.strip_prefix(NOTE)?.split_once(" against ")?;
        let (origin, reasons) = rest.split_once(": ")?;
        Some(Note {
            time,
            name,
            milliseconds,
            stem,
            origin,
            reasons: reasons.split("; ").collect(),
        })
    }

    /// The stems of the libraries that differ.
    fn libraries(&self) -> Vec<&'a str> {
        let named = self.reasons.iter().find_map(|reason| reason.strip_prefix("libraries "));
        let named = named.map(|named| named.split(" and ").next().unwrap_or(named));
        named.into_iter().flat_map(|named| named.split(", ")).collect()
    }

    /// What each reason other than the libraries says differs, without what the comparison found.
    fn subjects(&self) -> impl Iterator<Item = &'a str> {
        let subject = |reason: &'a str| match reason.strip_prefix("tree ") {
            Some(tree) => &reason[..5 + tree.find([',', ' ']).unwrap_or(tree.len())],
            None => reason,
        };
        self.reasons
            .iter()
            .filter(|reason| !reason.starts_with("libraries "))
            .map(move |reason| subject(reason))
    }

    /// The reasons as a clause of a sentence.
    fn clause(&self) -> String {
        let clauses = Vec::from_iter(self.reasons.iter().map(|reason| clause(reason)));
        let clauses = match clauses.split_last() {
            Some((last, [])) => last.clone(),
            Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
            None => String::new(),
        };
        match self.reasons[..] {
            ["gone"] => clauses,
            _ => format!("{clauses} from {}", place(self.origin)),
        }
    }
}

/// The compile that a note of `origin` was compared with, in plain words.
fn place(origin: &str) -> String {
    match origin {
        "this target" => "the last compile here".to_string(),
        "another target of this worktree" => {
            "the compile in another target folder of this worktree".to_string()
        }
        "another target" => "the compile in another target folder".to_string(),
        _ => format!("the compile in {origin}"),
    }
}

/// `reason` as a clause of a sentence.
fn clause(reason: &str) -> String {
    let (kind, rest) = reason.split_once(' ').unwrap_or((reason, ""));
    match kind {
        "rustc" => "the compiler differs".to_string(),
        "env" => format!("the environment variable {rest} differs"),
        "link" => format!("the native library {rest} differs"),
        "named" => format!("the file {rest} that an argument names differs"),
        "libraries" => format!("the dependencies {rest} differ"),
        "sources" => "a source or variable that it read differs".to_string(),
        "gone" => "its saved copy was removed from the cache".to_string(),
        // A tree is a directory that the compile searches for libraries.
        _ => match rest.split_once(" in ") {
            Some((tree, files)) if files.contains(", ") => {
                format!("the files {files} in the library folder {tree} differ")
            }
            Some((tree, file)) => format!("the file {file} in the library folder {tree} differs"),
            None => match rest.split_once(", ") {
                Some((tree, state)) => {
                    let state = match state {
                        "which this target rewrote since" => "which a later build rewrote here",
                        state => state,
                    };
                    format!("the library folder {tree}, {state}, differs")
                }
                None => format!("the library folder {rest} differs"),
            },
        },
    }
}

/// The time of `start` as an age, or the whole log where it is 0.
fn since(start: u64) -> String {
    let age = crate::now().saturating_sub(start);
    match start {
        0 => "in the log".to_string(),
        _ => format!("in the last {}", ago(age)),
    }
}

fn ago(seconds: u64) -> String {
    match seconds {
        0..90 => format!("{seconds} s"),
        90..5400 => format!("{} min", seconds / 60),
        5400..129_600 => format!("{} h", seconds / 3600),
        _ => format!("{} d", seconds / 86400),
    }
}

fn seconds(milliseconds: u64) -> f64 {
    milliseconds as f64 / 1000.0
}

/// The package whose build directory holds the tree that `subject` names, if it names one.
fn owner(subject: &str) -> Option<&str> {
    let mut components = subject.strip_prefix("tree ")?.split('/');
    components.find(|component| *component == "build")?;
    components.next()
}

/// The causes of the misses that `notes` describe, as the index of each note's causes.
///
/// A note follows from the latest earlier note of each library that differs, and of each tree of another package that
/// differs, because cargo passes the link search directories of a build script to every dependent. It is a cause
/// itself where it follows from no note, or where it differs in a part that none of those notes names.
fn causes(notes: &[Note]) -> Vec<BTreeSet<usize>> {
    let (mut latest, mut named) = (HashMap::<&str, usize>::new(), HashMap::<&str, usize>::new());
    let (mut causes, mut subjects) = (Vec::<BTreeSet<usize>>::new(), Vec::<BTreeSet<&str>>::new());
    for (at, note) in notes.iter().enumerate() {
        let libraries = note.libraries().into_iter().filter_map(|stem| latest.get(stem));
        let package = |subject: &str| owner(subject).map(|owner| owner.replace('-', "_"));
        let trees = note
            .subjects()
            .filter(|subject| package(subject).is_some_and(|package| package != note.name));
        let upstream =
            Vec::from_iter(libraries.chain(trees.filter_map(|tree| named.get(tree))).copied());
        let inherited =
            BTreeSet::from_iter(upstream.iter().flat_map(|&up| subjects[up].iter().copied()));
        let follows =
            !upstream.is_empty() && note.subjects().all(|subject| inherited.contains(subject));
        causes.push(match follows {
            true => upstream.iter().flat_map(|&up| causes[up].iter().copied()).collect(),
            false => BTreeSet::from([at]),
        });
        subjects.push(inherited.into_iter().chain(note.subjects()).collect());
        named.extend(note.subjects().map(|subject| (subject, at)));
        latest.insert(note.stem, at);
    }
    causes
}

/// The report of `trucker missed`, from the log of `store`, or `None` where `args` are not valid.
///
/// Without a crate it ranks the causes of the misses since `--since` by the time that they and the misses that followed
/// from them took. With a crate it says why its newest miss missed. With `--duplicates` it is the report of
/// [`crate::duplicates::report`], which reads every entry rather than the log, so the ranking leaves it out.
pub fn report(args: &[String], store: &Path) -> Option<String> {
    let (mut start, mut name, mut duplicates, mut args) = (0, None, false, args.iter());
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--since" => start = crate::since(args.next()?)?,
            "--duplicates" => duplicates = true,
            _ if arg.starts_with('-') || name.is_some() => return None,
            _ => name = Some(arg.as_str()),
        }
    }
    match (duplicates, name) {
        (true, Some(_)) => return None,
        (true, None) => return Some(crate::duplicates::report(&store.join("entries"), start)),
        _ => {}
    }
    let log = std::fs::read_to_string(store.join("log")).unwrap_or_default();
    let lines = log.lines().filter(|line| {
        line.split(' ')
            .next()
            .and_then(|time| time.parse::<u64>().ok())
            .is_some_and(|time| time >= start)
    });
    let (lines, misses): (Vec<&str>, Vec<&str>) = lines
        .filter(|line| line.contains(NOTE) || line.contains(" miss"))
        .partition(|line| line.contains(NOTE));
    let notes = Vec::from_iter(lines.into_iter().filter_map(Note::parse));
    let causes = causes(&notes);
    Some(match name {
        Some(name) => newest(&notes, &causes, name, start),
        None => {
            let misses = misses
                .iter()
                .filter(|line| line.splitn(4, ' ').nth(3).is_some_and(|o| o.starts_with("miss")));
            ranked(&notes, &causes, misses.count(), start)
        }
    })
}

/// The time that the misses of `notes` at `indices` took.
fn took<'a>(notes: &[Note], indices: impl IntoIterator<Item = &'a usize>) -> u64 {
    indices.into_iter().map(|&at| notes[at].milliseconds).sum()
}

/// The causes in `notes` ranked by the time that they and their dependents took, of `misses` in the log.
///
/// A dependent of several causes counts in the row of each.
fn ranked(notes: &[Note], causes: &[BTreeSet<usize>], misses: usize, start: u64) -> String {
    // The causes of one crate that name the same parts form one row, whatever the compile that each was compared with.
    let mut rows = BTreeMap::<(&str, Vec<&str>), (Vec<usize>, BTreeSet<usize>)>::new();
    for (at, causes) in causes.iter().enumerate() {
        for &cause in causes {
            let key = (notes[cause].name, notes[cause].reasons.clone());
            let (roots, dependents) = rows.entry(key).or_default();
            if at == cause {
                roots.push(at);
            } else {
                dependents.insert(at);
            }
        }
    }
    let mut rows = Vec::from_iter(rows.into_values().map(|(roots, dependents)| {
        (took(notes, &roots) + took(notes, &dependents), roots, dependents)
    }));
    rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let followed = causes.iter().enumerate().filter(|(at, causes)| !causes.contains(at)).count();
    let lost = seconds(notes.iter().map(|note| note.milliseconds).sum());
    let count = notes.len();
    let mut text = match count {
        0 => format!("no miss {} could be compared with an earlier compile\n", since(start)),
        _ => format!(
            "{count} misses {} took {lost:.1} s and could be compared with an earlier compile\n",
            since(start)
        ),
    };
    if followed > 0 {
        let _ = writeln!(
            text,
            "{followed} of these {count} missed only because a crate that they use missed first"
        );
    }
    if misses > count {
        let _ = writeln!(
            text,
            "{} other misses ran a build script or had no recent compile to compare with",
            misses - count
        );
    }
    let listed = rows.iter().take(LISTED_CAUSES);
    let width = listed.clone().map(|(_, roots, _)| notes[roots[0]].name.len()).max().unwrap_or(0);
    for (total, roots, dependents) in listed {
        let newest = &notes[*roots.last().unwrap_or(&0)];
        let times = match roots.len() {
            1 => "once".to_string(),
            count => format!("{count} times"),
        };
        let _ = writeln!(
            text,
            "{:>8.1} s  {:<width$} | missed {times}, because {}",
            seconds(*total),
            newest.name,
            newest.clause()
        );
        if !dependents.is_empty() {
            let names = BTreeSet::from_iter(dependents.iter().map(|&at| notes[at].name));
            let named = Vec::from_iter(names.iter().take(NAMED_DEPENDENTS).copied());
            let more = names.len() - named.len();
            let crates = match names.len() {
                1 => "1 crate that uses it".to_string(),
                count => format!("{count} crates that use it"),
            };
            let times = match dependents.len() {
                count if count == names.len() => String::new(),
                count => format!(" {count} times"),
            };
            let time = seconds(took(notes, dependents));
            let _ = writeln!(
                text,
                "            {:width$} | then {crates} missed{times} in {time:.1} s: {}",
                "",
                joined(&named, more)
            );
        }
    }
    if rows.len() > LISTED_CAUSES {
        let rest = &rows[LISTED_CAUSES..];
        let time = seconds(rest.iter().map(|(total, _, _)| total).sum());
        let _ = writeln!(text, "and {} more causes took {time:.1} s", rest.len());
    }
    text
}

/// Why the newest miss of the crate `name` in `notes` missed, and the causes that it followed from.
fn newest(notes: &[Note], causes: &[BTreeSet<usize>], name: &str, start: u64) -> String {
    let Some(at) = notes.iter().rposition(|note| note.name == name) else {
        return format!(
            "no miss of {name} {} could be compared with an earlier compile\n",
            since(start)
        );
    };
    let note = &notes[at];
    let age = ago(crate::now().saturating_sub(note.time));
    let upstream =
        Vec::from_iter(causes[at].iter().filter(|&&cause| cause != at).map(|&at| &notes[at]));
    let width =
        upstream.iter().map(|cause| cause.name.len()).chain([name.len()]).max().unwrap_or(0);
    let mut text = format!(
        "{name:<width$} | missed {age} ago in {:.1} s, because {}\n",
        seconds(note.milliseconds),
        note.clause()
    );
    for cause in upstream {
        let _ = writeln!(
            text,
            "{:<width$} | missed first and led to this miss, because {}",
            cause.name,
            cause.clause()
        );
    }
    text
}
