use std::cell::Cell;

use helix_core::text_annotations::LineAnnotation;
use helix_core::Position;

/// Reserves virtual lines for AI progress display.
///
/// `above_line` is the doc line BEFORE the selection start — virtual lines inserted
/// after this line appear visually above the selection.
/// `below_line` is the last doc line of the selection — virtual lines inserted
/// after this line appear visually below the selection.
///
/// Spinners always appear on both sides (above and below the selection).
/// When `show_below` is true, thinking text + tool calls are placed below the
/// selection instead of above. This is used for large selections when the
/// selection start is near the top of the viewport.
pub struct AiProgressAnnotation {
    above_line: usize,
    below_line: usize,
    above_char: usize,
    below_char: usize,
    next_anchor: Cell<usize>,
    /// Number of thinking text lines to reserve (0, 1, or 2)
    thinking_lines: u8,
    /// Whether a tool call display line is active
    has_tool_line: bool,
    /// If true, thinking + tool lines go below the selection instead of above
    show_below: bool,
}

impl AiProgressAnnotation {
    pub fn new(
        above_line: usize,
        below_line: usize,
        above_char: usize,
        below_char: usize,
        thinking_lines: u8,
        has_tool_line: bool,
        show_below: bool,
    ) -> Box<dyn LineAnnotation> {
        Box::new(Self {
            above_line,
            below_line,
            above_char,
            below_char,
            next_anchor: Cell::new(above_char.min(below_char)),
            thinking_lines,
            has_tool_line,
            show_below,
        })
    }

    fn compute_next_anchor(&self, after: usize) -> usize {
        // Always anchor both lines — spinners appear on both sides
        if after <= self.above_char {
            self.above_char
        } else if after <= self.below_char && self.below_line != self.above_line {
            self.below_char
        } else {
            usize::MAX
        }
    }
}

impl LineAnnotation for AiProgressAnnotation {
    fn reset_pos(&mut self, char_idx: usize) -> usize {
        let next = self.compute_next_anchor(char_idx);
        self.next_anchor.set(next);
        next
    }

    fn skip_concealed_anchors(&mut self, conceal_end_char_idx: usize) -> usize {
        let next = self.compute_next_anchor(conceal_end_char_idx);
        self.next_anchor.set(next);
        next
    }

    fn process_anchor(&mut self, _grapheme: &helix_core::doc_formatter::FormattedGrapheme) -> usize {
        let current = self.next_anchor.get();
        let next = if current == self.above_char {
            if self.below_line != self.above_line {
                self.below_char
            } else {
                usize::MAX
            }
        } else {
            usize::MAX
        };
        self.next_anchor.set(next);
        next
    }

    fn insert_virtual_lines(
        &mut self,
        _line_end_char_idx: usize,
        _line_end_visual_pos: Position,
        doc_line: usize,
    ) -> Position {
        let detail_lines = self.thinking_lines as usize
            + if self.has_tool_line { 1 } else { 0 };

        if doc_line == self.above_line {
            // Spinner always above; thinking+tool here when !show_below
            let extra = if self.show_below { 0 } else { detail_lines };
            Position::new(1 + extra, 0)
        } else if doc_line == self.below_line && self.below_line != self.above_line {
            // Spinner always below; thinking+tool here when show_below
            let extra = if self.show_below { detail_lines } else { 0 };
            Position::new(1 + extra, 0)
        } else {
            Position::new(0, 0)
        }
    }
}
