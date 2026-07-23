//! A crossterm-backed line editor for the interactive prompt.
//!
//! When stdin is a real terminal, this provides raw-mode key-by-key
//! editing: cursor movement, Backspace/Delete, and Up/Down history
//! navigation (in-memory only for now — see the note below). When stdin
//! is redirected (a piped script, or `cargo test`-style automation), it
//! transparently falls back to plain `io::stdin().read_line()` so scripted
//! input keeps working exactly as before.
//!
//! Implemented shortcuts (ion-manual page 84's list, partial):
//! - Ctrl+U: delete from cursor to the start of the line
//! - Esc: clear the current line
//! - Tab: complete builtin names and filesystem paths when unambiguous
//! - Ctrl+C: abort the current line and redraw a fresh prompt (does not
//!   exit the shell)
//! - Ctrl+D on an empty line: EOF (exits the shell); on a non-empty line,
//!   behaves like Delete (matches common shell convention)
//! - Shift+Arrow/Home/End: select text; typing replaces it and
//!   Backspace/Delete removes it
//!
//! NOT implemented: Ctrl+R/Ctrl+S incremental history search, Ctrl+F
//! autosuggestion acceptance, Vi keybindings, and persisting history to
//! `$HOME/.local/share/ion/history` across sessions (history here is
//! in-memory for the current process only) — see ARCHITECTURE.md.

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::Stylize;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::{cursor, execute, terminal};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Runtime on/off switch for live syntax highlighting, toggled via the
/// `highlight on|off` builtin (see `shell.rs`). A plain global rather than
/// a field threaded through `LineEditor`/`redraw` because `redraw` is a
/// free function called from deep inside the keystroke loop, and the
/// toggle needs to be flippable from `dispatch` (a builtin), which has no
/// handle to the running `LineEditor` at all.
static HIGHLIGHT_ENABLED: AtomicBool = AtomicBool::new(true);

pub fn set_highlight_enabled(on: bool) {
    HIGHLIGHT_ENABLED.store(on, Ordering::Relaxed);
}

pub fn highlight_enabled() -> bool {
    HIGHLIGHT_ENABLED.load(Ordering::Relaxed)
}

/// Colors a line for display only, via its own tiny linear scan over the
/// raw characters — deliberately *not* built on `Interpreter::tokenize`,
/// which strips quote characters and exact spacing when it produces
/// `Token`s (fine for expansion, but it would make faithfully redrawing
/// the line byte-for-byte impossible). This scan never errors and always
/// preserves every input character exactly, so it degrades gracefully on
/// syntax that's incomplete because the user simply hasn't finished typing
/// it yet (e.g. an unclosed quote just consumes to end of buffer).
fn highlight(line: &str) -> String {
    let mut out = String::with_capacity(line.len() * 2);
    let mut chars = line.chars().peekable();
    let mut seen_word = false;

    while let Some(&c) = chars.peek() {
        if c == '#' {
            let rest: String = chars.by_ref().collect();
            out.push_str(&format!("{}", rest.dark_grey()));
            break;
        }
        if c.is_whitespace() {
            out.push(c);
            chars.next();
            continue;
        }
        if c == '\'' || c == '"' {
            let quote = c;
            let mut buf = String::new();
            buf.push(chars.next().unwrap());
            for ch in chars.by_ref() {
                buf.push(ch);
                if ch == quote {
                    break;
                }
            }
            out.push_str(&format!("{}", buf.green()));
            seen_word = true;
            continue;
        }
        if c == '$' || c == '@' {
            let mut buf = String::new();
            buf.push(chars.next().unwrap());
            while let Some(&ch) = chars.peek() {
                if ch.is_alphanumeric() || matches!(ch, '_' | '{' | '}' | '(' | ')') {
                    buf.push(ch);
                    chars.next();
                } else {
                    break;
                }
            }
            out.push_str(&format!("{}", buf.cyan()));
            seen_word = true;
            continue;
        }
        let mut buf = String::new();
        while let Some(&ch) = chars.peek() {
            if ch.is_whitespace() || matches!(ch, '#' | '\'' | '"' | '$' | '@') {
                break;
            }
            buf.push(ch);
            chars.next();
        }
        if !seen_word {
            if crate::builtin_names::is_keyword(&buf) {
                out.push_str(&format!("{}", buf.magenta()));
            } else {
                out.push_str(&format!("{}", buf.blue()));
            }
            seen_word = true;
        } else {
            out.push_str(&buf);
        }
    }
    out
}

