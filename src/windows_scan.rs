//! The scan for baked paths on Windows, which matches every spelling that a path of a root can take in an output.
//!
//! Windows compares paths without regard to ASCII case and accepts `/` for `\`, and JSON doubles each `\`. So a root
//! matches where its components follow each other in any case, separated by one or two of `\` and `/`, in UTF-8 or in
//! UTF-16LE, as a resource or a PDB holds text. The drive is left out, so a path that MSYS spells as `/c/w` matches too.
//! A root must be ASCII, as [`crate::windows::is_canonical`] checks, so ASCII folding covers every match that NTFS makes.

/// The roots that a scan looks for, each as its components after the drive in lower case.
pub struct Scan {
    roots: Vec<Vec<Vec<u8>>>,
}

impl Scan {
    pub fn new(roots: &[&str]) -> Scan {
        let components = |root: &&str| {
            let path = root.split_once(':').map_or(*root, |(_, path)| path);
            let parts = path.split(['\\', '/']).filter(|part| !part.is_empty());
            Vec::from_iter(parts.map(|part| part.to_ascii_lowercase().into_bytes()))
        };
        Scan { roots: Vec::from_iter(roots.iter().map(components)) }
    }

    /// Whether `bytes` contain a spelling of any root, or a root has no components to match.
    pub fn contains(&self, bytes: &[u8]) -> bool {
        self.roots.iter().any(|root| {
            root.is_empty()
                || memchr::memchr2_iter(b'\\', b'/', bytes)
                    .any(|at| matches_at(bytes, at, 1, root) || matches_at(bytes, at, 2, root))
        })
    }
}

/// Whether `root` starts at `at` in `bytes`, whose characters take `width` bytes with zeros after the first.
fn matches_at(bytes: &[u8], mut at: usize, width: usize, root: &[Vec<u8>]) -> bool {
    let unit = |at: usize| {
        let wide = bytes.get(at + 1..at + width)?.iter().all(|byte| *byte == 0);
        bytes.get(at).filter(|_| wide).map(u8::to_ascii_lowercase)
    };
    for component in root {
        let mut separators = 0;
        while separators < 2 && matches!(unit(at), Some(b'\\' | b'/')) {
            (at, separators) = (at + width, separators + 1);
        }
        if separators == 0 {
            return false;
        }
        for byte in component {
            if unit(at) != Some(*byte) {
                return false;
            }
            at += width;
        }
    }
    true
}

// These tests run on every platform, unlike those in `tests.rs`.
#[cfg(test)]
mod tests {
    use super::Scan;

    #[test]
    fn matches_a_root_in_any_case_separator_json_or_utf16() {
        let scan = Scan::new(&[r"C:\Work\tree\target\"]);
        for text in [
            r"x C:\Work\tree\target\debug",
            "x c:/work/TREE/target/debug",
            r#"{"artifact":"C:\\Work\\tree\\target\\debug"}"#,
            r"\\?\C:\Work\tree\target",
            "/c/work/tree/target/out",
            r"C:\Work/tree\\target",
        ] {
            assert!(scan.contains(text.as_bytes()), "{text}");
        }
        let wide =
            Vec::from_iter("D:\\WORK\\Tree\\Target".encode_utf16().flat_map(u16::to_le_bytes));
        assert!(scan.contains(&[b"\x01".as_slice(), &wide].concat()));
        for text in [r"C:\Work\tree\targe", r"C:\Work\treetarget", r"C:\Work\other\target", ""] {
            assert!(!scan.contains(text.as_bytes()), "{text}");
        }
    }

    #[test]
    fn matches_everything_for_a_root_without_components() {
        assert!(Scan::new(&[r"C:\"]).contains(b"nothing"));
    }
}
