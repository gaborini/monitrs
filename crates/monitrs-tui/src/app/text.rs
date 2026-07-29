//! The one-line text buffer behind `FilterEdit` and the command palette (§6.1).
//!
//! Every mutation reports whether it changed anything, because the reducer turns
//! "nothing changed" into "no [`crate::action::Effect::RequestRedraw`]": §16.1
//! forbids a redraw busy loop, and a `Left` at the start of an empty box is
//! exactly the sort of keypress that would otherwise cause one.
//!
//! The cursor is a byte offset that is always on a `char` boundary, so no
//! operation here can panic on multi-byte input. That is not a theoretical
//! concern: a filter is the one place in monitrs where the user types arbitrary
//! text, and a panic corrupts the terminal (§14.3).

use monitrs_core::units::display_width;

/// The longest input the editor accepts, in characters.
///
/// §10.3 forbids unbounded growth anywhere in the pipeline, and a held-down key
/// with terminal key repeat is an unbounded producer. A filter or palette command
/// that needs more than this is a mistake rather than a use case.
pub const MAX_INPUT_CHARS: usize = 256;

/// A single-line editable buffer with a cursor.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TextInput {
    text: String,
    /// Byte offset of the cursor, always on a `char` boundary and never past the
    /// end of `text`.
    cursor: usize,
}

