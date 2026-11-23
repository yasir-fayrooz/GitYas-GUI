use std::path::Path;

use super::*;

/// The category of the first span covering `needle` in `text`.
fn category_of(h: &Highlights, text: &str, needle: &str) -> Option<Highlight> {
    let start = text.find(needle)?;
    h.within(start..start + needle.len()).next().map(|s| s.highlight)
}

#[test]
fn every_bundled_grammar_compiles() {
    let broken: Vec<_> = languages::grammar_names().filter(|name| !languages::compiles(name)).collect();
    assert!(broken.is_empty(), "highlight queries that do not compile: {broken:?}");
}

#[test]
fn rust_tokens_get_categories() {
    let text = "// note\nfn main() {\n    let s = \"hi\\n\";\n    let n = 42;\n}\n";
    let h = Highlights::compute(Path::new("src/main.rs"), text);
    assert_eq!(category_of(&h, text, "// note"), Some(Highlight::Comment));
    assert_eq!(category_of(&h, text, "fn"), Some(Highlight::Keyword));
    assert_eq!(category_of(&h, text, "main"), Some(Highlight::Function));
    assert_eq!(category_of(&h, text, "\"hi"), Some(Highlight::String));
    assert_eq!(category_of(&h, text, "\\n"), Some(Highlight::StringEscape));
    assert_eq!(category_of(&h, text, "42"), Some(Highlight::Number));
}

#[test]
fn multi_line_constructs_cover_every_line() {
    let text = "/* first\n   second\n*/\nlet x = 1;\n";
    let h = Highlights::compute(Path::new("a.js"), text);
    let second = text.find("second").unwrap_or(0);
    assert_eq!(h.within(second..second + 6).next().map(|s| s.highlight), Some(Highlight::Comment));
}

#[test]
fn injections_highlight_embedded_languages() {
    let text = "<p>x</p>\n<script>\nconst a = 1;\n</script>\n";
    let h = Highlights::compute(Path::new("index.html"), text);
    assert_eq!(category_of(&h, text, "p>"), Some(Highlight::Tag));
    assert_eq!(category_of(&h, text, "const"), Some(Highlight::Keyword));

    let md = "# Title\n\n```rust\nfn f() {}\n```\n";
    let h = Highlights::compute(Path::new("README.md"), md);
    assert_eq!(category_of(&h, md, "fn"), Some(Highlight::Keyword));
}

#[test]
fn detects_by_file_name_and_extension() {
    let make = "all:\n\techo hi\n";
    assert!(!Highlights::compute(Path::new("Makefile"), make).is_empty());
    assert!(!Highlights::compute(Path::new("dir/CMakeLists.txt"), "project(x)\n").is_empty());
    assert!(!Highlights::compute(Path::new("SRC/MAIN.RS"), "fn main() {}\n").is_empty());
    assert!(Highlights::compute(Path::new("notes.unknownext"), "plain words\n").is_empty());
}

#[test]
fn falls_back_to_sublime_syntaxes() {
    // Kotlin, Dockerfile and a shebang script have no bundled grammar.
    let kt = "fun main() {\n    val s = \"x\"\n}\n";
    let h = Highlights::compute(Path::new("Main.kt"), kt);
    assert_eq!(category_of(&h, kt, "fun"), Some(Highlight::Keyword));
    assert_eq!(category_of(&h, kt, "\"x\""), Some(Highlight::String));

    let docker = "FROM alpine\nRUN echo hi\n";
    let h = Highlights::compute(Path::new("Dockerfile"), docker);
    assert_eq!(category_of(&h, docker, "FROM"), Some(Highlight::Keyword));

    let script = "#!/usr/bin/env perl\nmy $x = 'a';\n";
    let h = Highlights::compute(Path::new("tool"), script);
    assert_eq!(category_of(&h, script, "'a'"), Some(Highlight::String));
}

#[test]
fn within_clips_and_rebases_spans() {
    let h = Highlights {
        spans: vec![
            Span {
                range: 0..4,
                highlight: Highlight::Comment,
            },
            Span {
                range: 6..12,
                highlight: Highlight::String,
            },
        ],
    };
    let got: Vec<_> = h.within(2..8).collect();
    assert_eq!(
        got,
        [
            Span {
                range: 0..2,
                highlight: Highlight::Comment
            },
            Span {
                range: 4..6,
                highlight: Highlight::String
            },
        ]
    );
    assert_eq!(h.within(12..20).count(), 0);
    assert_eq!(h.within(4..6).count(), 0);
}

#[test]
fn push_merges_touching_equal_spans() {
    let mut spans = Vec::new();
    push(&mut spans, 0..2, Highlight::Keyword);
    push(&mut spans, 2..4, Highlight::Keyword);
    push(&mut spans, 4..4, Highlight::String);
    push(&mut spans, 5..6, Highlight::Keyword);
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[0].range, 0..4);
}
