//! Keyboard input used by the `read` builtin.
//!
//! Ordinary line input stays in the terminal's normal cooked mode. Hidden
//! (`-s`) and fixed-character (`-n`) reads temporarily use crossterm raw mode
//! so the OS does not echo characters or wait for Enter. Non-terminal stdin
//! keeps a buffered fallback for scripts, pipes, and automated tests.

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::io::{self, IsTerminal, Write};

pub struct ReadRequest<'a> {
    pub prompt: &'a str,
    pub silent: bool,
    pub count: Option<usize>,
}

/// Prints the prompt and reads either a complete line or at most `count`
/// Unicode scalar values. `None` means EOF before any input.
pub fn read(request: ReadRequest<'_>) -> io::Result<Option<String>> {
    print!("{}", request.prompt);
    io::stdout().flush()?;

    if io::stdin().is_terminal() && (request.silent || request.count.is_some()) {
        read_raw(request)
    } else {
        read_buffered(request.count)
    }
}

fn read_buffered(count: Option<usize>) -> io::Result<Option<String>> {
    let mut line = String::new();
    if io::stdin().read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let line = line.trim_end_matches(['\r', '\n']);
    Ok(Some(match count {
        Some(limit) => line.chars().take(limit).collect(),
        None => line.to_string(),
    }))
}

fn read_raw(request: ReadRequest<'_>) -> io::Result<Option<String>> {
    enable_raw_mode()?;
    let result = read_raw_events(request.silent, request.count);
    let restore_result = disable_raw_mode();

    if result.as_ref().is_err_and(|error| error.kind() == io::ErrorKind::Interrupted) {
        print!("^C\r\n");
        let _ = io::stdout().flush();
    // Enter is not echoed in raw mode. Hidden line input should still leave
    // the following output on a fresh line; fixed-count input returns as soon
    // as the requested characters arrive and deliberately stays on that line.
    } else if request.count.is_none() {
        print!("\r\n");
        let _ = io::stdout().flush();
    }

    restore_result?;
    result
}

fn read_raw_events(silent: bool, count: Option<usize>) -> io::Result<Option<String>> {
    let mut input = String::new();
    loop {
        let Event::Key(KeyEvent {
            code,
            modifiers,
            kind,
            ..
        }) = event::read()?
        else {
            continue;
        };
        if kind == KeyEventKind::Release {
            continue;
        }

        match code {
            KeyCode::Enter => return Ok(Some(input)),
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "input interrupted",
                ));
            }
            KeyCode::Backspace if count.is_none() => {
                if input.pop().is_some() && !silent {
                    print!("\u{8} \u{8}");
                    io::stdout().flush()?;
                }
            }
            KeyCode::Char(ch)
                if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                input.push(ch);
                if !silent {
                    print!("{ch}");
                    io::stdout().flush()?;
                }
                if count.is_some_and(|limit| input.chars().count() >= limit) {
                    return Ok(Some(input));
                }
            }
            _ => {}
        }
    }
}

/// Splits a line across destination variables. The last variable captures
/// the unsplit remainder, retaining the original `read` behavior.
pub fn split_fields(line: &str, variable_count: usize) -> Vec<String> {
    if variable_count == 0 {
        return Vec::new();
    }
    if variable_count == 1 {
        return vec![line.to_string()];
    }
    line.splitn(variable_count, char::is_whitespace)
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_variable_receives_the_complete_line() {
        assert_eq!(split_fields("Robert James", 1), ["Robert James"]);
    }

    #[test]
    fn last_variable_receives_the_remainder() {
        assert_eq!(
            split_fields("one two three four", 3),
            ["one", "two", "three four"]
        );
    }

    #[test]
    fn missing_fields_can_be_filled_by_the_caller() {
        assert_eq!(split_fields("one", 3), ["one"]);
    }
}