/// Outcome of reading one line. `Aborted` only ever comes back from
/// `read_continuation_line` (Esc pressed on an already-empty continuation
/// line, i.e. a second Esc press) — plain `read_line` never produces it,
/// since top-level prompts don't have a multi-line block to cancel.
pub enum EditorOutcome {
    Line(String),
    Eof,
    Aborted,
}

pub struct LineEditor {
    history: Vec<String>,
}

impl LineEditor {
    pub fn new() -> Self {
        LineEditor {
            history: Vec::new(),
        }
    }

    /// Creates an editor pre-seeded with history loaded from a previous
    /// session (via `history::load`), so Up-arrow recall includes it
    /// immediately, not just newly-typed lines.
    pub fn with_history(initial: Vec<String>) -> Self {
        LineEditor { history: initial }
    }

    /// The full accumulated history (loaded entries plus everything typed
    /// this session), for `history::save` to persist at shell exit.
    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// Reads one line, using raw-mode editing if stdin is a terminal and
    /// falling back to plain buffered reads otherwise. Returns `None` on
    /// EOF.
    pub fn read_line(&mut self, prompt: &str) -> Option<String> {
        match self.read_line_outcome(prompt, false) {
            EditorOutcome::Line(line) => Some(line),
            EditorOutcome::Eof => None,
            EditorOutcome::Aborted => {
                unreachable!("Esc-abort only triggers when allow_abort is true")
            }
        }
    }

    /// Like `read_line`, but for a block's continuation lines: a *second*
    /// Esc press (i.e. one more Esc once the current continuation line is
    /// already empty) returns `Aborted` instead of just clearing the line
    /// again, letting `read_block_body` cancel the whole in-progress
    /// multi-line block rather than only the partial line being typed.
    pub fn read_continuation_line(&mut self, prompt: &str) -> EditorOutcome {
        self.read_line_outcome(prompt, true)
    }

    fn read_line_outcome(&mut self, prompt: &str, allow_abort: bool) -> EditorOutcome {
        if io::stdin().is_terminal() {
            self.read_line_interactive(prompt, allow_abort)
        } else {
            match read_line_plain(prompt) {
                Some(line) => EditorOutcome::Line(line),
                None => EditorOutcome::Eof,
            }
        }
    }

    /// Records a non-empty line in history, skipping consecutive exact
    /// duplicates (a common shell convenience).
    pub fn add_history(&mut self, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        if self.history.last().map(String::as_str) != Some(line) {
            self.history.push(line.to_string());
        }
    }

    /// Removes the most recently recorded entry when it is the input that
    /// just ran. Used by `HISTORY_IGNORE=no_such_command` after execution
    /// reveals that a command could not be resolved.
    pub fn remove_last_history_if(&mut self, line: &str) {
        if self.history.last().map(String::as_str) == Some(line) {
            self.history.pop();
        }
    }

    fn read_line_interactive(&mut self, prompt: &str, allow_abort: bool) -> EditorOutcome {
        if enable_raw_mode().is_err() {
            return match read_line_plain(prompt) {
                Some(line) => EditorOutcome::Line(line),
                None => EditorOutcome::Eof,
            };
        }
        let result = self.run_editor(prompt, allow_abort);
        let _ = disable_raw_mode();
        // Raw mode suppresses normal newline echo; move to a fresh line
        // ourselves before the caller prints anything further.
        print!("\r\n");
        let _ = io::stdout().flush();
        result
    }

