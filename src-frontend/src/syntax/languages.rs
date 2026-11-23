//! The bundled tree-sitter grammars: which files use them, their highlight queries and the capture names we
//! colour. A grammar's [`HighlightConfiguration`] (query compilation, up to ~0.5 s for the biggest grammars) is
//! built on first use and kept for the life of the process.

use std::path::Path;
use std::sync::OnceLock;

use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};
use tree_sitter_language::LanguageFn;

use super::{Highlight, Span, push};

/// Capture names we recognise. tree-sitter-highlight gives each query capture the entry with the most matching
/// dot-separated parts (`function.method.call` → `function.method`); captures that match nothing stay plain.
const CAPTURES: &[(&str, Highlight)] = &[
    ("attribute", Highlight::Attribute),
    ("tag.attribute", Highlight::Attribute),
    ("boolean", Highlight::Boolean),
    ("comment", Highlight::Comment),
    ("comment.doc", Highlight::CommentDoc),
    ("comment.documentation", Highlight::CommentDoc),
    ("constant", Highlight::Constant),
    // Literals such as `true`, `nil` and (in tree-sitter-rust) numbers: coloured like numbers.
    ("constant.builtin", Highlight::Number),
    ("constant.character", Highlight::String),
    ("character", Highlight::String),
    ("constant.numeric", Highlight::Number),
    ("number", Highlight::Number),
    ("float", Highlight::Number),
    ("constructor", Highlight::Constructor),
    ("embedded", Highlight::Embedded),
    ("emphasis", Highlight::Emphasis),
    ("text.emphasis", Highlight::Emphasis),
    ("emphasis.strong", Highlight::EmphasisStrong),
    ("text.strong", Highlight::EmphasisStrong),
    ("enum", Highlight::Enum),
    ("function", Highlight::Function),
    ("method", Highlight::Function),
    ("keyword", Highlight::Keyword),
    ("conditional", Highlight::Keyword),
    ("repeat", Highlight::Keyword),
    ("include", Highlight::Keyword),
    ("exception", Highlight::Keyword),
    ("storageclass", Highlight::Keyword),
    ("keyword.operator", Highlight::Keyword),
    ("label", Highlight::Label),
    ("text.reference", Highlight::LinkText),
    ("link_text", Highlight::LinkText),
    ("text.uri", Highlight::LinkUri),
    ("link_uri", Highlight::LinkUri),
    ("string.special.uri", Highlight::LinkUri),
    ("module", Highlight::Namespace),
    ("namespace", Highlight::Namespace),
    ("operator", Highlight::Operator),
    ("preproc", Highlight::Preproc),
    ("keyword.directive", Highlight::Preproc),
    ("property", Highlight::Property),
    ("field", Highlight::Property),
    ("variable.member", Highlight::Property),
    ("punctuation", Highlight::Punctuation),
    ("delimiter", Highlight::PunctuationDelimiter),
    ("punctuation.bracket", Highlight::PunctuationBracket),
    ("punctuation.delimiter", Highlight::PunctuationDelimiter),
    ("punctuation.list_marker", Highlight::PunctuationListMarker),
    ("punctuation.special", Highlight::PunctuationSpecial),
    ("string", Highlight::String),
    ("string.escape", Highlight::StringEscape),
    ("escape", Highlight::StringEscape),
    ("string.regex", Highlight::StringRegex),
    ("string.regexp", Highlight::StringRegex),
    ("string.special", Highlight::StringSpecial),
    ("string.special.key", Highlight::Property),
    ("string.special.symbol", Highlight::StringSpecial),
    ("tag", Highlight::Tag),
    ("text.literal", Highlight::TextLiteral),
    ("text.title", Highlight::Title),
    ("title", Highlight::Title),
    ("type", Highlight::Type),
    ("type.builtin", Highlight::Type),
    ("variable", Highlight::Variable),
    ("variable.builtin", Highlight::VariableSpecial),
    ("variable.parameter", Highlight::VariableParameter),
    ("parameter", Highlight::VariableParameter),
    ("variable.special", Highlight::VariableSpecial),
    ("variant", Highlight::Variant),
    ("addition", Highlight::DiffPlus),
    ("deletion", Highlight::DiffMinus),
];

