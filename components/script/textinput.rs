/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Common handling of keyboard input and state management for text input controls

use clipboard_provider::ClipboardProvider;
use dom::bindings::str::DOMString;
use dom::keyboardevent::KeyboardEvent;
use msg::constellation_msg::{Key, KeyModifiers};
use std::borrow::ToOwned;
use std::cmp::{max, min};
use std::default::Default;
use std::ops::Range;
use std::usize;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy, PartialEq)]
pub enum Selection {
    Selected,
    NotSelected
}

#[derive(Clone, Copy, Debug, JSTraceable, MallocSizeOf, PartialEq)]
pub enum SelectionDirection {
    Forward,
    Backward,
    None,
}

impl From<DOMString> for SelectionDirection {
    fn from(direction: DOMString) -> SelectionDirection {
        match direction.as_ref() {
            "forward" => SelectionDirection::Forward,
            "backward" => SelectionDirection::Backward,
            _ => SelectionDirection::None,
        }
    }
}

impl From<SelectionDirection> for DOMString {
    fn from(direction: SelectionDirection) -> DOMString {
        match direction {
            SelectionDirection::Forward => DOMString::from("forward"),
            SelectionDirection::Backward => DOMString::from("backward"),
            SelectionDirection::None => DOMString::from("none"),
        }
    }
}

#[derive(Clone, Copy, Debug, JSTraceable, MallocSizeOf, PartialEq, PartialOrd)]
pub struct TextPoint {
    /// 0-based line number
    pub line: usize,
    /// 0-based column number in UTF-8 bytes
    pub index: usize,
}

#[derive(Clone, Copy, PartialEq)]
pub struct SelectionState {
    start: TextPoint,
    end: TextPoint,
    direction: SelectionDirection,
}

/// Encapsulated state for handling keyboard input in a single or multiline text input control.
#[derive(JSTraceable, MallocSizeOf)]
pub struct TextInput<T: ClipboardProvider> {
    /// Current text input content, split across lines without trailing '\n'
    lines: Vec<DOMString>,
    /// Current cursor input point
    pub edit_point: TextPoint,
    /// The current selection goes from the selection_origin until the edit_point. Note that the
    /// selection_origin may be after the edit_point, in the case of a backward selection.
    pub selection_origin: Option<TextPoint>,
    /// Is this a multiline input?
    multiline: bool,
    #[ignore_malloc_size_of = "Can't easily measure this generic type"]
    clipboard_provider: T,
    /// The maximum number of UTF-16 code units this text input is allowed to hold.
    ///
    /// <https://html.spec.whatwg.org/multipage/#attr-fe-maxlength>
    pub max_length: Option<usize>,
    pub min_length: Option<usize>,
    pub selection_direction: SelectionDirection,
}

/// Resulting action to be taken by the owner of a text input that is handling an event.
pub enum KeyReaction {
    TriggerDefaultAction,
    DispatchInput,
    RedrawSelection,
    Nothing,
}

impl Default for TextPoint {
    fn default() -> TextPoint {
        TextPoint {
            line: 0,
            index: 0,
        }
    }
}

/// Control whether this control should allow multiple lines.
#[derive(Eq, PartialEq)]
pub enum Lines {
    Single,
    Multiple,
}

/// The direction in which to delete a character.
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum Direction {
    Forward,
    Backward
}