    fn run_editor(&mut self, prompt: &str, allow_abort: bool) -> EditorOutcome {
        let mut buffer: Vec<char> = Vec::new();
        let mut cursor_pos = 0usize;
        let mut selection_anchor = None;
        let mut history_index = self.history.len(); // == "not browsing history"
        let mut saved_current = String::new();

        redraw(prompt, &buffer, cursor_pos, selection_anchor);

        loop {
            let event = match event::read() {
                Ok(e) => e,
                Err(_) => return EditorOutcome::Eof,
            };

            let Event::Key(KeyEvent {
                code,
                modifiers,
                kind,
                ..
            }) = event
            else {
                continue; // ignore resize/mouse/focus/paste events
            };
            // Windows reports both press and release; only act once per key.
            if kind == KeyEventKind::Release {
                continue;
            }

            match (code, modifiers) {
                (KeyCode::Enter, _) => {
                    return EditorOutcome::Line(buffer.into_iter().collect());
                }
                (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                    print!("^C\r\n");
                    let _ = io::stdout().flush();
                    buffer.clear();
                    cursor_pos = 0;
                    selection_anchor = None;
                    history_index = self.history.len();
                    redraw(prompt, &buffer, cursor_pos, selection_anchor);
                }
                (KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => {
                    if buffer.is_empty() {
                        return EditorOutcome::Eof;
                    }
                    if !delete_selection(&mut buffer, &mut cursor_pos, &mut selection_anchor)
                        && cursor_pos < buffer.len()
                    {
                        buffer.remove(cursor_pos);
                    }
                }
                (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => {
                    if !delete_selection(&mut buffer, &mut cursor_pos, &mut selection_anchor) {
                        buffer.drain(0..cursor_pos);
                        cursor_pos = 0;
                    }
                }
                (KeyCode::Char('k'), m) if m.contains(KeyModifiers::CONTROL) => {
                    // Kill to end of line, complementing Ctrl+U's kill to
                    // start. No kill-ring/yank — matches Ctrl+U's existing
                    // plain-delete behavior rather than adding new state.
                    if !delete_selection(&mut buffer, &mut cursor_pos, &mut selection_anchor) {
                        buffer.truncate(cursor_pos);
                    }
                }
                (KeyCode::Char('w'), m) if m.contains(KeyModifiers::CONTROL) => {
                    if !delete_selection(&mut buffer, &mut cursor_pos, &mut selection_anchor) {
                        let start = prev_word_boundary(&buffer, cursor_pos);
                        buffer.drain(start..cursor_pos);
                        cursor_pos = start;
                    }
                }
                (KeyCode::Esc, _) => {
                    if selection_anchor.take().is_some() {
                        redraw(prompt, &buffer, cursor_pos, selection_anchor);
                        continue;
                    }
                    // On a continuation line (typing a `while`/`if`/`for`/
                    // `fn` block's body), pressing Esc again once the
                    // current line is already empty cancels the whole
                    // block instead of just clearing this one line —
                    // otherwise there'd be no way to back out of a
                    // multi-line block short of typing `end` and letting
                    // it run.
                    if allow_abort && buffer.is_empty() {
                        print!("^[\r\n");
                        let _ = io::stdout().flush();
                        return EditorOutcome::Aborted;
                    }
                    buffer.clear();
                    cursor_pos = 0;
                    selection_anchor = None;
                    history_index = self.history.len();
                }
                (KeyCode::Tab, _) => {
                    delete_selection(&mut buffer, &mut cursor_pos, &mut selection_anchor);
                    if let Some((new_buffer, new_cursor_pos)) = complete(&buffer, cursor_pos) {
                        buffer = new_buffer;
                        cursor_pos = new_cursor_pos;
                    }
                }
                (KeyCode::Backspace, m) if m.contains(KeyModifiers::ALT) => {
                    if !delete_selection(&mut buffer, &mut cursor_pos, &mut selection_anchor) {
                        let start = prev_word_boundary(&buffer, cursor_pos);
                        buffer.drain(start..cursor_pos);
                        cursor_pos = start;
                    }
                }
                (KeyCode::Backspace, _) => {
                    if !delete_selection(&mut buffer, &mut cursor_pos, &mut selection_anchor)
                        && cursor_pos > 0
                    {
                        buffer.remove(cursor_pos - 1);
                        cursor_pos -= 1;
                    }
                }
                (KeyCode::Delete, _) => {
                    if !delete_selection(&mut buffer, &mut cursor_pos, &mut selection_anchor)
                        && cursor_pos < buffer.len()
                    {
                        buffer.remove(cursor_pos);
                    }
                }
                (KeyCode::Left, m)
                    if m.contains(KeyModifiers::CONTROL) && m.contains(KeyModifiers::SHIFT) =>
                {
                    let destination = prev_word_boundary(&buffer, cursor_pos);
                    extend_selection(&mut selection_anchor, &mut cursor_pos, destination);
                }
                (KeyCode::Right, m)
                    if m.contains(KeyModifiers::CONTROL) && m.contains(KeyModifiers::SHIFT) =>
                {
                    let destination = next_word_boundary(&buffer, cursor_pos);
                    extend_selection(&mut selection_anchor, &mut cursor_pos, destination);
                }
                (KeyCode::Left, m) if m.contains(KeyModifiers::CONTROL) => {
                    selection_anchor = None;
                    cursor_pos = prev_word_boundary(&buffer, cursor_pos);
                }
                (KeyCode::Right, m) if m.contains(KeyModifiers::CONTROL) => {
                    selection_anchor = None;
                    cursor_pos = next_word_boundary(&buffer, cursor_pos);
                }
                (KeyCode::Left, m) if m.contains(KeyModifiers::SHIFT) => {
                    let destination = cursor_pos.saturating_sub(1);
                    extend_selection(&mut selection_anchor, &mut cursor_pos, destination);
                }
                (KeyCode::Right, m) if m.contains(KeyModifiers::SHIFT) => {
                    let destination = (cursor_pos + 1).min(buffer.len());
                    extend_selection(&mut selection_anchor, &mut cursor_pos, destination);
                }
                (KeyCode::Home, m) if m.contains(KeyModifiers::SHIFT) => {
                    extend_selection(&mut selection_anchor, &mut cursor_pos, 0);
                }
                (KeyCode::End, m) if m.contains(KeyModifiers::SHIFT) => {
                    extend_selection(&mut selection_anchor, &mut cursor_pos, buffer.len());
                }
                (KeyCode::Left, _) => {
                    selection_anchor = None;
                    cursor_pos = cursor_pos.saturating_sub(1);
                }
                (KeyCode::Right, _) => {
                    selection_anchor = None;
                    cursor_pos = (cursor_pos + 1).min(buffer.len());
                }
                (KeyCode::Home, _) => {
                    selection_anchor = None;
                    cursor_pos = 0;
                }
                (KeyCode::End, _) => {
                    selection_anchor = None;
                    cursor_pos = buffer.len();
                }
                (KeyCode::Up, _) => {
                    if history_index > 0 {
                        if history_index == self.history.len() {
                            saved_current = buffer.iter().collect();
                        }
                        history_index -= 1;
                        buffer = self.history[history_index].chars().collect();
                        cursor_pos = buffer.len();
                        selection_anchor = None;
                    }
                }
                (KeyCode::Down, _) => {
                    if history_index < self.history.len() {
                        history_index += 1;
                        buffer = if history_index == self.history.len() {
                            saved_current.chars().collect()
                        } else {
                            self.history[history_index].chars().collect()
                        };
                        cursor_pos = buffer.len();
                        selection_anchor = None;
                    }
                }
                (KeyCode::Char(ch), m)
                    if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
                {
                    delete_selection(&mut buffer, &mut cursor_pos, &mut selection_anchor);
                    buffer.insert(cursor_pos, ch);
                    cursor_pos += 1;
                }
                _ => continue, // unhandled key: don't redraw needlessly
            }

            redraw(prompt, &buffer, cursor_pos, selection_anchor);
        }
    }
}

impl Default for LineEditor {
    fn default() -> Self {
        Self::new()
    }
}

/// Index of the start of the word immediately before `pos`: skips any
/// whitespace directly to the left of the cursor, then skips back through
/// the word itself. Used by Ctrl+W/Alt+Backspace (delete word backward)
/// and Ctrl+Left (jump word backward).
fn prev_word_boundary(buffer: &[char], pos: usize) -> usize {
    let mut i = pos;
    while i > 0 && buffer[i - 1].is_whitespace() {
        i -= 1;
    }
    while i > 0 && !buffer[i - 1].is_whitespace() {
        i -= 1;
    }
    i
}

/// Index of the start of the next word after `pos`: skips the rest of the
/// current word (if `pos` is inside one), then any whitespace, landing on
/// the first character of the next word (or end of buffer). Used by
/// Ctrl+Right (jump word forward).
fn next_word_boundary(buffer: &[char], pos: usize) -> usize {
    let mut i = pos;
    let len = buffer.len();
    while i < len && !buffer[i].is_whitespace() {
        i += 1;
    }
    while i < len && buffer[i].is_whitespace() {
        i += 1;
    }
    i
}

fn selection_range(anchor: Option<usize>, cursor_pos: usize) -> Option<(usize, usize)> {
    let anchor = anchor?;
    (anchor != cursor_pos).then(|| {
        if anchor < cursor_pos {
            (anchor, cursor_pos)
        } else {
            (cursor_pos, anchor)
        }
    })
}

fn extend_selection(anchor: &mut Option<usize>, cursor_pos: &mut usize, destination: usize) {
    let original = *cursor_pos;
    anchor.get_or_insert(original);
    *cursor_pos = destination;
    if *anchor == Some(destination) {
        *anchor = None;
    }
}

fn delete_selection(
    buffer: &mut Vec<char>,
    cursor_pos: &mut usize,
    anchor: &mut Option<usize>,
) -> bool {
    let Some((start, end)) = selection_range(*anchor, *cursor_pos) else {
        *anchor = None;
        return false;
    };
    buffer.drain(start..end);
    *cursor_pos = start;
    *anchor = None;
    true
}

/// Adds reverse-video SGR around a visible character range while preserving
/// syntax colors. Syntax spans use reset codes, so reverse video is re-applied
/// after every SGR sequence inside the selected range.
fn apply_selection_highlight(rendered: &str, range: (usize, usize)) -> String {
    let (start, end) = range;
    let mut out = String::with_capacity(rendered.len() + 32);
    let mut visible = 0usize;
    let mut chars = rendered.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            out.push(ch);
            while let Some(code) = chars.next() {
                out.push(code);
                if code == 'm' {
                    if visible >= start && visible < end {
                        out.push_str("\u{1b}[7m");
                    }
                    break;
                }
            }
            continue;
        }
        if visible == start {
            out.push_str("\u{1b}[7m");
        }
        if visible == end {
            out.push_str("\u{1b}[27m");
        }
        out.push(ch);
        visible += 1;
    }
    if visible == end {
        out.push_str("\u{1b}[27m");
    }
    out
}

