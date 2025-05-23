use std::io::{BufRead as _, Write as _};
use std::{fmt, fs, io};
use std::{fs::File, io::BufReader};

use crossterm::{cursor, execute, terminal};

use super::{Read, INITIAL_BUFFER_CAPACITY, PROMPT};
use crate::dprintln;
use crate::{
    output::{debugger_colors, Output},
    term::{self, Key},
};

// Note: `TerminalHistory` could use a cyclic fixed-size buffer and store fixed-size strings, but
// the performance benifits have not been deemed great enough, considering this only affects the
// interactive terminal reading mode.

/// Interactive unbuffered terminal.
#[derive(Debug)]
pub struct Terminal {
    stderr: io::Stderr,
    buffer: String,
    /// Byte index.
    cursor: usize,
    /// Visible line cursor in terminal (char index, not byte index).
    visible_cursor: usize,
    /// History list and file.
    history: TerminalHistory,
}

/// All history information for `Terminal`.
#[derive(Debug)]
struct TerminalHistory {
    list: Vec<String>,
    /// Focused item in history, or new entry if index==length.
    index: usize,
    /// `None` indicates failure to open file.
    file: Option<File>,
}

impl Terminal {
    pub fn new() -> Self {
        Self {
            stderr: io::stderr(),
            buffer: String::with_capacity(INITIAL_BUFFER_CAPACITY),
            cursor: 0,
            visible_cursor: 0,
            history: TerminalHistory::new(),
        }
    }

    /// Returns `true` if current line is a new line, rather than a focused history item.
    fn is_next(&self) -> bool {
        debug_assert!(
            self.history.index <= self.history.list.len(),
            "index went past history"
        );
        self.history.index >= self.history.list.len()
    }

    /// Run before modifying `next`.
    /// If focused on a historic item, clone it to `next` and update index.
    fn update_next(&mut self) {
        if self.is_next() {
            return;
        }
        self.buffer = self
            .history
            .list
            .get(self.history.index)
            .expect("checked above")
            .clone();
        self.history.index = self.history.list.len();
    }

    /// Get next or historic line, from history index.
    fn get_current(&self) -> &str {
        if self.is_next() {
            &self.buffer
        } else {
            self.history
                .list
                .get(self.history.index)
                .expect("checked above")
        }
    }

    /// Clear current line, draw REPL prompt and current input, and set cursor position.
    fn print_prompt(&mut self) {
        // Don't use `dprint(ln)!` in this function: we already have a handle to `stderr` and want
        // to have control over conditions and attributes for printing.

        // Equivalent to `write!(... "\r")`
        execute!(
            self.stderr,
            terminal::Clear(terminal::ClearType::CurrentLine),
            cursor::MoveToColumn(0),
        )
        .expect("failed to clear line and move cursor");

        // Print prompt and current input
        // Equivalent code found in non-terminal source
        if Output::is_minimal() {
            write!(&self.stderr, "{}", PROMPT)
        } else {
            write!(
                &self.stderr,
                "\x1b[1;{}m{}\x1b[0m",
                debugger_colors::PRIMARY,
                PROMPT
            )
        }
        .expect("failed to print debugger prompt");

        // Print current input
        // Inline `self.get_current()` due to borrowing issues
        let current = if self.is_next() {
            &self.buffer
        } else {
            self.history
                .list
                .get(self.history.index)
                .expect("checked above")
        };
        write!(self.stderr, "{}", current).expect("failed to print debugger input");

        // Set final cursor position
        execute!(
            self.stderr,
            cursor::MoveToColumn((PROMPT.len() + self.visible_cursor) as u16),
        )
        .expect("failed to move cursor");

        // Previous `execute!` call flushed output already
    }

