use crate::compositor::{self, Component, Context, EventResult};
use helix_core::Position;
use helix_view::graphics::{CursorKind, Rect};
use helix_view::input::{Event, KeyEvent};
use helix_view::keyboard::{KeyCode, KeyModifiers};
use tui::buffer::Buffer as Surface;
use tui::text::Span;
use tui::widgets::{Block, Borders, Widget};

/// Multi-line text input popup component for AI-assisted code replacement.
///
/// Enter submits, Shift-Enter inserts a newline, Esc/Ctrl-c cancels.
pub struct AiPrompt {
    lines: Vec<String>,
    cursor_row: usize,
    cursor_col: usize,
    scroll_offset: usize, // in visual rows (after wrapping)
    on_submit: Option<Box<dyn FnOnce(&mut compositor::Context, String) + Send>>,
    title: String,
}

impl AiPrompt {
    pub fn new(
        title: String,
        on_submit: impl FnOnce(&mut compositor::Context, String) + Send + 'static,
    ) -> Self {
        Self {
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
            scroll_offset: 0,
            on_submit: Some(Box::new(on_submit)),
            title,
        }
    }

    fn insert_char(&mut self, c: char) {
        if self.cursor_row < self.lines.len() {
            self.lines[self.cursor_row].insert(self.cursor_col, c);
            self.cursor_col += 1;
        }
    }

    fn insert_newline(&mut self) {
        if self.cursor_row < self.lines.len() {
            let remaining = self.lines[self.cursor_row][self.cursor_col..].to_string();
            self.lines[self.cursor_row].truncate(self.cursor_col);
            self.lines.insert(self.cursor_row + 1, remaining);
            self.cursor_row += 1;
            self.cursor_col = 0;
        }
    }

    fn delete_char_backward(&mut self) {
        if self.cursor_col > 0 {
            self.lines[self.cursor_row].remove(self.cursor_col - 1);
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            let current_line = self.lines.remove(self.cursor_row);
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].len();
            self.lines[self.cursor_row].push_str(&current_line);
        }
    }

    fn delete_char_forward(&mut self) {
        if self.cursor_row < self.lines.len() {
            let line_len = self.lines[self.cursor_row].len();
            if self.cursor_col < line_len {
                self.lines[self.cursor_row].remove(self.cursor_col);
            } else if self.cursor_row + 1 < self.lines.len() {
                let next_line = self.lines.remove(self.cursor_row + 1);
                self.lines[self.cursor_row].push_str(&next_line);
            }
        }
    }

    fn delete_word_backward(&mut self) {
        if self.cursor_col > 0 {
            let line = &self.lines[self.cursor_row];
            let before_cursor = &line[..self.cursor_col];
            let mut new_col = self.cursor_col;
            let chars: Vec<char> = before_cursor.chars().collect();

            while new_col > 0 && chars[new_col - 1].is_whitespace() {
                new_col -= 1;
            }
            while new_col > 0 && !chars[new_col - 1].is_whitespace() {
                new_col -= 1;
            }

            self.lines[self.cursor_row].replace_range(new_col..self.cursor_col, "");
            self.cursor_col = new_col;
        }
    }

    fn move_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].len();
        }
    }

    fn move_right(&mut self) {
        if self.cursor_row < self.lines.len() {
            let line_len = self.lines[self.cursor_row].len();
            if self.cursor_col < line_len {
                self.cursor_col += 1;
            } else if self.cursor_row + 1 < self.lines.len() {
                self.cursor_row += 1;
                self.cursor_col = 0;
            }
        }
    }

    fn move_up(&mut self) {
        if self.cursor_row > 0 {
            self.cursor_row -= 1;
            let line_len = self.lines[self.cursor_row].len();
            self.cursor_col = self.cursor_col.min(line_len);
        }
    }

    fn move_down(&mut self) {
        if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            let line_len = self.lines[self.cursor_row].len();
            self.cursor_col = self.cursor_col.min(line_len);
        }
    }

    fn move_home(&mut self) {
        self.cursor_col = 0;
    }

    fn move_end(&mut self) {
        if self.cursor_row < self.lines.len() {
            self.cursor_col = self.lines[self.cursor_row].len();
        }
    }

    /// How many visual rows a logical line occupies when wrapped to `width` columns.
    fn wrapped_line_height(line: &str, width: usize) -> usize {
        if width == 0 {
            return 1;
        }
        let len = line.len().max(1); // empty line still takes 1 row
        (len + width - 1) / width
    }

    /// Compute the visual row of the cursor within the entire wrapped document.
    fn cursor_visual_row(&self, width: usize) -> usize {
        let mut vrow = 0;
        for (i, line) in self.lines.iter().enumerate() {
            if i == self.cursor_row {
                // cursor is on this line — add the wrap offset within it
                if width > 0 {
                    vrow += self.cursor_col / width;
                }
                return vrow;
            }
            vrow += Self::wrapped_line_height(line, width);
        }
        vrow
    }

    /// Total number of visual rows across all lines.
    fn total_visual_rows(&self, width: usize) -> usize {
        self.lines
            .iter()
            .map(|l| Self::wrapped_line_height(l, width))
            .sum()
    }

    /// Update scroll_offset (in visual rows) to keep cursor visible.
    fn update_scroll(&mut self, visible_height: usize, width: usize) {
        if visible_height == 0 {
            return;
        }
        let cursor_vrow = self.cursor_visual_row(width);
        if cursor_vrow >= self.scroll_offset + visible_height {
            self.scroll_offset = cursor_vrow - visible_height + 1;
        }
        if cursor_vrow < self.scroll_offset {
            self.scroll_offset = cursor_vrow;
        }
    }
}