/// Repaints the current line: clears it, reprints `prompt` + buffer
/// contents, then positions the terminal cursor at `cursor_pos` within the
/// buffer (not byte position — `char` position, consistent with the
/// buffer's own indexing).
fn redraw(prompt: &str, buffer: &[char], cursor_pos: usize, selection_anchor: Option<usize>) {
    let mut stdout = io::stdout();
    let _ = execute!(
        stdout,
        cursor::MoveToColumn(0),
        terminal::Clear(terminal::ClearType::CurrentLine)
    );
    let line: String = buffer.iter().collect();
    let mut rendered = if highlight_enabled() {
        highlight(&line)
    } else {
        line
    };
    if let Some(range) = selection_range(selection_anchor, cursor_pos) {
        rendered = apply_selection_highlight(&rendered, range);
    }
    print!("{prompt}{rendered}");
    let _ = stdout.flush();
    // Color escape sequences are zero-width, so the cursor's target column
    // is still just prompt length + character offset into the buffer,
    // regardless of whether `rendered` above is colored or plain.
    let target_col = (prompt.chars().count() + cursor_pos) as u16;
    let _ = execute!(stdout, cursor::MoveToColumn(target_col));
}

fn read_line_plain(prompt: &str) -> Option<String> {
    print!("{prompt}");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
        return None;
    }
    Some(line.trim_end().to_string())
}

