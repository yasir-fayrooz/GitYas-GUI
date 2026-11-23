//! Fallback for languages without a bundled tree-sitter grammar: bat's Sublime syntax set (`two-face`, ~210
//! syntaxes) parsed with `syntect`'s pure-Rust regex engine. Sublime scopes (`keyword.control.rust`,
//! `string.quoted.double`, …) map onto the same [`Highlight`] categories as tree-sitter captures.

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

use super::{Highlight, Span, push};

/// Lines longer than this end highlighting (regex-based parsing of minified files is slow).
const MAX_LINE: usize = 16 * 1024;

/// Scope prefixes, most specific first; the innermost scope with a match decides a token's category.
const SCOPES: &[(&str, Highlight)] = &[
    ("comment.block.documentation", Highlight::CommentDoc),
    ("comment.line.documentation", Highlight::CommentDoc),
    ("punctuation.definition.comment", Highlight::Comment),
    ("comment", Highlight::Comment),
    ("punctuation.definition.string", Highlight::String),
    ("constant.character.escape", Highlight::StringEscape),
    ("string.regexp", Highlight::StringRegex),
    ("string.other.link", Highlight::LinkText),
    ("string", Highlight::String),
    ("constant.numeric", Highlight::Number),
    ("constant.language.boolean", Highlight::Boolean),
    ("constant.character", Highlight::String),
    ("constant.other.symbol", Highlight::StringSpecial),
    ("constant", Highlight::Constant),
    ("keyword.operator.word", Highlight::Keyword),
    ("keyword.operator", Highlight::Operator),
    ("keyword", Highlight::Keyword),
    ("storage.type.function", Highlight::Keyword),
    ("storage.modifier", Highlight::Keyword),
    ("storage.type", Highlight::Type),
    ("storage", Highlight::Keyword),
    ("entity.name.function", Highlight::Function),
    ("support.function", Highlight::Function),
    ("variable.function", Highlight::Function),
    ("entity.name.tag", Highlight::Tag),
    ("entity.other.attribute-name", Highlight::Attribute),
    ("entity.name.namespace", Highlight::Namespace),
    ("entity.name.module", Highlight::Namespace),
    ("entity.name.label", Highlight::Label),
    ("entity.name.section", Highlight::Title),
    ("entity.name.enum", Highlight::Enum),
    ("entity.name", Highlight::Type),
    ("entity.other.inherited-class", Highlight::Type),
    ("support.type", Highlight::Type),
    ("support.class", Highlight::Type),
    ("support.constant", Highlight::Constant),
    ("support.variable", Highlight::VariableSpecial),
    ("variable.parameter", Highlight::VariableParameter),
    ("variable.language", Highlight::VariableSpecial),
    ("variable.other.member", Highlight::Property),
    ("variable.other.property", Highlight::Property),
    ("variable.other.constant", Highlight::Constant),
    ("variable", Highlight::Variable),
    ("meta.preprocessor", Highlight::Preproc),
    ("markup.heading", Highlight::Title),
    ("markup.bold", Highlight::EmphasisStrong),
    ("markup.italic", Highlight::Emphasis),
    ("markup.raw", Highlight::TextLiteral),
    ("markup.underline.link", Highlight::LinkUri),
    ("markup.inserted", Highlight::DiffPlus),
    ("markup.deleted", Highlight::DiffMinus),
    ("punctuation.definition.list", Highlight::PunctuationListMarker),
    ("punctuation.section", Highlight::PunctuationBracket),
    ("punctuation.separator", Highlight::PunctuationDelimiter),
    ("punctuation.terminator", Highlight::PunctuationDelimiter),
    ("punctuation.accessor", Highlight::PunctuationDelimiter),
    ("punctuation", Highlight::Punctuation),
];

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(two_face::syntax::extra_newlines)
}

fn find<'a>(set: &'a SyntaxSet, path: &Path, text: &str) -> Option<&'a SyntaxReference> {
    let name = path.file_name().and_then(|n| n.to_str());
    let ext = path.extension().and_then(|e| e.to_str());
    // Sublime syntaxes list whole file names (`Dockerfile`, `.gitignore`) among their "extensions".
    let syntax = name
        .and_then(|n| set.find_syntax_by_extension(n))
        .or_else(|| ext.and_then(|e| set.find_syntax_by_extension(e)))
        .or_else(|| ext.and_then(|e| set.find_syntax_by_extension(&e.to_ascii_lowercase())))
        .or_else(|| set.find_syntax_by_first_line(text.lines().next().unwrap_or("")))?;
    (syntax.name != "Plain Text").then_some(syntax)
}

/// The category of the innermost scope that has one.
fn category(stack: &ScopeStack, cache: &mut HashMap<Scope, Option<Highlight>>) -> Option<Highlight> {
    stack.as_slice().iter().rev().find_map(|scope| {
        *cache.entry(*scope).or_insert_with(|| {
            let name = scope.build_string();
            SCOPES
                .iter()
                .find(|(prefix, _)| name == *prefix || name.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('.')))
                .map(|(_, h)| *h)
        })
    })
}

/// Highlights `text` with the Sublime syntax for `path`; `None` when no syntax matches.
pub(super) fn highlight(path: &Path, text: &str) -> Option<Vec<Span>> {
    let set = syntaxes();
    let syntax = find(set, path, text)?;
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut cache = HashMap::new();
    let mut spans = Vec::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if line.len() > MAX_LINE {
            break;
        }
        let Ok(ops) = state.parse_line(line, set) else { break };
        let mut pos = 0;
        for (at, op) in ops {
            if let Some(h) = category(&stack, &mut cache) {
                push(&mut spans, offset + pos..offset + at, h);
            }
            if stack.apply(&op).is_err() {
                return Some(spans);
            }
            pos = at;
        }
        if let Some(h) = category(&stack, &mut cache) {
            push(&mut spans, offset + pos..offset + line.len(), h);
        }
        offset += line.len();
    }
    Some(spans)
}