impl Component for AiPrompt {
    fn handle_event(&mut self, event: &Event, _ctx: &mut Context) -> EventResult {
        match event {
            // Submit on Enter
            Event::Key(KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            }) => {
                let text = self.lines.join("\n");
                let on_submit = self.on_submit.take();
                EventResult::Consumed(Some(Box::new(move |compositor, ctx| {
                    if let Some(cb) = on_submit {
                        cb(ctx, text);
                    }
                    compositor.pop();
                })))
            }

            // Insert newline on Shift-Enter
            Event::Key(KeyEvent {
                code: KeyCode::Enter,
                modifiers,
                ..
            }) if modifiers.contains(KeyModifiers::SHIFT) => {
                self.insert_newline();
                EventResult::Consumed(None)
            }

            // Cancel on Esc or Ctrl-c
            Event::Key(KeyEvent {
                code: KeyCode::Esc, ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => EventResult::Consumed(Some(Box::new(|compositor, _ctx| {
                compositor.pop();
            }))),

            // Backspace
            Event::Key(KeyEvent {
                code: KeyCode::Backspace,
                ..
            }) => {
                self.delete_char_backward();
                EventResult::Consumed(None)
            }

            // Delete
            Event::Key(KeyEvent {
                code: KeyCode::Delete,
                ..
            }) => {
                self.delete_char_forward();
                EventResult::Consumed(None)
            }

            // Ctrl-w: delete word backward
            Event::Key(KeyEvent {
                code: KeyCode::Char('w'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                self.delete_word_backward();
                EventResult::Consumed(None)
            }

            // Arrow keys
            Event::Key(KeyEvent {
                code: KeyCode::Left,
                ..
            }) => {
                self.move_left();
                EventResult::Consumed(None)
            }
            Event::Key(KeyEvent {
                code: KeyCode::Right,
                ..
            }) => {
                self.move_right();
                EventResult::Consumed(None)
            }
            Event::Key(KeyEvent {
                code: KeyCode::Up, ..
            }) => {
                self.move_up();
                EventResult::Consumed(None)
            }
            Event::Key(KeyEvent {
                code: KeyCode::Down,
                ..
            }) => {
                self.move_down();
                EventResult::Consumed(None)
            }

            // Home/End
            Event::Key(KeyEvent {
                code: KeyCode::Home,
                ..
            }) => {
                self.move_home();
                EventResult::Consumed(None)
            }
            Event::Key(KeyEvent {
                code: KeyCode::End, ..
            }) => {
                self.move_end();
                EventResult::Consumed(None)
            }

            // Tab inserts spaces
            Event::Key(KeyEvent {
                code: KeyCode::Tab, ..
            }) => {
                for _ in 0..4 {
                    self.insert_char(' ');
                }
                EventResult::Consumed(None)
            }

            // Regular character input
            Event::Key(KeyEvent {
                code: KeyCode::Char(c),
                modifiers,
                ..
            }) if modifiers.is_empty() || *modifiers == KeyModifiers::SHIFT => {
                self.insert_char(*c);
                EventResult::Consumed(None)
            }

            _ => EventResult::Ignored(None),
        }
    }

    fn render(&mut self, area: Rect, surface: &mut Surface, ctx: &mut Context) {
        use helix_view::graphics::{Color, Modifier, Style};

        // Muted white border, dark background
        let border_style = Style::default().fg(Color::Rgb(140, 140, 150));
        let bg_style = ctx.editor.theme.get("ui.popup");

        // Clear the area so editor text doesn't bleed through
        surface.clear_with(area, bg_style);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border_style);

        let inner = block.inner(area);
        block.render(area, surface);

        // Draw centered title on the top border in light blue
        let title = format!(" {} ", self.title);
        let title_len = title.len() as u16;
        if title_len < area.width.saturating_sub(2) {
            let title_x = area.x + (area.width.saturating_sub(title_len)) / 2;
            let title_style = Style::default()
                .fg(Color::Rgb(130, 200, 210))
                .add_modifier(Modifier::BOLD);
            surface.set_string(title_x, area.y, &title, title_style);
        }

        let width = inner.width as usize;
        let visible_height = inner.height as usize;
        self.update_scroll(visible_height, width);

        // Render lines with wrapping — white input text
        let text_style = Style::default().fg(Color::White);
        let mut visual_row: usize = 0; // global visual row counter
        let mut screen_row: usize = 0; // row on screen (relative to inner.y)

        for line in &self.lines {
            let line_vrows = Self::wrapped_line_height(line, width);

            // Check if any part of this line is visible
            if visual_row + line_vrows > self.scroll_offset
                && visual_row < self.scroll_offset + visible_height
            {
                let chars: Vec<char> = line.chars().collect();
                let chunk_count = if width > 0 { (chars.len() + width - 1) / width } else { 1 };
                let chunk_count = chunk_count.max(1); // at least 1 row for empty lines

                for chunk_idx in 0..chunk_count {
                    if visual_row + chunk_idx < self.scroll_offset {
                        continue;
                    }
                    if screen_row >= visible_height {
                        break;
                    }

                    let y = inner.y + screen_row as u16;
                    let start = chunk_idx * width;
                    let end = (start + width).min(chars.len());

                    for (col, &ch) in chars[start..end].iter().enumerate() {
                        let x = inner.x + col as u16;
                        if let Some(cell) = surface.get_mut(x, y) {
                            cell.set_char(ch);
                            cell.set_style(text_style);
                        }
                    }

                    screen_row += 1;
                }
            }

            visual_row += line_vrows;
            if screen_row >= visible_height {
                break;
            }
        }
    }

    fn cursor(&self, area: Rect, _editor: &helix_view::Editor) -> (Option<Position>, CursorKind) {
        let block = Block::default().borders(Borders::ALL);
        let inner = block.inner(area);
        let width = inner.width as usize;

        if width == 0 {
            return (None, CursorKind::Hidden);
        }

        let cursor_vrow = self.cursor_visual_row(width);
        let visible_row = cursor_vrow.saturating_sub(self.scroll_offset);
        let col_in_wrap = self.cursor_col % width;

        if visible_row < inner.height as usize {
            let cursor_x = inner.x + col_in_wrap as u16;
            let cursor_y = inner.y + visible_row as u16;

            if cursor_x < inner.x + inner.width && cursor_y < inner.y + inner.height {
                return (
                    Some(Position::new(cursor_y as usize, cursor_x as usize)),
                    CursorKind::Block,
                );
            }
        }

        (None, CursorKind::Hidden)
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        Some((viewport.0, viewport.1))
    }

    fn id(&self) -> Option<&'static str> {
        Some("ai-prompt")
    }
}