/// One bundled grammar.
pub(super) struct Grammar {
    /// Name used by other grammars' injection queries (and Markdown fence info strings).
    name: &'static str,
    aliases: &'static [&'static str],
    /// Lower-case extensions, without the dot.
    extensions: &'static [&'static str],
    /// Whole file names, matched case-insensitively.
    file_names: &'static [&'static str],
    build: fn() -> Option<HighlightConfiguration>,
}

fn config(name: &str, language: LanguageFn, highlights: &str, injections: &str, locals: &str) -> Option<HighlightConfiguration> {
    match HighlightConfiguration::new(language.into(), name, highlights, injections, locals) {
        Ok(mut config) => {
            config.configure(&CAPTURES.iter().map(|(name, _)| *name).collect::<Vec<_>>());
            Some(config)
        }
        Err(err) => {
            tracing::warn!("syntax: the {name} highlight query does not compile: {err:?}");
            None
        }
    }
}

const GRAMMARS: &[Grammar] = &[
    Grammar {
        name: "rust",
        aliases: &["rs"],
        extensions: &["rs"],
        file_names: &[],
        build: || {
            use tree_sitter_rust as l;
            config("rust", l::LANGUAGE, l::HIGHLIGHTS_QUERY, l::INJECTIONS_QUERY, "")
        },
    },
    Grammar {
        name: "python",
        aliases: &["py", "python3"],
        extensions: &["py", "pyi", "pyw"],
        file_names: &["SConstruct", "SConscript"],
        build: || {
            use tree_sitter_python as l;
            config("python", l::LANGUAGE, l::HIGHLIGHTS_QUERY, "", "")
        },
    },
    Grammar {
        name: "javascript",
        aliases: &["js", "jsx", "node"],
        extensions: &["js", "mjs", "cjs", "jsx"],
        file_names: &[],
        build: || {
            use tree_sitter_javascript as l;
            let highlights = format!("{}\n{}", l::HIGHLIGHT_QUERY, l::JSX_HIGHLIGHT_QUERY);
            config("javascript", l::LANGUAGE, &highlights, l::INJECTIONS_QUERY, l::LOCALS_QUERY)
        },
    },
    Grammar {
        name: "typescript",
        aliases: &["ts"],
        extensions: &["ts", "mts", "cts"],
        file_names: &[],
        build: || {
            use tree_sitter_typescript as l;
            // The TypeScript query only adds to JavaScript's; the two are layered.
            let highlights = format!("{}\n{}", l::HIGHLIGHTS_QUERY, tree_sitter_javascript::HIGHLIGHT_QUERY);
            config(
                "typescript",
                l::LANGUAGE_TYPESCRIPT,
                &highlights,
                tree_sitter_javascript::INJECTIONS_QUERY,
                l::LOCALS_QUERY,
            )
        },
    },
    Grammar {
        name: "tsx",
        aliases: &[],
        extensions: &["tsx"],
        file_names: &[],
        build: || {
            use tree_sitter_typescript as l;
            let highlights = format!(
                "{}\n{}\n{}",
                l::HIGHLIGHTS_QUERY,
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY
            );
            config(
                "tsx",
                l::LANGUAGE_TSX,
                &highlights,
                tree_sitter_javascript::INJECTIONS_QUERY,
                l::LOCALS_QUERY,
            )
        },
    },
    Grammar {
        name: "go",
        aliases: &["golang"],
        extensions: &["go"],
        file_names: &[],
        build: || {
            use tree_sitter_go as l;
            config("go", l::LANGUAGE, l::HIGHLIGHTS_QUERY, "", "")
        },
    },
    Grammar {
        name: "c",
        aliases: &[],
        extensions: &["c", "h"],
        file_names: &[],
        build: || {
            use tree_sitter_c as l;
            config("c", l::LANGUAGE, l::HIGHLIGHT_QUERY, "", "")
        },
    },
    Grammar {
        name: "cpp",
        aliases: &["c++", "cxx"],
        extensions: &[
            "cpp", "cc", "cxx", "c++", "hpp", "hh", "hxx", "h++", "ipp", "inl", "tpp", "ino", "cu", "cuh",
        ],
        file_names: &[],
        build: || {
            use tree_sitter_cpp as l;
            // Like TypeScript over JavaScript, the C++ query extends C's.
            let highlights = format!("{}\n{}", l::HIGHLIGHT_QUERY, tree_sitter_c::HIGHLIGHT_QUERY);
            config("cpp", l::LANGUAGE, &highlights, "", "")
        },
    },
    Grammar {
        name: "c_sharp",
        aliases: &["csharp", "cs", "c#"],
        extensions: &["cs", "csx"],
        file_names: &[],
        build: || {
            use tree_sitter_c_sharp as l;
            config("c_sharp", l::LANGUAGE, l::HIGHLIGHTS_QUERY, "", "")
        },
    },
    Grammar {
        name: "java",
        aliases: &[],
        extensions: &["java"],
        file_names: &[],
        build: || {
            use tree_sitter_java as l;
            config("java", l::LANGUAGE, l::HIGHLIGHTS_QUERY, "", "")
        },
    },
    Grammar {
        name: "json",
        aliases: &["jsonc", "json5"],
        extensions: &["json", "jsonc", "json5", "webmanifest", "code-workspace", "ipynb"],
        file_names: &[".babelrc", ".eslintrc", ".prettierrc", ".swcrc", "flake.lock", "composer.lock"],
        build: || {
            use tree_sitter_json as l;
            config("json", l::LANGUAGE, l::HIGHLIGHTS_QUERY, "", "")
        },
    },
    Grammar {
        name: "html",
        aliases: &["htm", "xhtml"],
        extensions: &["html", "htm", "xhtml", "shtml"],
        file_names: &[],
        build: || {
            use tree_sitter_html as l;
            config("html", l::LANGUAGE, l::HIGHLIGHTS_QUERY, l::INJECTIONS_QUERY, "")
        },
    },
    Grammar {
        name: "css",
        aliases: &[],
        extensions: &["css"],
        file_names: &[],
        build: || {
            use tree_sitter_css as l;
            config("css", l::LANGUAGE, l::HIGHLIGHTS_QUERY, "", "")
        },
    },
    Grammar {
        name: "bash",
        aliases: &["sh", "shell", "zsh", "ksh", "shellscript", "console"],
        extensions: &["sh", "bash", "zsh", "ksh", "bats", "ebuild", "eclass"],
        file_names: &[
            ".bashrc",
            ".bash_profile",
            ".bash_aliases",
            ".bash_logout",
            ".profile",
            ".zshrc",
            ".zshenv",
            ".zprofile",
            ".zlogin",
            ".envrc",
            "PKGBUILD",
            "APKBUILD",
        ],
        build: || {
            use tree_sitter_bash as l;
            config("bash", l::LANGUAGE, l::HIGHLIGHT_QUERY, "", "")
        },
    },
    Grammar {
        name: "ruby",
        aliases: &["rb"],
        extensions: &["rb", "rake", "gemspec", "ru", "rbw", "jbuilder", "podspec"],
        file_names: &[
            "Gemfile",
            "Rakefile",
            "Podfile",
            "Vagrantfile",
            "Brewfile",
            "Guardfile",
            "Fastfile",
            "Dangerfile",
        ],
        build: || {
            use tree_sitter_ruby as l;
            config("ruby", l::LANGUAGE, l::HIGHLIGHTS_QUERY, "", l::LOCALS_QUERY)
        },
    },
    Grammar {
        name: "php",
        aliases: &[],
        extensions: &["php", "phtml", "php3", "php4", "php5", "php7", "php8", "phps"],
        file_names: &[],
        build: || {
            use tree_sitter_php as l;
            config("php", l::LANGUAGE_PHP, l::HIGHLIGHTS_QUERY, l::INJECTIONS_QUERY, "")
        },
    },
    Grammar {
        name: "toml",
        aliases: &[],
        extensions: &["toml"],
        file_names: &["Cargo.lock", "Pipfile", "poetry.lock", "uv.lock", "Gopkg.lock"],
        build: || {
            use tree_sitter_toml_ng as l;
            config("toml", l::LANGUAGE, l::HIGHLIGHTS_QUERY, "", "")
        },
    },
    Grammar {
        name: "yaml",
        aliases: &["yml"],
        extensions: &["yaml", "yml"],
        file_names: &[".clang-format", ".clang-tidy", ".clangd", "pnpm-lock.yaml"],
        build: || {
            use tree_sitter_yaml as l;
            config("yaml", l::LANGUAGE, l::HIGHLIGHTS_QUERY, "", "")
        },
    },
    Grammar {
        name: "markdown",
        aliases: &["md"],
        extensions: &["md", "markdown", "mdown", "mkd", "mkdn"],
        file_names: &[],
        build: || {
            use tree_sitter_md as l;
            config("markdown", l::LANGUAGE, l::HIGHLIGHT_QUERY_BLOCK, l::INJECTION_QUERY_BLOCK, "")
        },
    },
    Grammar {
        // Injected by `markdown` into paragraphs and headings.
        name: "markdown_inline",
        aliases: &[],
        extensions: &[],
        file_names: &[],
        build: || {
            use tree_sitter_md as l;
            config(
                "markdown_inline",
                l::INLINE_LANGUAGE,
                l::HIGHLIGHT_QUERY_INLINE,
                l::INJECTION_QUERY_INLINE,
                "",
            )
        },
    },
    Grammar {
        name: "lua",
        aliases: &[],
        extensions: &["lua", "luau", "rockspec"],
        file_names: &[".luacheckrc"],
        build: || {
            use tree_sitter_lua as l;
            config("lua", l::LANGUAGE, l::HIGHLIGHTS_QUERY, l::INJECTIONS_QUERY, l::LOCALS_QUERY)
        },
    },
    Grammar {
        name: "swift",
        aliases: &[],
        extensions: &["swift"],
        file_names: &[],
        build: || {
            use tree_sitter_swift as l;
            config("swift", l::LANGUAGE, l::HIGHLIGHTS_QUERY, l::INJECTIONS_QUERY, l::LOCALS_QUERY)
        },
    },
    Grammar {
        name: "scala",
        aliases: &[],
        extensions: &["scala", "sc", "sbt", "mill"],
        file_names: &[],
        build: || {
            use tree_sitter_scala as l;
            config("scala", l::LANGUAGE, l::HIGHLIGHTS_QUERY, "", l::LOCALS_QUERY)
        },
    },
    Grammar {
        name: "haskell",
        aliases: &["hs"],
        extensions: &["hs", "hs-boot"],
        file_names: &[],
        build: || {
            use tree_sitter_haskell as l;
            config("haskell", l::LANGUAGE, l::HIGHLIGHTS_QUERY, l::INJECTIONS_QUERY, l::LOCALS_QUERY)
        },
    },
    Grammar {
        name: "elixir",
        aliases: &["ex", "exs"],
        extensions: &["ex", "exs"],
        file_names: &[],
        build: || {
            use tree_sitter_elixir as l;
            config("elixir", l::LANGUAGE, l::HIGHLIGHTS_QUERY, l::INJECTIONS_QUERY, "")
        },
    },
    Grammar {
        name: "zig",
        aliases: &[],
        extensions: &["zig", "zon"],
        file_names: &[],
        build: || {
            use tree_sitter_zig as l;
            config("zig", l::LANGUAGE, l::HIGHLIGHTS_QUERY, l::INJECTIONS_QUERY, "")
        },
    },
    Grammar {
        name: "ocaml",
        aliases: &["ml"],
        extensions: &["ml"],
        file_names: &[],
        build: || {
            use tree_sitter_ocaml as l;
            config("ocaml", l::LANGUAGE_OCAML, l::HIGHLIGHTS_QUERY, "", l::LOCALS_QUERY)
        },
    },
    Grammar {
        name: "nix",
        aliases: &[],
        extensions: &["nix"],
        file_names: &[],
        build: || {
            use tree_sitter_nix as l;
            config("nix", l::LANGUAGE, l::HIGHLIGHTS_QUERY, l::INJECTIONS_QUERY, "")
        },
    },
    Grammar {
        name: "make",
        aliases: &["makefile"],
        extensions: &["mk", "mak", "make"],
        file_names: &["Makefile", "GNUmakefile", "makefile", "Kbuild"],
        build: || {
            use tree_sitter_make as l;
            config("make", l::LANGUAGE, l::HIGHLIGHTS_QUERY, "", "")
        },
    },
    Grammar {
        name: "xml",
        aliases: &["svg", "xsl"],
        extensions: &[
            "xml",
            "svg",
            "xsd",
            "xsl",
            "xslt",
            "plist",
            "csproj",
            "fsproj",
            "vbproj",
            "vcxproj",
            "props",
            "targets",
            "resx",
            "xaml",
            "axaml",
            "nuspec",
            "storyboard",
            "xib",
            "wsdl",
            "rss",
            "atom",
            "kml",
            "gpx",
            "pom",
        ],
        file_names: &[],
        build: || {
            use tree_sitter_xml as l;
            config("xml", l::LANGUAGE_XML, l::XML_HIGHLIGHT_QUERY, "", "")
        },
    },
    Grammar {
        name: "r",
        aliases: &[],
        extensions: &["r", "rprofile"],
        file_names: &[".Rprofile"],
        build: || {
            use tree_sitter_r as l;
            config("r", l::LANGUAGE, l::HIGHLIGHTS_QUERY, "", l::LOCALS_QUERY)
        },
    },
    Grammar {
        name: "erlang",
        aliases: &["erl"],
        extensions: &["erl", "hrl", "escript", "app.src"],
        file_names: &["rebar.config"],
        build: || {
            use tree_sitter_erlang as l;
            config("erlang", l::LANGUAGE, l::HIGHLIGHTS_QUERY, "", "")
        },
    },
    Grammar {
        name: "elm",
        aliases: &[],
        extensions: &["elm"],
        file_names: &[],
        build: || {
            use tree_sitter_elm as l;
            config("elm", l::LANGUAGE, l::HIGHLIGHTS_QUERY, l::INJECTIONS_QUERY, l::LOCALS_QUERY)
        },
    },
    Grammar {
        name: "dart",
        aliases: &[],
        extensions: &["dart"],
        file_names: &[],
        build: || {
            use tree_sitter_dart as l;
            config("dart", l::LANGUAGE, l::HIGHLIGHTS_QUERY, "", "")
        },
    },
    Grammar {
        name: "sql",
        aliases: &["postgresql", "mysql", "sqlite", "plsql"],
        extensions: &["sql", "psql", "pgsql", "ddl", "dml"],
        file_names: &[],
        build: || {
            use tree_sitter_sequel as l;
            config("sql", l::LANGUAGE, l::HIGHLIGHTS_QUERY, "", "")
        },
    },
    Grammar {
        name: "cmake",
        aliases: &[],
        extensions: &["cmake"],
        file_names: &["CMakeLists.txt"],
        build: || {
            use tree_sitter_cmake as l;
            config("cmake", l::LANGUAGE, l::HIGHLIGHTS_QUERY, "", "")
        },
    },
    Grammar {
        name: "glsl",
        aliases: &[],
        extensions: &["glsl", "vert", "frag", "geom", "comp", "tesc", "tese", "vs", "fs", "gs"],
        file_names: &[],
        build: || {
            use tree_sitter_glsl as l;
            config("glsl", l::LANGUAGE_GLSL, l::HIGHLIGHTS_QUERY, "", "")
        },
    },
    Grammar {
        name: "diff",
        aliases: &["patch"],
        extensions: &["diff", "patch", "rej"],
        file_names: &[],
        build: || {
            use tree_sitter_diff as l;
            config("diff", l::LANGUAGE, l::HIGHLIGHTS_QUERY, "", "")
        },
    },
    Grammar {
        name: "regex",
        aliases: &[],
        extensions: &[],
        file_names: &[],
        build: || {
            use tree_sitter_regex as l;
            config("regex", l::LANGUAGE, l::HIGHLIGHTS_QUERY, "", "")
        },
    },
    Grammar {
        name: "powershell",
        aliases: &["ps1", "pwsh", "ps"],
        extensions: &["ps1", "psm1", "psd1"],
        file_names: &[],
        build: || {
            use tree_sitter_powershell as l;
            config("powershell", l::LANGUAGE, l::HIGHLIGHTS_QUERY, "", "")
        },
    },
    Grammar {
        name: "fsharp",
        aliases: &["f#"],
        extensions: &["fs", "fsx", "fsi"],
        file_names: &[],
        build: || {
            use tree_sitter_fsharp as l;
            config("fsharp", l::LANGUAGE_FSHARP, l::HIGHLIGHTS_QUERY, "", "")
        },
    },
    Grammar {
        name: "objc",
        aliases: &["objective-c", "objectivec"],
        extensions: &["m"],
        file_names: &[],
        build: || {
            use tree_sitter_objc as l;
            config("objc", l::LANGUAGE, l::HIGHLIGHTS_QUERY, "", "")
        },
    },
    Grammar {
        name: "svelte",
        aliases: &[],
        extensions: &["svelte"],
        file_names: &[],
        build: || {
            use tree_sitter_svelte_ng as l;
            config("svelte", l::LANGUAGE, l::HIGHLIGHTS_QUERY, l::INJECTIONS_QUERY, "")
        },
    },
    Grammar {
        name: "gleam",
        aliases: &[],
        extensions: &["gleam"],
        file_names: &[],
        build: || {
            use tree_sitter_gleam as l;
            config("gleam", l::LANGUAGE, l::HIGHLIGHT_QUERY, "", "")
        },
    },
];