    // Returns `true` indicates to break loop (EOL). Only occurs on `Key::Enter` when buffer
    // is non-empty.
    fn handle_key(&mut self, key: Key) -> bool {
        match key {
            Key::Enter => {
                if self.is_next() && self.buffer.trim().is_empty() {
                    self.buffer.clear();
                    self.visible_cursor = 0;
                    println!();
                } else {
                    self.update_next();
                    return true;
                }
            }

            Key::Char(ch) => match ch {
                // Ignore ASCII control characters
                '\x00'..='\x1f' | '\x7f' => (),

                // Pasting should be automatically supported, since terminals simulate typing each
                // character
                _ => {
                    self.update_next();
                    insert_char_index(&mut self.buffer, self.visible_cursor, ch);
                    self.visible_cursor += 1;
                }
            },

            Key::Backspace => {
                self.update_next();
                if self.visible_cursor > 0
                    && self.visible_cursor <= self.get_current().chars().count()
                {
                    self.visible_cursor -= 1;
                    remove_char_index(&mut self.buffer, self.visible_cursor);
                }
            }
            Key::Delete => {
                self.update_next();
                if self.visible_cursor < self.get_current().chars().count() {
                    remove_char_index(&mut self.buffer, self.visible_cursor);
                }
            }

            // Left/right single character in input
            Key::Left => {
                if self.visible_cursor > 0 {
                    self.visible_cursor -= 1;
                }
            }
            Key::Right => {
                if self.visible_cursor < self.get_current().chars().count() {
                    self.visible_cursor += 1;
                }
            }

            // Left/right entire word in input
            Key::CtrlLeft => {
                self.visible_cursor =
                    find_word_back(self.get_current(), self.visible_cursor, false);
            }
            Key::CtrlRight => {
                self.visible_cursor =
                    find_word_next(self.get_current(), self.visible_cursor, false);
            }

            // Back/forth through history
            Key::Up => {
                if self.history.index > 0 {
                    self.history.index -= 1;
                    self.visible_cursor = self.get_current().chars().count();
                }
            }
            Key::Down => {
                if self.history.index < self.history.list.len() {
                    self.history.index += 1;
                    self.visible_cursor = self.get_current().chars().count();
                }
            }
        }
        false
    }

    /// Read keys until newline.
    fn read_line_raw(&mut self) {
        term::enable_raw_mode();
        loop {
            // Technically redrawing of prompt could be avoided, but this method makes it much
            // simpler and less error-prone
            self.print_prompt();
            let key = term::read_key();
            if self.handle_key(key) {
                break; // EOL
            }
        }
        term::disable_raw_mode();
        println!();
    }

    /// Read entire (multi-command) line from terminal.
    fn read_line(&mut self) {
        self.buffer.clear();
        self.visible_cursor = 0;

        self.read_line_raw();
        debug_assert!(
            !self.buffer.trim().is_empty(),
            "should have read characters until non-empty"
        );

        // Push to history if different to last line
        if self
            .history
            .list
            .last()
            .is_none_or(|previous| previous != &self.buffer)
        {
            self.history.push(self.buffer.clone());
        }
        // Always reset index to next line
        self.history.index = self.history.list.len();
    }

    /// Returns next command from line buffer.
    fn get_next_command(&mut self) -> &str {
        let rest = &self.buffer[self.cursor..];
        match rest.find(';') {
            // Multiple commands in buffer
            // Take first command and update head index
            Some(index) => {
                self.cursor += index + 1;
                &rest[..index]
            }
            // Rest of buffer is 1 command
            // Take rest of buffer and reset head index
            None => {
                self.cursor = 0;
                rest
            }
        }
    }
}

impl Read for Terminal {
    fn read(&mut self) -> Option<&str> {
        // Reached end of line buffer: read new line
        if self.cursor == 0 {
            self.read_line();
        }
        Some(self.get_next_command())
    }
}

impl TerminalHistory {
    const FILE_NAME: &str = "lace-debugger-history";

    pub fn new() -> Self {
        let mut file = Self::get_file();
        let list = Self::read_file(file.as_mut());
        let index = list.len();
        Self { list, index, file }
    }

    /// Push line into list and write to file.
    pub fn push(&mut self, line: String) {
        if let Some(file) = &mut self.file {
            if writeln!(file, "{}", line).is_err() {
                Self::report_error("Failed to write to file");
            }
        }
        self.list.push(line);
    }

    /// Returns empty vector if failed to read.
    fn read_file(file: Option<&mut File>) -> Vec<String> {
        let Some(file) = file else {
            return Vec::new();
        };
        let mut history = Vec::new();
        for line in BufReader::new(file).lines() {
            let Ok(line) = line else {
                Self::report_error("Failed to read from file");
                break;
            };
            history.push(line);
        }
        history
    }

