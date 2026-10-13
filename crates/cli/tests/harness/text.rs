//! UTF-16 position mapping, written here on purpose.
//!
//! `javelin-document` computes the same thing and is reachable from these
//! tests, because the `javelin` binary depends on it. Using it would mean a
//! position bug agreed with itself: the harness would ask about exactly the
//! offset the server derived, and both would be wrong together.

/// The byte offset of an LSP position, clamping rather than panicking.
///
/// A client that asks past the end of a line or a file is not misbehaving; an
/// editor does it whenever a buffer shrinks under a cursor.
pub fn offset_of(text: &str, line: u32, character: u32) -> usize {
    let mut offset = 0usize;
    for _ in 0..line {
        match text[offset..].find('\n') {
            Some(newline) => offset += newline + 1,
            None => return text.len(),
        }
    }
    let rest = &text[offset..];
    let line_end = rest.find('\n').unwrap_or(rest.len());
    let mut units = 0u32;
    for (byte, ch) in rest[..line_end].char_indices() {
        if units >= character {
            return offset + byte;
        }
        units += ch.len_utf16() as u32;
    }
    offset + line_end
}

/// The LSP position of a byte offset. The inverse of [`offset_of`] for any
/// offset that sits on a character boundary.
pub fn position_of(text: &str, offset: usize) -> (u32, u32) {
    let offset = offset.min(text.len());
    let line = text[..offset].matches('\n').count() as u32;
    let start = text[..offset].rfind('\n').map_or(0, |newline| newline + 1);
    let character = text[start..offset]
        .chars()
        .map(char::len_utf16)
        .sum::<usize>() as u32;
    (line, character)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GREETER: &str =
        include_str!("../../../../tests/projects/zed-demo/src/main/java/com/example/Greeter.java");

    #[test]
    fn a_character_count_past_a_non_ascii_letter_lands_after_its_bytes() {
        // Line 26 (0-based 25): `        String greeting = "Grüße, " + name + " 🚀";`
        // Character 30 is just past the `ü`, landing on `ß` (two UTF-8
        // bytes) followed by `e` (one byte) — three bytes for "ße".
        let offset = offset_of(GREETER, 25, 30);
        assert_eq!(&GREETER[offset..offset + 3], "ße");
    }

    #[test]
    fn an_astral_character_counts_as_two_units_going_out_and_coming_back() {
        let line = 25;
        let text_of_line = GREETER.lines().nth(line as usize).expect("line 26 exists");
        let units = text_of_line.chars().map(char::len_utf16).sum::<usize>() as u32;
        let end = offset_of(GREETER, line, units);
        assert_eq!(position_of(GREETER, end), (line, units));
        assert!(
            text_of_line.contains('\u{1F680}'),
            "the rocket is what makes this a test"
        );
    }

    #[test]
    fn positions_past_the_end_clamp_instead_of_panicking() {
        let end = offset_of(GREETER, 9_999, 9_999);
        assert_eq!(end, GREETER.len());
        let past = offset_of(GREETER, 25, 9_999);
        assert_eq!(&GREETER[past..past + 1], "\n");
    }

    #[test]
    fn an_offset_at_the_start_of_a_line_reports_character_zero() {
        let offset = offset_of(GREETER, 12, 0);
        assert_eq!(position_of(GREETER, offset), (12, 0));
        assert!(GREETER[offset..].starts_with("public class Greeter"));
    }
}
