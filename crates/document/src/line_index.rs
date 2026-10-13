//! Conversion between byte offsets and editor line/character positions.
//!
//! LSP counts characters in UTF-16 code units while Rust strings are UTF-8, so
//! every position crossing the protocol boundary needs translating. Getting this
//! wrong is invisible in ASCII test files and produces off-by-N cursors the
//! moment a user types an emoji or a non-Latin identifier, so it is handled once,
//! here, instead of at each call site.

/// A zero-based editor position. `character` is a UTF-16 code-unit offset within
/// the line, matching the LSP default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

impl Position {
    pub fn new(line: u32, character: u32) -> Self {
        Self { line, character }
    }
}

/// A zero-based position whose `column` is a UTF-8 **byte** offset within the
/// line, not a UTF-16 code-unit offset.
///
/// It exists so the unit is impossible to mix up with [`Position`]. Incremental
/// parsers count columns in bytes, and on a line containing an emoji the two
/// units disagree — silently, and only for the users who have such lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct BytePosition {
    pub line: u32,
    pub column: u32,
}

impl BytePosition {
    pub fn new(line: u32, column: u32) -> Self {
        Self { line, column }
    }
}

/// Byte offsets of the first character of each line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineIndex {
    line_starts: Vec<u32>,
    len: u32,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let mut line_starts = vec![0u32];
        for (offset, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                line_starts.push(offset as u32 + 1);
            }
        }
        Self {
            line_starts,
            len: text.len() as u32,
        }
    }

    pub fn line_count(&self) -> u32 {
        self.line_starts.len() as u32
    }

    /// Byte offset of a position, clamped into the document.
    ///
    /// Clamping rather than failing is deliberate: an editor can legitimately
    /// send a position from a revision the server has already replaced, and
    /// answering about the nearest valid spot beats erroring out mid-keystroke.
    pub fn offset(&self, text: &str, position: Position) -> u32 {
        let line = (position.line as usize).min(self.line_starts.len() - 1);
        let line_start = self.line_starts[line] as usize;
        let line_end = self
            .line_starts
            .get(line + 1)
            .map_or(self.len as usize, |next| *next as usize);
        let line_text = &text[line_start..line_end];

        let mut utf16_seen = 0u32;
        for (byte_offset, ch) in line_text.char_indices() {
            if utf16_seen >= position.character {
                return (line_start + byte_offset) as u32;
            }
            // A trailing newline is not addressable; stop at the line's content end.
            if ch == '\n' {
                return (line_start + byte_offset) as u32;
            }
            utf16_seen += ch.len_utf16() as u32;
        }
        (line_start + line_text.trim_end_matches('\n').len()) as u32
    }

    /// Line and byte column of a byte offset, clamped into the document.
    ///
    /// No text is needed: both halves are byte counts, so the line table alone
    /// answers it.
    pub fn byte_position(&self, offset: u32) -> BytePosition {
        let offset = offset.min(self.len);
        let line = self.line_starts.partition_point(|start| *start <= offset) - 1;
        BytePosition {
            line: line as u32,
            column: offset - self.line_starts[line],
        }
    }

    /// Position of a byte offset, clamped into the document.
    pub fn position(&self, text: &str, offset: u32) -> Position {
        let offset = offset.min(self.len) as usize;
        let line = self
            .line_starts
            .partition_point(|start| *start as usize <= offset)
            - 1;
        let line_start = self.line_starts[line] as usize;
        // Snap into the middle of a multi-byte character rather than panicking.
        let mut boundary = offset;
        while boundary > line_start && !text.is_char_boundary(boundary) {
            boundary -= 1;
        }
        let character = text[line_start..boundary]
            .chars()
            .map(|c| c.len_utf16() as u32)
            .sum();
        Position {
            line: line as u32,
            character,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_positions_in_plain_ascii() {
        let text = "class A {\n  int x;\n}\n";
        let index = LineIndex::new(text);
        assert_eq!(index.line_count(), 4);
        assert_eq!(index.offset(text, Position::new(1, 2)), 12);
        assert_eq!(index.position(text, 12), Position::new(1, 2));
    }

    #[test]
    fn counts_characters_in_utf16_units_like_the_protocol_does() {
        // The emoji is 4 UTF-8 bytes but 2 UTF-16 code units.
        let text = "// 🚀 go\nx";
        let index = LineIndex::new(text);
        let after_emoji = Position::new(0, 5); // "// " = 3, emoji = 2
        assert_eq!(index.offset(text, after_emoji), 7);
        assert_eq!(index.position(text, 7), after_emoji);
    }

    #[test]
    fn round_trips_every_char_boundary() {
        let text = "ä\nbü\nc\n";
        let index = LineIndex::new(text);
        for (offset, _) in text.char_indices() {
            let position = index.position(text, offset as u32);
            assert_eq!(
                index.offset(text, position),
                offset as u32,
                "at byte {offset}"
            );
        }
    }

    #[test]
    fn byte_columns_count_bytes_where_editor_columns_count_utf16_units() {
        let text = "// 🚀 go\nx";
        let index = LineIndex::new(text);
        // Byte 7 is just past the emoji: 3 ASCII bytes + 4 emoji bytes, but only
        // 3 + 2 UTF-16 code units.
        assert_eq!(index.byte_position(7), BytePosition::new(0, 7));
        assert_eq!(index.position(text, 7), Position::new(0, 5));
    }

    #[test]
    fn byte_positions_past_the_end_clamp_to_the_last_line() {
        let text = "a\nbc";
        let index = LineIndex::new(text);
        assert_eq!(index.byte_position(999), BytePosition::new(1, 2));
        assert_eq!(index.byte_position(2), BytePosition::new(1, 0));
    }

    #[test]
    fn positions_past_the_end_clamp_instead_of_panicking() {
        let text = "a\nb";
        let index = LineIndex::new(text);
        assert_eq!(index.offset(text, Position::new(99, 99)), 3);
        assert_eq!(index.position(text, 999), Position::new(1, 1));
    }

    #[test]
    fn a_character_past_the_line_end_clamps_to_the_line_end() {
        let text = "ab\ncd\n";
        let index = LineIndex::new(text);
        assert_eq!(index.offset(text, Position::new(0, 40)), 2);
    }
}