    /// Get file path and open file.
    ///
    /// Returns `None` if anything fails.
    fn get_file() -> Option<File> {
        let Some(parent_dir) = dirs_next::cache_dir() else {
            Self::report_error(format_args!(
                "Cannot retrieve user cache directory. Eg. $XDG_CACHE_HOME"
            ));
            return None;
        };
        if !parent_dir.is_dir() {
            Self::report_error(format_args!(
                "Parent directory is not a directory: {}",
                parent_dir.display(),
            ));
            return None;
        }

        let file_path = parent_dir.join(Self::FILE_NAME);
        if file_path.exists() && !file_path.is_file() {
            Self::report_error(format_args!(
                "File exists but is not a regular file: {}",
                file_path.display(),
            ));
            return None;
        }

        match fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&file_path)
        {
            Ok(file) => Some(file),
            Err(_error) => {
                Self::report_error(format_args!("Failed to open file: {}", file_path.display()));
                None
            }
        }
    }

    fn report_error(message: impl fmt::Display) {
        dprintln!(
            Always,
            Error,
            "Error with debugger history file: {}",
            message,
        );
    }
}

/// Return character index of start of the word to the left of cursor. Uses Vim rules.
///
/// - If `full_word == true`, then it considers a word boundary to only be between whitespace and
///   non-whitespace characters. Eg. `abc def` has word boundaries directly before and after the
///   whitespace character.
/// - If `full_word == false`, then it additionally considers a word boundary
///   to be between alphanumeric characters and non-alphanumeric characters. Eg: `abc+def` has word
///   boundaries directly before and after the `+` character.
fn find_word_next(string: &str, cursor: usize, full_word: bool) -> usize {
    let mut chars = string.char_indices().skip(cursor);
    // At end of line (covers empty string case)
    let Some((_, first)) = chars.next() else {
        return string.len();
    };
    if first.is_whitespace() {
        // On a space
        // Look for first non-space character
        for (i, ch) in chars.by_ref() {
            if !ch.is_whitespace() {
                return i;
            }
        }
    } else {
        // On non-space
        let alnum = first.is_alphanumeric();
        while let Some((i, ch)) = chars.next() {
            // Space found
            // Look for first non-space character
            if ch.is_whitespace() {
                for (i, ch) in chars.by_ref() {
                    if !ch.is_whitespace() {
                        return i;
                    }
                }
            }
            // First punctuation after word
            // OR first word after punctuation
            // (If distinguishing words and punctuation)
            if !full_word && ch.is_alphanumeric() != alnum {
                return i;
            }
        }
    }
    // No next word found
    // Go to end of line
    string.len()
}

/// Return character index of end of the word to the right of cursor. Uses Vim rules.
///
/// See [`find_word_next`]
// TODO(refactor/opt): Rewrite to be more idiomaticly Rust
fn find_word_back(string: &str, mut cursor: usize, full_word: bool) -> usize {
    // At start of line
    if cursor <= 1 {
        return 0;
    }
    // Start at previous character
    cursor -= 1;
    // On a sequence of spaces (>=1)
    // Look for end of previous word, start from there instead
    while cursor > 0 && string.chars().nth(cursor).unwrap().is_whitespace() {
        cursor -= 1;
    }
    // Now on a non-space
    let alnum = string.chars().nth(cursor).unwrap().is_alphanumeric();
    while cursor > 0 {
        cursor -= 1;
        // Space found
        // OR first punctuation before word
        // OR first word before punctuation
        // Word starts at next index
        // (If distinguishing words and punctuation)
        if string.chars().nth(cursor).unwrap().is_whitespace()
            || (!full_word && string.chars().nth(cursor).unwrap().is_alphanumeric() != alnum)
        {
            return cursor + 1;
        }
    }
    // No previous word found
    // Go to start of line
    0
}

/// Insert a character at a character index.
fn insert_char_index(string: &mut String, char_index: usize, ch: char) {
    let (byte_index, char_count) = count_chars_bytes(string, char_index);
    assert!(char_index <= char_count, "out-of-bounds char index");
    string.insert(byte_index, ch)
}
/// Remove a character at a character index.
fn remove_char_index(string: &mut String, char_index: usize) -> char {
    let (byte_index, char_count) = count_chars_bytes(string, char_index);
    assert!(char_index < char_count, "out-of-bounds char index");
    string.remove(byte_index)
}

/// Returns the byte index from a character index, and the total character count.
fn count_chars_bytes(string: &str, char_index: usize) -> (usize, usize) {
    let mut byte_index = string.len();
    let mut char_count = 0;
    for (i, (j, _)) in string.char_indices().enumerate() {
        if i == char_index {
            byte_index = j;
        }
        char_count += 1;
    }
    (byte_index, char_count)
}
