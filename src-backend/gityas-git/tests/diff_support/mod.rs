//! Helpers shared by the diff, conflict and merge-fixture tests. The `git` CLI is used here as a **test-only oracle**.
#![allow(dead_code)]

use std::path::Path;
use std::process::Command;

/// `git` with user/system config ignored, so the oracle is reproducible on any machine.
pub fn git(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("LC_ALL", "C");
    cmd
}

/// Runs git and returns stdout; panics with stderr on failure.
pub fn git_ok(dir: &Path, args: &[&str]) -> Vec<u8> {
    let out = git(dir).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// `git merge-file --diff3 -p` → (number of conflicts, output).
pub fn git_merge_file(base: &str, ours: &str, theirs: &str) -> (i32, String) {
    let dir = tempfile::tempdir().unwrap();
    let p = |n: &str| dir.path().join(n);
    std::fs::write(p("ours"), ours).unwrap();
    std::fs::write(p("base"), base).unwrap();
    std::fs::write(p("theirs"), theirs).unwrap();
    let out = git(dir.path())
        .args([
            "merge-file",
            "--diff3",
            "-p",
            "-L",
            "ours",
            "-L",
            "base",
            "-L",
            "theirs",
            "ours",
            "base",
            "theirs",
        ])
        .output()
        .unwrap();
    let code = out.status.code().unwrap();
    assert!(code >= 0, "git merge-file failed: {}", String::from_utf8_lossy(&out.stderr));
    (code, String::from_utf8(out.stdout).unwrap())
}

/// Deterministic xorshift PRNG for property tests (no extra dev-dependency).
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    pub fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

/// Lines from a small alphabet (lots of repeats, so alignments are ambiguous), incl. blank and Unicode lines.
pub const WORDS: &[&str] = &["a", "b", "c", "}", "", "fn x() {", "é", "日本"];

pub fn gen_lines(r: &mut Rng, max: usize) -> Vec<String> {
    let n = r.below(max + 1);
    (0..n).map(|_| r.pick(WORDS).to_string()).collect()
}

pub fn mutate(r: &mut Rng, base: &[String]) -> Vec<String> {
    let mut out = base.to_vec();
    for _ in 0..r.below(4) {
        let pos = r.below(out.len() + 1);
        match r.below(3) {
            0 => out.insert(pos, format!("x{}", r.below(4))),
            1 if pos < out.len() => {
                out.remove(pos);
            }
            _ if pos < out.len() => out[pos] = format!("y{}", r.below(4)),
            _ => {}
        }
    }
    out
}

/// Joins with `eol`; optionally drops the final newline.
pub fn join(lines: &[String], eol: &str, trailing_newline: bool) -> String {
    let mut s = lines.join(eol);
    if trailing_newline && !lines.is_empty() {
        s.push_str(eol);
    }
    s
}