fn complete(buffer: &[char], cursor_pos: usize) -> Option<(Vec<char>, usize)> {
    let line: String = buffer.iter().collect();
    let before_cursor: String = buffer[..cursor_pos].iter().collect();
    let token_start = before_cursor
        .rfind(char::is_whitespace)
        .map(|i| i + 1)
        .unwrap_or(0);
    let token = &before_cursor[token_start..];
    if token.is_empty() {
        return None;
    }

    let first_token = before_cursor[..token_start].trim().is_empty();
    let completion = if first_token && !looks_like_path(token) {
        complete_command(token)
    } else {
        complete_path(token)
    }?;

    if completion == token {
        return None;
    }

    let new_line = format!(
        "{}{}{}",
        &line[..token_start],
        completion,
        &line[cursor_pos..]
    );
    let new_cursor_pos = token_start + completion.chars().count();
    Some((new_line.chars().collect(), new_cursor_pos))
}

fn complete_command(prefix: &str) -> Option<String> {
    let mut matches: Vec<String> = crate::builtin_names::names()
        .filter(|cmd| cmd.starts_with(prefix))
        .map(str::to_string)
        .collect();

    matches.extend(path_matches(prefix));
    choose_completion(prefix, matches)
}

fn complete_path(prefix: &str) -> Option<String> {
    choose_completion(prefix, path_matches(prefix))
}