/// Was the keyboard event accompanied by the standard control modifier,
/// i.e. cmd on Mac OS or ctrl on other platforms.
#[cfg(target_os = "macos")]
fn is_control_key(mods: KeyModifiers) -> bool {
    mods.contains(KeyModifiers::SUPER) && !mods.contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

#[cfg(not(target_os = "macos"))]
fn is_control_key(mods: KeyModifiers) -> bool {
    mods.contains(KeyModifiers::CONTROL) && !mods.contains(KeyModifiers::SUPER | KeyModifiers::ALT)
}

/// The length in bytes of the first n characters in a UTF-8 string.
///
/// If the string has fewer than n characters, returns the length of the whole string.
fn len_of_first_n_chars(text: &str, n: usize) -> usize {
    match text.char_indices().take(n).last() {
        Some((index, ch)) => index + ch.len_utf8(),
        None => 0
    }
}

/// The length in bytes of the first n code units a string when encoded in UTF-16.
///
/// If the string is fewer than n code units, returns the length of the whole string.
fn len_of_first_n_code_units(text: &str, n: usize) -> usize {
    let mut utf8_len = 0;
    let mut utf16_len = 0;
    for c in text.chars() {
        utf16_len += c.len_utf16();
        if utf16_len > n {
            break;
        }
        utf8_len += c.len_utf8();
    }
    utf8_len
}

impl<T: ClipboardProvider> TextInput<T> {
    /// Instantiate a new text input control
    pub fn new(lines: Lines, initial: DOMString,
               clipboard_provider: T, max_length: Option<usize>,
               min_length: Option<usize>,
               selection_direction: SelectionDirection) -> TextInput<T> {
        let mut i = TextInput {
            lines: vec!(),
            edit_point: Default::default(),
            selection_origin: None,
            multiline: lines == Lines::Multiple,
            clipboard_provider: clipboard_provider,
            max_length: max_length,
            min_length: min_length,
            selection_direction: selection_direction,
        };
        i.set_content(initial, false);
        i
    }

    /// Remove a character at the current editing point
    pub fn delete_char(&mut self, dir: Direction) {
        if self.selection_origin.is_none() || self.selection_origin == Some(self.edit_point) {
            self.adjust_horizontal_by_one(dir, Selection::Selected);
        }
        self.replace_selection(DOMString::new());
    }

    /// Insert a character at the current editing point
    pub fn insert_char(&mut self, ch: char) {
        self.insert_string(ch.to_string());
    }

    /// Insert a string at the current editing point
    pub fn insert_string<S: Into<String>>(&mut self, s: S) {
        if self.selection_origin.is_none() {
            self.selection_origin = Some(self.edit_point);
        }
        self.replace_selection(DOMString::from(s.into()));
    }

    /// The selection origin, or the edit point if there is no selection. Note that the selection
    /// origin may be after the edit point, in the case of a backward selection.
    pub fn selection_origin_or_edit_point(&self) -> TextPoint {
        self.selection_origin.unwrap_or(self.edit_point)
    }

    /// The start of the selection (or the edit point, if there is no selection). Always less than
    /// or equal to selection_end(), regardless of the selection direction.
    pub fn selection_start(&self) -> TextPoint {
        match self.selection_direction {
            SelectionDirection::None | SelectionDirection::Forward => self.selection_origin_or_edit_point(),
            SelectionDirection::Backward => self.edit_point,
        }
    }

    /// The UTF-8 byte offset of the selection_start()
    pub fn selection_start_offset(&self) -> usize {
        self.text_point_to_offset(&self.selection_start())
    }

    /// The end of the selection (or the edit point, if there is no selection). Always greater
    /// than or equal to selection_start(), regardless of the selection direction.
    pub fn selection_end(&self) -> TextPoint {
        match self.selection_direction {
            SelectionDirection::None | SelectionDirection::Forward => self.edit_point,
            SelectionDirection::Backward => self.selection_origin_or_edit_point(),
        }
    }

    /// The UTF-8 byte offset of the selection_end()
    pub fn selection_end_offset(&self) -> usize {
        self.text_point_to_offset(&self.selection_end())
    }

    /// Whether or not there is an active selection (the selection may be zero-length)
    #[inline]
    pub fn has_selection(&self) -> bool {
        self.selection_origin.is_some()
    }

    /// Returns a tuple of (start, end) giving the bounds of the current selection. start is always
    /// less than or equal to end.
    pub fn sorted_selection_bounds(&self) -> (TextPoint, TextPoint) {
        (self.selection_start(), self.selection_end())
    }

    /// Return the selection range as UTF-8 byte offsets from the start of the content.
    ///
    /// If there is no selection, returns an empty range at the edit point.
    pub fn sorted_selection_offsets_range(&self) -> Range<usize> {
        self.selection_start_offset() .. self.selection_end_offset()
    }

    /// The state of the current selection. Can be used to compare whether selection state has changed.
    pub fn selection_state(&self) -> SelectionState {
        SelectionState {
            start: self.selection_start(),
            end: self.selection_end(),
            direction: self.selection_direction,
        }
    }

    // Check that the selection is valid.
    fn assert_ok_selection(&self) {
        if let Some(begin) = self.selection_origin {
            debug_assert!(begin.line < self.lines.len());
            debug_assert!(begin.index <= self.lines[begin.line].len());

            match self.selection_direction {
                SelectionDirection::None | SelectionDirection::Forward => {
                    debug_assert!(begin <= self.edit_point)
                },

                SelectionDirection::Backward => {
                    debug_assert!(self.edit_point <= begin)
                },
            }
        }

        debug_assert!(self.edit_point.line < self.lines.len());
        debug_assert!(self.edit_point.index <= self.lines[self.edit_point.line].len());
    }

    pub fn get_selection_text(&self) -> Option<String> {
        let text = self.fold_selection_slices(String::new(), |s, slice| s.push_str(slice));
        if text.is_empty() {
            return None
        }
        Some(text)
    }

    /// The length of the selected text in UTF-16 code units.
    fn selection_utf16_len(&self) -> usize {
        self.fold_selection_slices(0usize,
            |len, slice| *len += slice.chars().map(char::len_utf16).sum::<usize>())
    }

    /// Run the callback on a series of slices that, concatenated, make up the selected text.
    ///
    /// The accumulator `acc` can be mutated by the callback, and will be returned at the end.
    fn fold_selection_slices<B, F: FnMut(&mut B, &str)>(&self, mut acc: B, mut f: F) -> B {
        if self.has_selection() {
            let (start, end) = self.sorted_selection_bounds();

            if start.line == end.line {
                f(&mut acc, &self.lines[start.line][start.index..end.index])
            } else {
                f(&mut acc, &self.lines[start.line][start.index..]);
                for line in &self.lines[start.line + 1 .. end.line] {
                    f(&mut acc, "\n");
                    f(&mut acc, line);
                }
                f(&mut acc, "\n");
                f(&mut acc, &self.lines[end.line][..end.index])
            }
        }

        acc
    }

    pub fn replace_selection(&mut self, insert: DOMString) {
        if !self.has_selection() {
            return
        }

        let (start, end) = self.sorted_selection_bounds();

        let allowed_to_insert_count = if let Some(max_length) = self.max_length {
            let len_after_selection_replaced = self.utf16_len() - self.selection_utf16_len();
            if len_after_selection_replaced >= max_length {
                // If, after deleting the selection, the len is still greater than the max
                // length, then don't delete/insert anything
                return
            }

            max_length - len_after_selection_replaced
        } else {
            usize::MAX
        };

        let last_char_index = len_of_first_n_code_units(&*insert, allowed_to_insert_count);
        let chars_to_insert = &insert[..last_char_index];

        self.clear_selection();

        let new_lines = {
            let prefix = &self.lines[start.line][..start.index];
            let suffix = &self.lines[end.line][end.index..];
            let lines_prefix = &self.lines[..start.line];
            let lines_suffix = &self.lines[end.line + 1..];

            let mut insert_lines = if self.multiline {
                chars_to_insert.split('\n').map(|s| DOMString::from(s)).collect()
            } else {
                vec!(DOMString::from(chars_to_insert))
            };

            // FIXME(ajeffrey): effecient append for DOMStrings
            let mut new_line = prefix.to_owned();

            new_line.push_str(&insert_lines[0]);
            insert_lines[0] = DOMString::from(new_line);

            let last_insert_lines_index = insert_lines.len() - 1;
            self.edit_point.index = insert_lines[last_insert_lines_index].len();
            self.edit_point.line = start.line + last_insert_lines_index;

            // FIXME(ajeffrey): effecient append for DOMStrings
            insert_lines[last_insert_lines_index].push_str(suffix);

            let mut new_lines = vec!();
            new_lines.extend_from_slice(lines_prefix);
            new_lines.extend_from_slice(&insert_lines);
            new_lines.extend_from_slice(lines_suffix);
            new_lines
        };

        self.lines = new_lines;
        self.assert_ok_selection();
    }

    /// Return the length in UTF-8 bytes of the current line under the editing point.
    pub fn current_line_length(&self) -> usize {
        self.lines[self.edit_point.line].len()
    }

    /// Adjust the editing point position by a given of lines. The resulting column is
    /// as close to the original column position as possible.
    pub fn adjust_vertical(&mut self, adjust: isize, select: Selection) {
        if !self.multiline {
            return;
        }

        if select == Selection::Selected {
            if self.selection_origin.is_none() {
                self.selection_origin = Some(self.edit_point);
            }
        } else {
            self.clear_selection();
        }

        assert!(self.edit_point.line < self.lines.len());

        let target_line: isize = self.edit_point.line as isize + adjust;

        if target_line < 0 {
            self.edit_point.index = 0;
            self.edit_point.line = 0;
            return;
        } else if target_line as usize >= self.lines.len() {
            self.edit_point.line = self.lines.len() - 1;
            self.edit_point.index = self.current_line_length();
            return;
        }

        let col = self.lines[self.edit_point.line][..self.edit_point.index].chars().count();
        self.edit_point.line = target_line as usize;
        self.edit_point.index = len_of_first_n_chars(&self.lines[self.edit_point.line], col);
        self.assert_ok_selection();
    }

    /// Adjust the editing point position by a given number of bytes. If the adjustment
    /// requested is larger than is available in the current line, the editing point is
    /// adjusted vertically and the process repeats with the remaining adjustment requested.
    pub fn adjust_horizontal(&mut self, adjust: isize, select: Selection) {
        let direction = if adjust >= 0 { Direction::Forward } else { Direction::Backward };
        if self.adjust_selection_for_horizontal_change(direction, select) {
            return
        }
        self.perform_horizontal_adjustment(adjust, select);
    }

    pub fn adjust_horizontal_by_one(&mut self, direction: Direction, select: Selection) {
        if self.adjust_selection_for_horizontal_change(direction, select) {
            return
        }
        let adjust = {
            let current_line = &self.lines[self.edit_point.line];
            match direction {
                Direction::Forward => {
                    match current_line[self.edit_point.index..].graphemes(true).next() {
                        Some(c) => c.len() as isize,
                        None => 1,  // Going to the next line is a "one byte" offset
                    }
                }
                Direction::Backward => {
                    match current_line[..self.edit_point.index].graphemes(true).next_back() {
                        Some(c) => -(c.len() as isize),
                        None => -1,  // Going to the previous line is a "one byte" offset
                    }
                }
            }
        };
        self.perform_horizontal_adjustment(adjust, select);
    }

    /// Return whether to cancel the caret move
    fn adjust_selection_for_horizontal_change(&mut self, adjust: Direction, select: Selection)
                                              -> bool {
        if select == Selection::Selected {
            if self.selection_origin.is_none() {
                self.selection_origin = Some(self.edit_point);
            }

            self.selection_direction = match adjust {
                Direction::Backward => SelectionDirection::Backward,
                Direction::Forward => SelectionDirection::Forward,
            };
        } else {
            if self.has_selection() {
                self.edit_point = match adjust {
                    Direction::Backward => self.selection_start(),
                    Direction::Forward => self.selection_end(),
                };
                self.clear_selection();
                return true
            }
        }
        false
    }

    fn perform_horizontal_adjustment(&mut self, adjust: isize, select: Selection) {
        if adjust < 0 {
            let remaining = self.edit_point.index;
            if adjust.abs() as usize > remaining && self.edit_point.line > 0 {
                self.adjust_vertical(-1, select);
                self.edit_point.index = self.current_line_length();
                self.adjust_horizontal(adjust + remaining as isize + 1, select);
            } else {
                self.edit_point.index = max(0, self.edit_point.index as isize + adjust) as usize;
            }
        } else {
            let remaining = self.current_line_length() - self.edit_point.index;
            if adjust as usize > remaining && self.lines.len() > self.edit_point.line + 1 {
                self.adjust_vertical(1, select);
                self.edit_point.index = 0;
                // one shift is consumed by the change of line, hence the -1
                self.adjust_horizontal(adjust - remaining as isize - 1, select);
            } else {
                self.edit_point.index = min(self.current_line_length(),
                                            self.edit_point.index + adjust as usize);
            }
        }
        self.assert_ok_selection();
    }

    /// Deal with a newline input.
    pub fn handle_return(&mut self) -> KeyReaction {
        if !self.multiline {
            KeyReaction::TriggerDefaultAction
        } else {
            self.insert_char('\n');
            KeyReaction::DispatchInput
        }
    }

    /// Select all text in the input control.
    pub fn select_all(&mut self) {
        self.selection_origin = Some(TextPoint {
            line: 0,
            index: 0,
        });
        let last_line = self.lines.len() - 1;
        self.edit_point.line = last_line;
        self.edit_point.index = self.lines[last_line].len();
        self.assert_ok_selection();
    }

    /// Remove the current selection.
    pub fn clear_selection(&mut self) {
        self.selection_origin = None;
        self.selection_direction = SelectionDirection::None;
    }

    /// Remove the current selection and set the edit point to the end of the content.
    pub fn clear_selection_to_limit(&mut self, direction: Direction, update_text_cursor: bool) {
        self.clear_selection();
        self.adjust_horizontal_to_limit(direction, Selection::NotSelected, update_text_cursor);
    }

    pub fn adjust_horizontal_by_word(&mut self, direction: Direction, select: Selection) {
        if self.adjust_selection_for_horizontal_change(direction, select) {
            return
        }
        let shift_increment: isize =  {
            let input: &str;
            match direction {
                Direction::Backward => {
                    let remaining = self.edit_point.index;
                    let current_line = self.edit_point.line;
                    let mut newline_adjustment = 0;
                    if remaining == 0 && current_line > 0 {
                        input = &self
                            .lines[current_line-1];
                        newline_adjustment = 1;
                    } else {
                        input = &self
                            .lines[current_line]
                            [..remaining];
                    }

                    let mut iter = input.split_word_bounds().rev();
                    let mut shift_temp: isize = 0;
                    loop {
                        match iter.next() {
                            None => break,
                            Some(x) => {
                                shift_temp += - (x.len() as isize);
                                if x.chars().any(|x| x.is_alphabetic() || x.is_numeric()) {
                                    break;
                                }
                            }
                        }
                    }
                    shift_temp - newline_adjustment
                }
                Direction::Forward => {
                    let remaining = self.current_line_length() - self.edit_point.index;
                    let current_line = self.edit_point.line;
                    let mut newline_adjustment = 0;
                    if remaining == 0 && self.lines.len() > self.edit_point.line + 1 {
                        input = &self
                            .lines[current_line + 1];
                        newline_adjustment = 1;
                    } else {
                        input = &self
                            .lines[current_line]
                            [self.edit_point.index..];
                    }

                    let mut iter = input.split_word_bounds();
                    let mut shift_temp: isize = 0;
                    loop {
                        match iter.next() {
                            None => break,
                            Some(x) => {
                                shift_temp += x.len() as isize;
                                if x.chars().any(|x| x.is_alphabetic() || x.is_numeric()) {
                                    break;
                                }
                            }
                        }
                    }
                    shift_temp + newline_adjustment
                }
            }
        };

        self.adjust_horizontal(shift_increment, select);
    }

    pub fn adjust_horizontal_to_line_end(&mut self, direction: Direction, select: Selection) {
        if self.adjust_selection_for_horizontal_change(direction, select) {
            return
        }
        let shift: isize = {
            let current_line = &self.lines[self.edit_point.line];
            match direction {
                Direction::Backward => {
                    - (current_line[..self.edit_point.index].len() as isize)
                },
                Direction::Forward => {
                    current_line[self.edit_point.index..].len() as isize
                }
            }
        };
        self.perform_horizontal_adjustment(shift, select);
    }

    pub fn adjust_horizontal_to_limit(&mut self, direction: Direction, select: Selection, update_text_cursor: bool) {
        if self.adjust_selection_for_horizontal_change(direction, select) {
            return
        }
        if update_text_cursor {
            match direction {
                Direction::Backward => {
                    self.edit_point.line = 0;
                    self.edit_point.index = 0;
                },
                Direction::Forward => {
                    self.edit_point.line = &self.lines.len() - 1;
                    self.edit_point.index = (&self.lines[&self.lines.len() - 1]).len();
                }
            }
        }
    }

    /// Process a given `KeyboardEvent` and return an action for the caller to execute.
    pub fn handle_keydown(&mut self, event: &KeyboardEvent) -> KeyReaction {
        if let Some(key) = event.get_key() {
            self.handle_keydown_aux(event.printable(), key, event.get_key_modifiers())
        } else {
            KeyReaction::Nothing
        }
    }

    pub fn handle_keydown_aux(&mut self,
                              printable: Option<char>,
                              key: Key,
                              mods: KeyModifiers) -> KeyReaction {
        let maybe_select = if mods.contains(KeyModifiers::SHIFT) {
                Selection::Selected
            } else {
                Selection::NotSelected
        };

        match (printable, key) {
            (_, Key::B) if mods.contains(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                self.adjust_horizontal_by_word(Direction::Backward, maybe_select);
                KeyReaction::RedrawSelection
            },
            (_, Key::F) if mods.contains(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                self.adjust_horizontal_by_word(Direction::Forward, maybe_select);
                KeyReaction::RedrawSelection
            },
            (_, Key::A) if mods.contains(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                self.adjust_horizontal_to_line_end(Direction::Backward, maybe_select);
                KeyReaction::RedrawSelection
            },
            (_, Key::E) if mods.contains(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                self.adjust_horizontal_to_line_end(Direction::Forward, maybe_select);
                KeyReaction::RedrawSelection
            },
            #[cfg(target_os = "macos")]
            (None, Key::A) if mods == KeyModifiers::CONTROL => {
                self.adjust_horizontal_to_line_end(Direction::Backward, maybe_select);
                KeyReaction::RedrawSelection
            },
            #[cfg(target_os = "macos")]
            (None, Key::E) if mods == KeyModifiers::CONTROL => {
                self.adjust_horizontal_to_line_end(Direction::Forward, maybe_select);
                KeyReaction::RedrawSelection
            },
            (_, Key::A) if is_control_key(mods) => {
                self.select_all();
                KeyReaction::RedrawSelection
            },
            (_, Key::C) if is_control_key(mods) => {
                if let Some(text) = self.get_selection_text() {
                    self.clipboard_provider.set_clipboard_contents(text);
                }
                KeyReaction::DispatchInput
            },
            (_, Key::V) if is_control_key(mods) => {
                let contents = self.clipboard_provider.clipboard_contents();
                self.insert_string(contents);
                KeyReaction::DispatchInput
            },
            (Some(c), _) => {
                self.insert_char(c);
                KeyReaction::DispatchInput
            },
            (None, Key::Delete) => {
                self.delete_char(Direction::Forward);
                KeyReaction::DispatchInput
            },
            (None, Key::Backspace) => {
                self.delete_char(Direction::Backward);
                KeyReaction::DispatchInput
            },
            #[cfg(target_os = "macos")]
            (None, Key::Left) if mods.contains(KeyModifiers::SUPER) => {
                self.adjust_horizontal_to_line_end(Direction::Backward, maybe_select);
                KeyReaction::RedrawSelection
            },
            #[cfg(target_os = "macos")]
            (None, Key::Right) if mods.contains(KeyModifiers::SUPER) => {
                self.adjust_horizontal_to_line_end(Direction::Forward, maybe_select);
                KeyReaction::RedrawSelection
            },
            #[cfg(target_os = "macos")]
            (None, Key::Up) if mods.contains(KeyModifiers::SUPER) => {
                self.adjust_horizontal_to_limit(Direction::Backward, maybe_select, true);
                KeyReaction::RedrawSelection
            },
            #[cfg(target_os = "macos")]
            (None, Key::Down) if mods.contains(KeyModifiers::SUPER) => {
                self.adjust_horizontal_to_limit(Direction::Forward, maybe_select, true);
                KeyReaction::RedrawSelection
            },
            (None, Key::Left) if mods.contains(KeyModifiers::ALT) => {
                self.adjust_horizontal_by_word(Direction::Backward, maybe_select);
                KeyReaction::RedrawSelection
            },
            (None, Key::Right) if mods.contains(KeyModifiers::ALT) => {
                self.adjust_horizontal_by_word(Direction::Forward, maybe_select);
                KeyReaction::RedrawSelection
            },
            (None, Key::Left) => {
                self.adjust_horizontal_by_one(Direction::Backward, maybe_select);
                KeyReaction::RedrawSelection
            },
            (None, Key::Right) => {
                self.adjust_horizontal_by_one(Direction::Forward, maybe_select);
                KeyReaction::RedrawSelection
            },
            (None, Key::Up) => {
                self.adjust_vertical(-1, maybe_select);
                KeyReaction::RedrawSelection
            },
            (None, Key::Down) => {
                self.adjust_vertical(1, maybe_select);
                KeyReaction::RedrawSelection
            },
            (None, Key::Enter) | (None, Key::KpEnter) => self.handle_return(),
            (None, Key::Home) => {
                #[cfg(not(target_os = "macos"))]
                {
                    self.edit_point.index = 0;
                }
                KeyReaction::RedrawSelection
            },
            (None, Key::End) => {
                #[cfg(not(target_os = "macos"))]
                {
                    self.edit_point.index = self.current_line_length();
                    self.assert_ok_selection();
                }
                KeyReaction::RedrawSelection
            },
            (None, Key::PageUp) => {
                self.adjust_vertical(-28, maybe_select);
                KeyReaction::RedrawSelection
            },
            (None, Key::PageDown) => {
                self.adjust_vertical(28, maybe_select);
                KeyReaction::RedrawSelection
            },
            _ => KeyReaction::Nothing,
        }
    }

    /// Whether the content is empty.
    pub fn is_empty(&self) -> bool {
        self.lines.len() <= 1 && self.lines.get(0).map_or(true, |line| line.is_empty())
    }

    /// The length of the content in bytes.
    pub fn len(&self) -> usize {
        self.lines.iter().fold(0, |m, l| {
            m + l.len() + 1 // + 1 for the '\n'
        }) - 1
    }

    /// The length of the content in bytes.
    pub fn utf16_len(&self) -> usize {
        self.lines.iter().fold(0, |m, l| {
            m + l.chars().map(char::len_utf16).sum::<usize>() + 1 // + 1 for the '\n'
        }) - 1
    }

    /// The length of the content in chars.
    pub fn char_count(&self) -> usize {
        self.lines.iter().fold(0, |m, l| {
            m + l.chars().count() + 1 // + 1 for the '\n'
        }) - 1
    }

    /// Get the current contents of the text input. Multiple lines are joined by \n.
    pub fn get_content(&self) -> DOMString {
        let mut content = "".to_owned();
        for (i, line) in self.lines.iter().enumerate() {
            content.push_str(&line);
            if i < self.lines.len() - 1 {
                content.push('\n');
            }
        }
        DOMString::from(content)
    }

    /// Get a reference to the contents of a single-line text input. Panics if self is a multiline input.
    pub fn single_line_content(&self) -> &DOMString {
        assert!(!self.multiline);
        &self.lines[0]
    }

    /// Get a mutable reference to the contents of a single-line text input. Panics if self is a multiline input.
    pub fn single_line_content_mut(&mut self) -> &mut DOMString {
        assert!(!self.multiline);
        &mut self.lines[0]
    }

    /// Set the current contents of the text input. If this is control supports multiple lines,
    /// any \n encountered will be stripped and force a new logical line.
    pub fn set_content(&mut self, content: DOMString, update_text_cursor: bool) {
        self.lines = if self.multiline {
            // https://html.spec.whatwg.org/multipage/#textarea-line-break-normalisation-transformation
            content.replace("\r\n", "\n")
                   .split(|c| c == '\n' || c == '\r')
                   .map(DOMString::from)
                   .collect()
        } else {
            vec!(content)
        };
        if update_text_cursor {
            self.edit_point.line = min(self.edit_point.line, self.lines.len() - 1);
            self.edit_point.index = min(self.edit_point.index, self.current_line_length());
        }
        self.selection_origin = None;
        self.assert_ok_selection();
    }

    /// Convert a TextPoint into a byte offset from the start of the content.
    fn text_point_to_offset(&self, text_point: &TextPoint) -> usize {
        self.lines.iter().enumerate().fold(0, |acc, (i, val)| {
            if i < text_point.line {
                acc + val.len() + 1 // +1 for the \n
            } else {
                acc
            }
        }) + text_point.index
    }

    /// Convert a byte offset from the start of the content into a TextPoint.
    fn offset_to_text_point(&self, abs_point: usize) -> TextPoint {
        let mut index = abs_point;
        let mut line = 0;
        let last_line_idx = self.lines.len() - 1;
        self.lines.iter().enumerate().fold(0, |acc, (i, val)| {
            if i != last_line_idx {
                let line_end = val.len();
                let new_acc = acc + line_end + 1;
                if abs_point >= new_acc && index > line_end {
                    index -= line_end + 1;
                    line += 1;
                }
                new_acc
            } else {
                acc
            }
        });

        TextPoint {
            line: line, index: index
        }
    }

    pub fn set_selection_range(&mut self, start: u32, end: u32, direction: SelectionDirection) {
        let mut start = start as usize;
        let mut end = end as usize;
        let text_end = self.get_content().len();

        if end > text_end {
            end = text_end;
        }
        if start > end {
            start = end;
        }

        self.selection_direction = direction;

        match direction {
            SelectionDirection::None |
            SelectionDirection::Forward => {
                self.selection_origin = Some(self.offset_to_text_point(start));
                self.edit_point = self.offset_to_text_point(end);
            },
            SelectionDirection::Backward => {
                self.selection_origin = Some(self.offset_to_text_point(end));
                self.edit_point = self.offset_to_text_point(start);
            }
        }
        self.assert_ok_selection();
    }

    pub fn set_edit_point_index(&mut self, index: usize) {
        let byte_size = self.lines[self.edit_point.line]
            .graphemes(true)
            .take(index)
            .fold(0, |acc, x| acc + x.len());
        self.edit_point.index = byte_size;
    }
}
