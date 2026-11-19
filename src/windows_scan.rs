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