fn path_matches(prefix: &str) -> Vec<String> {
    let expanded = expand_tilde(prefix);
    let path = Path::new(&expanded);
    let (dir, partial) = if prefix.ends_with('\\') || prefix.ends_with('/') {
        (PathBuf::from(&expanded), String::new())
    } else {
        (
            path.parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from(".")),
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        )
    };

    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    let typed_dir = typed_dir_prefix(prefix);
    let partial_lower = partial.to_lowercase();
    let mut matches = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.to_lowercase().starts_with(&partial_lower) {
            continue;
        }
        let suffix = if entry.path().is_dir() { "\\" } else { " " };
        matches.push(format!("{typed_dir}{name}{suffix}"));
    }
    matches.sort_by_key(|name| name.to_lowercase());
    matches
}

fn choose_completion(prefix: &str, matches: Vec<String>) -> Option<String> {
    match matches.as_slice() {
        [] => None,
        [only] => Some(only.clone()),
        _ => common_prefix(&matches).filter(|common| common.len() > prefix.len()),
    }
}

fn common_prefix(items: &[String]) -> Option<String> {
    let first = items.first()?;
    let mut end = first.len();
    for item in &items[1..] {
        while end > 0
            && !item
                .get(..end)
                .is_some_and(|s| first[..end].eq_ignore_ascii_case(s))
        {
            end = previous_char_boundary(first, end);
        }
    }
    Some(first[..end].to_string())
}

fn previous_char_boundary(s: &str, end: usize) -> usize {
    s[..end].char_indices().last().map(|(i, _)| i).unwrap_or(0)
}

fn looks_like_path(token: &str) -> bool {
    token.contains('\\')
        || token.contains('/')
        || token.starts_with('.')
        || token.starts_with('~')
        || token.contains(':')
}