static CONFIGS: [OnceLock<Option<HighlightConfiguration>>; GRAMMARS.len()] = [const { OnceLock::new() }; GRAMMARS.len()];

fn configuration(grammar: usize) -> Option<&'static HighlightConfiguration> {
    let build = GRAMMARS.get(grammar)?.build;
    CONFIGS.get(grammar)?.get_or_init(build).as_ref()
}

/// The grammar for the file at `path`, by file name, then extension.
pub(super) fn detect(path: &Path) -> Option<usize> {
    let name = path.file_name()?.to_str()?;
    if let Some(i) = GRAMMARS
        .iter()
        .position(|g| g.file_names.iter().any(|f| f.eq_ignore_ascii_case(name)))
    {
        return Some(i);
    }
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    GRAMMARS.iter().position(|g| g.extensions.contains(&ext.as_str()))
}

/// The grammar an injection (or a Markdown fence) names.
fn by_name(name: &str) -> Option<usize> {
    let name = name.trim().to_ascii_lowercase();
    GRAMMARS.iter().position(|g| g.name == name || g.aliases.contains(&name.as_str()))
}

/// Highlights `text` with `grammar` (and the grammars it injects). `None` when the grammar's query does not compile
/// or highlighting fails.
pub(super) fn highlight(grammar: usize, text: &str) -> Option<Vec<Span>> {
    let config = configuration(grammar)?;
    let mut highlighter = Highlighter::new();
    let events = highlighter
        .highlight(config, text.as_bytes(), None, |name| by_name(name).and_then(configuration))
        .ok()?;
    let mut spans = Vec::new();
    let mut stack: Vec<Highlight> = Vec::new();
    for event in events {
        match event.ok()? {
            HighlightEvent::HighlightStart(h) => stack.push(CAPTURES.get(h.0)?.1),
            HighlightEvent::HighlightEnd => {
                stack.pop();
            }
            HighlightEvent::Source { start, end } => {
                if let Some(&h) = stack.last() {
                    push(&mut spans, start..end, h);
                }
            }
        }
    }
    Some(spans)
}

#[cfg(test)]
pub(super) fn grammar_names() -> impl Iterator<Item = &'static str> {
    GRAMMARS.iter().map(|g| g.name)
}

#[cfg(test)]
pub(super) fn compiles(name: &str) -> bool {
    by_name(name).and_then(configuration).is_some()
}
