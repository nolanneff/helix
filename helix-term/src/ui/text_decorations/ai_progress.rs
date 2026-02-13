use std::sync::{Arc, Mutex};

use helix_core::Position;
use helix_view::graphics::Style;
use tokio::time::Instant;

use crate::ui::document::{LinePos, TextRenderer};
use crate::ui::text_decorations::Decoration;

const SPINNER_FRAMES: &[&str] = &["◜", "◠", "◝", "◞", "◡", "◟"];
const SPINNER_INTERVAL_MS: u128 = 80;

pub struct AiProgressDecoration {
    pub streaming_text: Arc<Mutex<String>>,
    pub tool_display: Arc<Mutex<String>>,
    pub started_at: Instant,
    /// Line before selection start (virtual lines after this = above selection)
    pub above_line: usize,
    /// Last line of selection (virtual lines after this = below selection)
    pub below_line: usize,
    /// Number of thinking text lines to display (0, 1, or 2) — must match annotation
    pub thinking_lines: u8,
    /// Whether a tool call line is active (1 extra virtual line)
    pub has_tool_line: bool,
    /// If true, render progress below the selection instead of above
    pub show_below: bool,
    /// Label shown next to spinner (e.g. "Implementing", "Analyzing", "Searching")
    pub label: String,
    pub style: Style,
    pub spinner_style: Style,
    pub tool_style: Style,
}

impl AiProgressDecoration {
    fn spinner_frame(&self) -> &'static str {
        let elapsed = self.started_at.elapsed().as_millis();
        let idx = (elapsed / SPINNER_INTERVAL_MS) as usize % SPINNER_FRAMES.len();
        SPINNER_FRAMES[idx]
    }

    fn render_spinner_line(&self, renderer: &mut TextRenderer, row: u16) {
        let frame = self.spinner_frame();
        let text = format!("  {} {}", frame, self.label);
        let style = self.spinner_style;
        renderer.set_string_truncated(
            renderer.viewport.x,
            row,
            &text,
            renderer.viewport.width as usize,
            |_| style,
            false,
            false,
        );
    }

    fn render_tool_line(&self, renderer: &mut TextRenderer, row: u16) {
        let text = match self.tool_display.lock() {
            Ok(guard) => guard.clone(),
            Err(_) => return,
        };
        if text.is_empty() {
            return;
        }
        let display = format!("    {}", text);
        let style = self.tool_style;
        renderer.set_string_truncated(
            renderer.viewport.x,
            row,
            &display,
            renderer.viewport.width as usize,
            |_| style,
            true,
            false,
        );
    }

    fn render_thinking_lines(&self, renderer: &mut TextRenderer, row_start: u16, max_lines: u8) {
        if max_lines == 0 {
            return;
        }

        let text = match self.streaming_text.lock() {
            Ok(guard) => guard.clone(),
            Err(_) => return,
        };

        if text.is_empty() {
            return;
        }

        // Word-wrap the text at viewport width (minus 4-char indent) so that
        // streaming tokens that arrive without newlines still produce multiple
        // visible lines.
        let indent = 4usize;
        let max_width = (renderer.viewport.width as usize).saturating_sub(indent).max(10);
        let wrapped = word_wrap(&text, max_width);

        let n = max_lines as usize;
        let tail: Vec<&str> = if wrapped.len() > n {
            wrapped[wrapped.len() - n..].iter().map(|s| s.as_str()).collect()
        } else {
            wrapped.iter().map(|s| s.as_str()).collect()
        };

        let style = self.style;
        for (i, line) in tail.iter().enumerate() {
            let display = format!("    {}", line);
            renderer.set_string_truncated(
                renderer.viewport.x,
                row_start + i as u16,
                &display,
                renderer.viewport.width as usize,
                |_| style,
                true,
                false,
            );
        }
    }
}

/// Simple word-wrap: breaks `text` into lines of at most `max_width` chars,
/// splitting at word boundaries when possible.
fn word_wrap(text: &str, max_width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.lines() {
        if paragraph.is_empty() {
            continue;
        }
        let mut current = String::new();
        for word in paragraph.split_whitespace() {
            if current.is_empty() {
                current.push_str(word);
            } else if current.len() + 1 + word.len() <= max_width {
                current.push(' ');
                current.push_str(word);
            } else {
                lines.push(std::mem::take(&mut current));
                current.push_str(word);
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
    }
    lines
}

impl Decoration for AiProgressDecoration {
    fn render_virt_lines(
        &mut self,
        renderer: &mut TextRenderer,
        pos: LinePos,
        virt_off: Position,
    ) -> Position {
        if pos.doc_line == self.above_line {
            let base_row = pos.visual_line + virt_off.row as u16;
            // Always render spinner above
            self.render_spinner_line(renderer, base_row);
            let mut rows = 1usize;
            // Thinking + tool go above when !show_below
            if !self.show_below {
                if self.thinking_lines > 0 {
                    self.render_thinking_lines(renderer, base_row + rows as u16, self.thinking_lines);
                    rows += self.thinking_lines as usize;
                }
                if self.has_tool_line {
                    self.render_tool_line(renderer, base_row + rows as u16);
                    rows += 1;
                }
            }
            Position::new(rows, 0)
        } else if pos.doc_line == self.below_line && self.below_line != self.above_line {
            let base_row = pos.visual_line + virt_off.row as u16;
            // Always render spinner below
            self.render_spinner_line(renderer, base_row);
            let mut rows = 1usize;
            // Thinking + tool go below when show_below
            if self.show_below {
                if self.thinking_lines > 0 {
                    self.render_thinking_lines(renderer, base_row + rows as u16, self.thinking_lines);
                    rows += self.thinking_lines as usize;
                }
                if self.has_tool_line {
                    self.render_tool_line(renderer, base_row + rows as u16);
                    rows += 1;
                }
            }
            Position::new(rows, 0)
        } else {
            Position::new(0, 0)
        }
    }
}
