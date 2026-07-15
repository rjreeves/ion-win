//! Colors the shell's own error/usage messages (the `ion:`/`ion-win:`
//! prefixed lines) red when printed to a real terminal. Never touches
//! ordinary command output (`echo`, external process stdout, etc.) — only
//! the messages that already went through `err_println!`/`err_eprintln!`
//! at their call sites.
//!
//! Deliberately conservative about *when* to color: only if the relevant
//! stream (stdout or stderr, checked separately since a script can redirect
//! one without the other) is a real terminal, and the `NO_COLOR` env var
//! (https://no-color.org) isn't set. This keeps piped/redirected/scripted
//! output byte-for-byte clean — no stray escape codes leaking into a file
//! or the next pipeline stage.

use crossterm::style::Stylize;
use std::io::IsTerminal;

/// The actual color decision, isolated from the `IsTerminal`/env-var I/O so
/// it can be unit tested without a real terminal.
fn wants_color(is_tty: bool, no_color_set: bool) -> bool {
    is_tty && !no_color_set
}

fn colors_enabled_stdout() -> bool {
    wants_color(
        std::io::stdout().is_terminal(),
        std::env::var_os("NO_COLOR").is_some(),
    )
}

fn colors_enabled_stderr() -> bool {
    wants_color(
        std::io::stderr().is_terminal(),
        std::env::var_os("NO_COLOR").is_some(),
    )
}

pub fn print_err(msg: &str) {
    if colors_enabled_stdout() {
        println!("{}", msg.red());
    } else {
        println!("{msg}");
    }
}

pub fn eprint_err(msg: &str) {
    if colors_enabled_stderr() {
        eprintln!("{}", msg.red());
    } else {
        eprintln!("{msg}");
    }
}

/// Drop-in, red-on-terminal replacement for `println!` — use for error/
/// usage messages printed to stdout (matches how this codebase already
/// prints most builtin errors via `println!` rather than `eprintln!`).
#[macro_export]
macro_rules! err_println {
    ($($arg:tt)*) => {
        $crate::colorout::print_err(&format!($($arg)*))
    };
}

/// Drop-in, red-on-terminal replacement for `eprintln!`.
#[macro_export]
macro_rules! err_eprintln {
    ($($arg:tt)*) => {
        $crate::colorout::eprint_err(&format!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::wants_color;

    #[test]
    fn only_colors_a_real_terminal_with_no_color_unset() {
        assert!(wants_color(true, false));
        assert!(!wants_color(false, false), "never color non-terminal output");
        assert!(!wants_color(true, true), "NO_COLOR must override a real terminal");
        assert!(!wants_color(false, true));
    }
}
