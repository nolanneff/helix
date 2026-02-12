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
/// The number of lines above the selection is dynamic:
/// - 1 line (spinner only) when thinking_lines == 0
/// - 2 lines (spinner + 1 thinking) when thinking_lines == 1
/// - 3 lines (spinner + 2 thinking) when thinking_lines == 2
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
}

impl AiProgressAnnotation {
    pub fn new(
        above_line: usize,
        below_line: usize,
        above_char: usize,
        below_char: usize,
        thinking_lines: u8,
        has_tool_line: bool,
    ) -> Box<dyn LineAnnotation> {
        Box::new(Self {
            above_line,
            below_line,
            above_char,
            below_char,
            next_anchor: Cell::new(above_char),
            thinking_lines,
            has_tool_line,
        })
    }

    fn compute_next_anchor(&self, after: usize) -> usize {
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
        if doc_line == self.above_line {
            // 1 (spinner) + thinking_lines (0..2) + tool_line (0 or 1)
            let tool_extra = if self.has_tool_line { 1 } else { 0 };
            Position::new(1 + self.thinking_lines as usize + tool_extra, 0)
        } else if doc_line == self.below_line && self.below_line != self.above_line {
            Position::new(1, 0)
        } else {
            Position::new(0, 0)
        }
    }
}
