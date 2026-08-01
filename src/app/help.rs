//! The Help window: renders an **external** `help.md` so the documentation can
//! be edited (or replaced per deployment) without rebuilding cim.
//!
//! The file is looked for next to the executable first, then in the working
//! directory. It is read on the first open and re-read by the window's Reload
//! button, so editing it while cim runs needs no restart.
//!
//! Only a small, deliberate subset of Markdown is rendered (see
//! [`draw_markdown`]): `#`/`##`/`###` headings, `-`/`*` bullets (one nesting
//! level), fenced code blocks, `---` rules, and the inline `**bold**`,
//! `*italic*` and `` `code` `` spans. Anything else is shown as plain text
//! rather than silently dropped.

use super::*;

use eframe::egui::text::{LayoutJob, TextFormat};

/// Name of the document, resolved relative to the executable / working dir.
const HELP_FILE: &str = "help.md";

/// Where to look for `help.md`, in order: beside the cim executable (how a
/// release is laid out, mirroring the `LIBS` folder), then the working
/// directory (how a `cargo run` from the repo finds the checked-in one).
fn candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| {
        let p = e.parent()?;
        Some(p.to_path_buf())
    }) {
        v.push(dir.join(HELP_FILE));
    }
    v.push(PathBuf::from(HELP_FILE));
    v
}

/// Read the help document, or an error naming every path that was tried (so a
/// missing file tells the user exactly where to put it).
pub(super) fn load() -> Result<String, String> {
    let paths = candidates();
    for p in &paths {
        if let Ok(s) = std::fs::read_to_string(p) {
            return Ok(s);
        }
    }
    let tried = paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    Err(format!(
        "Could not read {HELP_FILE}. Looked for it here:\n{tried}"
    ))
}

impl CimApp {
    pub(super) fn draw_help(&mut self, ctx: &egui::Context) {
        let mut open = self.show_help;
        egui::Window::new("❓ Help")
            .open(&mut open)
            .default_pos(ctx.screen_rect().center())
            .pivot(egui::Align2::CENTER_CENTER)
            .resizable(true)
            .default_width(620.0)
            .default_height(560.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .button("Reload")
                        .on_hover_text("Re-read help.md from disk")
                        .clicked()
                    {
                        self.help_doc = Some(load());
                    }
                    ui.label(
                        egui::RichText::new(format!("from {HELP_FILE}"))
                            .weak()
                            .small(),
                    );
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| match self.help_doc.as_ref() {
                        Some(Ok(md)) => draw_markdown(ui, md),
                        Some(Err(e)) => {
                            ui.colored_label(Color32::from_rgb(230, 120, 120), e);
                        }
                        None => {
                            ui.weak("Not loaded.");
                        }
                    });
            });
        self.show_help = open;
    }
}

/// Render the supported Markdown subset (see the module docs) into `ui`.
fn draw_markdown(ui: &mut egui::Ui, md: &str) {
    let body = ui.text_style_height(&egui::TextStyle::Body);
    let mut in_code = false;
    let mut code = String::new();
    for line in md.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim_start().starts_with("```") {
            if in_code {
                code_block(ui, &code);
                code.clear();
            }
            in_code = !in_code;
            continue;
        }
        if in_code {
            code.push_str(trimmed);
            code.push('\n');
            continue;
        }
        let t = trimmed.trim_start();
        // Indented bullets keep one nesting level; deeper ones flatten to it.
        let indent = (trimmed.len() - t.len()) as f32;
        if t.is_empty() {
            ui.add_space(body * 0.4);
        } else if let Some(rest) = t.strip_prefix("### ") {
            heading(ui, rest, body * 1.05, body * 0.6);
        } else if let Some(rest) = t.strip_prefix("## ") {
            heading(ui, rest, body * 1.2, body * 0.8);
        } else if let Some(rest) = t.strip_prefix("# ") {
            heading(ui, rest, body * 1.45, body * 0.4);
        } else if t == "---" || t == "***" {
            ui.separator();
        } else if let Some(rest) = t
            .strip_prefix("- ")
            .or_else(|| t.strip_prefix("* "))
            .or_else(|| t.strip_prefix("+ "))
        {
            ui.horizontal_top(|ui| {
                ui.add_space(8.0 + if indent >= 2.0 { 16.0 } else { 0.0 });
                ui.label("•");
                ui.label(inline(ui, rest, body));
            });
        } else {
            ui.label(inline(ui, t, body));
        }
    }
    if in_code && !code.is_empty() {
        code_block(ui, &code);
    }
}

fn heading(ui: &mut egui::Ui, text: &str, size: f32, space_above: f32) {
    ui.add_space(space_above);
    ui.label(
        egui::RichText::new(text)
            .size(size)
            .strong()
            .color(TEXT_BUTTON_HOVER),
    );
    ui.add_space(2.0);
}

fn code_block(ui: &mut egui::Ui, code: &str) {
    egui::Frame::none()
        .fill(BAR_FILL)
        .inner_margin(6.0)
        .show(ui, |ui| {
            ui.label(egui::RichText::new(code.trim_end()).monospace());
        });
}

/// Lay out one line's inline spans — `**bold**`, `*italic*`, `` `code` `` — as a
/// single `LayoutJob`, so the mixed styles wrap as one paragraph rather than as
/// separate widgets.
fn inline(ui: &egui::Ui, text: &str, size: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = ui.available_width();
    let mut rest = text;
    // Plain run up to the next marker, then the marked span it introduces.
    while !rest.is_empty() {
        let next = rest
            .char_indices()
            .find(|&(_, c)| c == '*' || c == '`')
            .map(|(i, _)| i);
        let Some(start) = next else {
            push(&mut job, rest, size, false, false, false);
            break;
        };
        if start > 0 {
            push(&mut job, &rest[..start], size, false, false, false);
            rest = &rest[start..];
        }
        let (marker, bold, italic, code) = if rest.starts_with("**") {
            ("**", true, false, false)
        } else if rest.starts_with('`') {
            ("`", false, false, true)
        } else {
            ("*", false, true, false)
        };
        let after = &rest[marker.len()..];
        match after.find(marker) {
            Some(end) => {
                push(&mut job, &after[..end], size, bold, italic, code);
                rest = &after[end + marker.len()..];
            }
            // Unterminated marker: show it literally rather than swallowing the
            // rest of the line.
            None => {
                push(&mut job, rest, size, false, false, false);
                break;
            }
        }
    }
    job
}

fn push(job: &mut LayoutJob, text: &str, size: f32, bold: bool, italic: bool, code: bool) {
    if text.is_empty() {
        return;
    }
    let font = if code {
        FontId::monospace(size * 0.95)
    } else {
        FontId::proportional(size)
    };
    job.append(
        text,
        0.0,
        TextFormat {
            font_id: font,
            color: if bold {
                TEXT_BUTTON_HOVER
            } else if code {
                Color32::from_rgb(200, 200, 150)
            } else {
                TEXT_DEFAULT
            },
            italics: italic,
            ..Default::default()
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The inline parser splits on markers and leaves unterminated ones literal
    /// (rather than swallowing the rest of the line).
    #[test]
    fn inline_spans_cover_the_whole_line() {
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |ctx| {
            egui::Area::new(egui::Id::new("t")).show(ctx, |ui| {
                let job = inline(ui, "a **b** c `d` *e*", 14.0);
                assert_eq!(job.text, "a b c d e");
                let job = inline(ui, "unterminated **bold", 14.0);
                assert_eq!(job.text, "unterminated **bold");
            });
        });
    }
}