fn typed_dir_prefix(prefix: &str) -> String {
    match prefix.rfind(['\\', '/']) {
        Some(i) => prefix[..=i].to_string(),
        None => String::new(),
    }
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix('~') {
        if let Ok(home) = std::env::var("USERPROFILE") {
            return format!("{home}{rest}");
        }
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prev_word_boundary_skips_trailing_space_then_the_word() {
        let buf: Vec<char> = "echo hello world".chars().collect();
        assert_eq!(prev_word_boundary(&buf, buf.len()), 11); // start of "world"
        assert_eq!(prev_word_boundary(&buf, 5), 0); // start of "hello" -> start of "echo"
        assert_eq!(prev_word_boundary(&buf, 0), 0); // already at start
    }

    #[test]
    fn prev_word_boundary_handles_cursor_mid_word() {
        let buf: Vec<char> = "hello world".chars().collect();
        assert_eq!(prev_word_boundary(&buf, 8), 6); // mid "world" -> start of "world"
    }

    #[test]
    fn next_word_boundary_skips_word_then_space() {
        let buf: Vec<char> = "echo hello world".chars().collect();
        assert_eq!(next_word_boundary(&buf, 0), 5); // start of "echo" -> start of "hello"
        assert_eq!(next_word_boundary(&buf, 5), 11); // start of "hello" -> start of "world"
        assert_eq!(next_word_boundary(&buf, 11), buf.len()); // last word -> end of buffer
    }

    #[test]
    fn selection_range_normalizes_both_directions_and_ignores_empty() {
        assert_eq!(selection_range(Some(2), 5), Some((2, 5)));
        assert_eq!(selection_range(Some(5), 2), Some((2, 5)));
        assert_eq!(selection_range(Some(2), 2), None);
        assert_eq!(selection_range(None, 2), None);
    }

    #[test]
    fn deleting_selection_replaces_the_whole_selected_range() {
        let mut buffer: Vec<char> = "echo hello".chars().collect();
        let mut cursor = 10;
        let mut anchor = Some(5);
        assert!(delete_selection(&mut buffer, &mut cursor, &mut anchor));
        assert_eq!(buffer.into_iter().collect::<String>(), "echo ");
        assert_eq!(cursor, 5);
        assert_eq!(anchor, None);
    }

    #[test]
    fn selection_highlight_preserves_text_and_ansi_syntax_colors() {
        let rendered = highlight("echo \"$name\"");
        let selected = apply_selection_highlight(&rendered, (5, 12));
        assert_eq!(strip_ansi(&selected), "echo \"$name\"");
        assert!(selected.contains("\u{1b}[7m"));
        assert!(selected.contains("\u{1b}[27m"));
    }

    /// Strips ANSI SGR escape sequences (`ESC '[' ... 'm'`) so a colored
    /// string can be compared against the plain original — the invariant
    /// `highlight` must never break, since `redraw`'s cursor-column math
    /// depends on every original character surviving unchanged.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' && chars.clone().next() == Some('[') {
                chars.next(); // consume '['
                for ch in chars.by_ref() {
                    if ch == 'm' {
                        break;
                    }
                }
                continue;
            }
            out.push(c);
        }
        out
    }

    #[test]
    fn highlight_preserves_every_character_including_incomplete_syntax() {
        for line in [
            "echo \"hello world\" $x @arr # a comment",
            "while true",
            "echo 'unclosed",
            "",
            "   ",
            "if $x -eq 1",
        ] {
            assert_eq!(strip_ansi(&highlight(line)), line, "line: {line:?}");
        }
    }

    #[test]
    fn highlight_toggle_defaults_on_and_roundtrips() {
        assert!(highlight_enabled(), "should default to on");
        set_highlight_enabled(false);
        assert!(!highlight_enabled());
        set_highlight_enabled(true);
        assert!(highlight_enabled());
    }

    #[test]
    fn add_history_skips_empty_and_consecutive_duplicates() {
        let mut editor = LineEditor::new();
        editor.add_history("");
        editor.add_history("   ");
        editor.add_history("echo hi");
        editor.add_history("echo hi");
        editor.add_history("echo bye");
        assert_eq!(
            editor.history,
            vec!["echo hi".to_string(), "echo bye".to_string()]
        );
    }

    #[test]
    fn add_history_allows_non_consecutive_repeats() {
        let mut editor = LineEditor::new();
        editor.add_history("a");
        editor.add_history("b");
        editor.add_history("a");
        assert_eq!(
            editor.history,
            vec!["a".to_string(), "b".to_string(), "a".to_string()]
        );
    }

    #[test]
    fn remove_last_history_if_only_removes_the_matching_latest_entry() {
        let mut editor = LineEditor::default();
        editor.add_history("echo kept");
        editor.add_history("missing-command");
        editor.remove_last_history_if("different-command");
        assert_eq!(editor.history, vec!["echo kept", "missing-command"]);
        editor.remove_last_history_if("missing-command");
        assert_eq!(editor.history, vec!["echo kept"]);
    }

    #[test]
    fn tab_completes_unique_builtin() {
        let input: Vec<char> = "pw".chars().collect();
        let (completed, cursor) = complete(&input, input.len()).unwrap();
        assert_eq!(completed.into_iter().collect::<String>(), "pwd");
        assert_eq!(cursor, 3);
    }

    #[test]
    fn tab_leaves_ambiguous_builtin_prefix_alone() {
        let input: Vec<char> = "ex".chars().collect();
        assert!(complete(&input, input.len()).is_none());
    }
}
