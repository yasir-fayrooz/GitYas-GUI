//! Shared `git2 <-> gityas_core` conversions and hashing used by more than one module. Existing
//! signatures and the hash definition must not change: diffs and hunk staging both depend on them.

use gityas_core::{GitYasError, Oid, Result};

use crate::git_error;

pub fn oid_from_git(oid: git2::Oid) -> Result<Oid> {
    Oid::from_bytes(oid.as_bytes())
}

pub fn oid_to_git(oid: &Oid) -> Result<git2::Oid> {
    git2::Oid::from_bytes(oid.as_bytes()).map_err(git_error)
}

/// Stable 64-bit FNV-1a, used where a hash must be identical across runs and processes (not `DefaultHasher`).
#[derive(Debug, Clone, Copy)]
pub struct StableHasher(u64);

impl Default for StableHasher {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl StableHasher {
    pub fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }

    pub fn finish(&self) -> u64 {
        self.0
    }
}

/// `HunkRef::content_hash`: produced by the diff and verified before hunk stage/unstage/discard,
/// so both sides must call this. Feed every line of the hunk in order as `(origin, content)`, where `origin` is
/// the libgit2 line origin char (`' '`, `'+'`, `'-'`, `'='`, `'>'`, `'<'`) and `content` the raw line bytes
/// including any trailing newline. The header and line numbers are deliberately excluded (they are compared
/// separately via `HunkRef`'s ranges).
pub fn hunk_content_hash<'a>(lines: impl IntoIterator<Item = (char, &'a [u8])>) -> u64 {
    let mut h = StableHasher::default();
    for (origin, content) in lines {
        let mut buf = [0u8; 4];
        h.write(origin.encode_utf8(&mut buf).as_bytes());
        h.write(&(content.len() as u64).to_le_bytes());
        h.write(content);
    }
    h.finish()
}

/// Context lines of every displayed / stageable hunk (`get_file_diff` and hunk staging).
pub const DIFF_CONTEXT_LINES: u32 = 3;

/// Applies the options that decide **hunk boundaries** (context, inter-hunk merging, indent heuristic). The diff builds
/// `FileDiff` hunks with these and hunk staging must re-compute the diff with the same options before verifying a
/// `HunkRef::content_hash`, otherwise the same edit yields different hunks and staging reports `StaleHunk`.
/// `indent_heuristic` is on to match the `git diff` CLI default (it is the test oracle).
pub fn apply_hunk_diff_options(opts: &mut git2::DiffOptions) -> &mut git2::DiffOptions {
    opts.context_lines(DIFF_CONTEXT_LINES).interhunk_lines(0).indent_heuristic(true)
}

/// Repository-relative path as the bytes git stores (`/` separators). Used to match `DiffDelta` paths exactly
/// (byte-wise, so non-UTF-8 paths work on Unix) and as a literal pathspec.
pub fn repo_path_bytes(path: &std::path::Path) -> Result<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(path.as_os_str().as_bytes().to_vec())
    }
    #[cfg(not(unix))]
    {
        Ok(path_to_str(path)?.replace('\\', "/").into_bytes())
    }
}

/// Borrows a path as `&str` for APIs that need it (git2 pathspecs); non-UTF-8 paths are `InvalidInput`.
pub fn path_to_str(path: &std::path::Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| GitYasError::InvalidInput(format!("path is not valid UTF-8: {}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oid_round_trip() -> Result<()> {
        let g = git2::Oid::from_str("0123456789abcdef0123456789abcdef01234567").map_err(git_error)?;
        assert_eq!(oid_to_git(&oid_from_git(g)?)?, g);
        Ok(())
    }

    #[test]
    fn hunk_hash_is_stable_and_order_sensitive() {
        let a = hunk_content_hash([('-', b"x\n".as_slice()), ('+', b"y\n".as_slice())]);
        let b = hunk_content_hash([('+', b"y\n".as_slice()), ('-', b"x\n".as_slice())]);
        assert_ne!(a, b);
        // Pinned value: changing the definition breaks staging of hunks hashed by another build.
        assert_eq!(a, 0x1bf0_f586_eb6f_9e48);
        assert_ne!(
            hunk_content_hash([('+', b"ab".as_slice())]),
            hunk_content_hash([('+', b"a".as_slice()), ('+', b"b".as_slice())])
        );
    }
}