impl TextInput {
    /// An empty buffer.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
        }
    }

    /// A buffer holding `text`, with the cursor at the end.
    ///
    /// Used when `/` re-opens the filter editor on an existing filter: §6.2 binds
    /// `/` to *edit* the filter, so the current value has to be there to edit.
    #[must_use]
    pub fn seeded(text: &str) -> Self {
        let mut input = Self::new();
        for character in text.chars().take(MAX_INPUT_CHARS) {
            input.text.push(character);
        }
        input.cursor = input.text.len();
        input
    }

    /// The current contents.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The cursor's byte offset.
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// The cursor's column, in terminal cells.
    ///
    /// The renderer needs cells rather than bytes to place the real terminal
    /// cursor, and a CJK character occupies two of them.
    #[must_use]
    pub fn cursor_column(&self) -> usize {
        display_width(self.text.get(..self.cursor).unwrap_or(""))
    }

    /// Whether the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// How many characters the buffer holds.
    #[must_use]
    pub fn len_chars(&self) -> usize {
        self.text.chars().count()
    }

    /// Inserts `character` at the cursor, returning whether it fitted.
    pub fn insert(&mut self, character: char) -> bool {
        if self.len_chars() >= MAX_INPUT_CHARS {
            return false;
        }
        self.text.insert(self.cursor, character);
        self.cursor = self.cursor.saturating_add(character.len_utf8());
        true
    }

    /// Replaces the whole buffer, putting the cursor at the end.
    ///
    /// This is how the palette completes a highlighted suggestion into the box,
    /// which keeps `Enter` meaning "run exactly what is displayed" (§6.3).
    pub fn set(&mut self, text: &str) -> bool {
        let replacement = Self::seeded(text);
        if replacement == *self {
            return false;
        }
        *self = replacement;
        true
    }

    /// Deletes the character before the cursor.
    pub fn delete_backward(&mut self) -> bool {
        let Some(start) = self.previous_boundary() else {
            return false;
        };
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
        true
    }

    /// Deletes the character under the cursor.
    pub fn delete_forward(&mut self) -> bool {
        let Some(end) = self.next_boundary() else {
            return false;
        };
        self.text.replace_range(self.cursor..end, "");
        true
    }

    /// Deletes the word before the cursor (`Ctrl-W`).
    ///
    /// A "word" is a run of non-space characters plus any spaces immediately
    /// before it, which is what a shell user expects and what makes deleting the
    /// last path component of `export snapshot /tmp/a.json` one keystroke.
    pub fn delete_word_backward(&mut self) -> bool {
        let head = self.text.get(..self.cursor).unwrap_or("");
        let trimmed = head.trim_end_matches(' ');
        let start = match trimmed.rfind(' ') {
            Some(index) => index.saturating_add(1),
            None => 0,
        };
        if start == self.cursor {
            return false;
        }
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
        true
    }

    /// Empties the buffer (`Ctrl-U`).
    pub fn clear(&mut self) -> bool {
        if self.text.is_empty() && self.cursor == 0 {
            return false;
        }
        self.text.clear();
        self.cursor = 0;
        true
    }

    /// Moves the cursor one character left.
    pub fn move_left(&mut self) -> bool {
        match self.previous_boundary() {
            Some(start) => {
                self.cursor = start;
                true
            }
            None => false,
        }
    }

    /// Moves the cursor one character right.
    pub fn move_right(&mut self) -> bool {
        match self.next_boundary() {
            Some(end) => {
                self.cursor = end;
                true
            }
            None => false,
        }
    }

    /// Moves the cursor to the start of the buffer.
    pub const fn move_to_start(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor = 0;
        true
    }

    /// Moves the cursor to the end of the buffer.
    pub fn move_to_end(&mut self) -> bool {
        if self.cursor == self.text.len() {
            return false;
        }
        self.cursor = self.text.len();
        true
    }

    /// The byte offset of the character before the cursor.
    fn previous_boundary(&self) -> Option<usize> {
        self.text
            .get(..self.cursor)?
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
    }

    /// The byte offset just past the character under the cursor.
    fn next_boundary(&self) -> Option<usize> {
        let tail = self.text.get(self.cursor..)?;
        let character = tail.chars().next()?;
        Some(self.cursor.saturating_add(character.len_utf8()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_and_deleting_track_the_cursor() {
        let mut input = TextInput::new();
        assert!(input.insert('r'));
        assert!(input.insert('s'));
        assert_eq!(input.text(), "rs");
        assert_eq!(input.cursor(), 2);
        assert!(input.delete_backward());
        assert_eq!(input.text(), "r");
        assert!(input.delete_backward());
        assert!(input.is_empty());
        assert!(
            !input.delete_backward(),
            "deleting an empty buffer changes nothing, so it must not redraw"
        );
    }

    #[test]
    fn a_seeded_buffer_puts_the_cursor_at_the_end_so_the_filter_can_be_extended() {
        let mut input = TextInput::seeded("rustc");
        assert_eq!(input.cursor(), 5);
        assert!(input.insert('!'));
        assert_eq!(input.text(), "rustc!");
    }

    #[test]
    fn multi_byte_text_never_splits_a_character() {
        let mut input = TextInput::seeded("日本語");
        assert_eq!(input.cursor(), 9);
        assert!(input.move_left());
        assert_eq!(input.cursor(), 6);
        assert!(input.delete_forward());
        assert_eq!(input.text(), "日本");
        assert!(input.move_left());
        assert!(input.delete_backward());
        assert_eq!(input.text(), "本");
    }

    #[test]
    fn the_cursor_column_counts_cells_not_bytes() {
        let input = TextInput::seeded("日本");
        assert_eq!(input.cursor(), 6, "two three-byte characters");
        assert_eq!(input.cursor_column(), 4, "each occupies two cells");
    }

    #[test]
    fn inserting_in_the_middle_lands_before_the_cursor_character() {
        let mut input = TextInput::seeded("ac");
        assert!(input.move_left());
        assert!(input.insert('b'));
        assert_eq!(input.text(), "abc");
        assert_eq!(input.cursor(), 2);
    }

    #[test]
    fn word_deletion_removes_trailing_spaces_with_the_word() {
        let mut input = TextInput::seeded("export snapshot /tmp/a.json");
        assert!(input.delete_word_backward());
        assert_eq!(input.text(), "export snapshot ");
        assert!(input.delete_word_backward());
        assert_eq!(input.text(), "export ");
        assert!(input.delete_word_backward());
        assert!(input.is_empty());
        assert!(!input.delete_word_backward());
    }

    #[test]
    fn word_deletion_only_touches_text_before_the_cursor() {
        let mut input = TextInput::seeded("one two");
        assert!(input.move_to_start());
        assert!(
            !input.delete_word_backward(),
            "there is nothing before the cursor"
        );
        assert_eq!(input.text(), "one two");
    }

    #[test]
    fn cursor_movement_reports_when_it_is_already_at_the_boundary() {
        let mut input = TextInput::seeded("ab");
        assert!(!input.move_right(), "already at the end");
        assert!(!input.move_to_end());
        assert!(input.move_to_start());
        assert!(!input.move_left(), "already at the start");
        assert!(!input.move_to_start());
    }

    #[test]
    fn clearing_reports_a_change_only_once() {
        let mut input = TextInput::seeded("rustc");
        assert!(input.clear());
        assert!(!input.clear());
    }

    #[test]
    fn the_buffer_is_bounded_so_key_repeat_cannot_grow_it_without_limit() {
        let mut input = TextInput::new();
        for _ in 0..(MAX_INPUT_CHARS * 2) {
            let _ = input.insert('x');
        }
        assert_eq!(input.len_chars(), MAX_INPUT_CHARS);
        assert!(!input.insert('x'), "a full buffer refuses more input");
    }

    #[test]
    fn a_seeded_buffer_is_truncated_to_the_bound() {
        let long = "y".repeat(MAX_INPUT_CHARS * 3);
        let input = TextInput::seeded(&long);
        assert_eq!(input.len_chars(), MAX_INPUT_CHARS);
    }

    #[test]
    fn setting_the_same_text_is_not_a_change() {
        let mut input = TextInput::seeded("sort cpu");
        assert!(!input.set("sort cpu"));
        assert!(input.set("sort memory"));
        assert_eq!(input.text(), "sort memory");
        assert_eq!(input.cursor(), input.text().len());
    }

    #[test]
    fn deleting_forward_at_the_end_changes_nothing() {
        let mut input = TextInput::seeded("ab");
        assert!(!input.delete_forward());
        assert_eq!(input.text(), "ab");
    }
}
