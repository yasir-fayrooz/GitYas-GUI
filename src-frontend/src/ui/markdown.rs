//! Minimal Markdown renderer for the What's new dialog (`CHANGELOG.md`): headings, paragraphs, nested lists, bold /
//! italic / strikethrough, inline code, code blocks, links and rules. Anything else shows as its plain text.

use egui::{RichText, Ui};
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::theme;

const INDENT: f32 = 18.0;

#[derive(Debug, Default, Clone, Copy)]
struct Style {
    strong: bool,
    emphasis: bool,
    strike: bool,
}

struct Span {
    text: String,
    style: Style,
    code: bool,
    link: Option<String>,
}

/// One line of output being collected (a paragraph, heading or list item's text).
#[derive(Default)]
struct Line {
    spans: Vec<Span>,
    heading: Option<HeadingLevel>,
    /// Bullet / number of the list item this line starts.
    marker: Option<String>,
}

pub fn show(ui: &mut Ui, markdown: &str) {
    let mut line = Line::default();
    let mut style = Style::default();
    let mut link: Option<String> = None;
    // One entry per open list: the next number of an ordered list, `None` for bullets.
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut code_block: Option<String> = None;

    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(markdown, options) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    flush(ui, &mut line, lists.len());
                    line.heading = Some(level);
                }
                Tag::List(start) => {
                    flush(ui, &mut line, lists.len());
                    lists.push(start);
                }
                Tag::Item => {
                    flush(ui, &mut line, lists.len());
                    line.marker = Some(match lists.last_mut() {
                        Some(Some(n)) => {
                            *n += 1;
                            format!("{}.", *n - 1)
                        }
                        _ => "•".into(),
                    });
                }
                Tag::CodeBlock(_) => {
                    flush(ui, &mut line, lists.len());
                    code_block = Some(String::new());
                }
                Tag::Strong => style.strong = true,
                Tag::Emphasis => style.emphasis = true,
                Tag::Strikethrough => style.strike = true,
                Tag::Link { dest_url, .. } => link = Some(dest_url.to_string()),
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(level) => {
                    flush(ui, &mut line, lists.len());
                    ui.add_space(if level <= HeadingLevel::H2 { 6.0 } else { 2.0 });
                }
                TagEnd::Paragraph => {
                    flush(ui, &mut line, lists.len());
                    if lists.is_empty() {
                        ui.add_space(6.0);
                    }
                }
                TagEnd::Item => flush(ui, &mut line, lists.len()),
                TagEnd::List(_) => {
                    flush(ui, &mut line, lists.len());
                    lists.pop();
                    if lists.is_empty() {
                        ui.add_space(6.0);
                    }
                }
                TagEnd::CodeBlock => {
                    if let Some(code) = code_block.take() {
                        code_frame(ui, code.trim_end());
                    }
                }
                TagEnd::Strong => style.strong = false,
                TagEnd::Emphasis => style.emphasis = false,
                TagEnd::Strikethrough => style.strike = false,
                TagEnd::Link => link = None,
                _ => {}
            },
            Event::Text(text) => match &mut code_block {
                Some(code) => code.push_str(&text),
                None => push(&mut line, &text, style, false, &link),
            },
            Event::Code(text) => push(&mut line, &text, style, true, &link),
            Event::SoftBreak => push(&mut line, " ", style, false, &link),
            Event::HardBreak => push(&mut line, "\n", style, false, &link),
            Event::TaskListMarker(done) => push(&mut line, if done { "☑ " } else { "☐ " }, style, false, &None),
            Event::Rule => {
                flush(ui, &mut line, lists.len());
                ui.separator();
            }
            Event::Html(text) | Event::InlineHtml(text) => push(&mut line, &text, style, false, &link),
            _ => {}
        }
    }
    flush(ui, &mut line, lists.len());
}

fn push(line: &mut Line, text: &str, style: Style, code: bool, link: &Option<String>) {
    line.spans.push(Span {
        text: text.to_owned(),
        style,
        code,
        link: link.clone(),
    });
}

/// Draws the collected line (if any) at the indentation of `depth` open lists.
fn flush(ui: &mut Ui, line: &mut Line, depth: usize) {
    let spans = std::mem::take(&mut line.spans);
    let marker = line.marker.take();
    if spans.is_empty() && marker.is_none() {
        return;
    }
    let p = theme::pal(ui);
    let size = match line.heading {
        Some(HeadingLevel::H1) => Some(20.0),
        Some(HeadingLevel::H2) => Some(17.0),
        Some(HeadingLevel::H3) => Some(15.0),
        Some(_) => Some(14.0),
        None => None,
    };
    ui.horizontal_top(|ui| {
        ui.add_space(depth.saturating_sub(1) as f32 * INDENT);
        if let Some(marker) = marker {
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(INDENT, ui.text_style_height(&egui::TextStyle::Body)),
                egui::Sense::hover(),
            );
            ui.painter().text(
                rect.left_center(),
                egui::Align2::LEFT_CENTER,
                marker,
                egui::FontId::proportional(13.0),
                p.text_muted,
            );
        }
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            for span in spans {
                let mut text = RichText::new(span.text);
                if let Some(size) = size {
                    text = text.size(size);
                }
                // `strong()` brightens the text, unless an explicit colour overrides it.
                text = match (span.link.is_some(), size.is_some() || span.style.strong) {
                    (true, _) => text.color(p.accent),
                    (false, true) => text.strong(),
                    (false, false) => text.color(p.text),
                };
                if span.style.emphasis {
                    text = text.italics();
                }
                if span.style.strike {
                    text = text.strikethrough();
                }
                if span.code {
                    text = text.code();
                }
                match span.link {
                    Some(url) => {
                        ui.hyperlink_to(text, url);
                    }
                    None => {
                        ui.label(text);
                    }
                }
            }
        });
    });
}

fn code_frame(ui: &mut Ui, code: &str) {
    let p = theme::pal(ui);
    egui::Frame::new().fill(p.bg_input).corner_radius(4).inner_margin(8).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(code).monospace().color(p.text));
    });
    ui.add_space(6.0);
}
