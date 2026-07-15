//! Condition builtins used by `if`/`while` and directly at the prompt:
//! `test`, `matches`, `bool`, `contains`, `starts-with`, `ends-with`,
//! `eq`/`is`, `isatty`. Modeled on ion-manual pages 50-52, 56-57, 68-76,
//! 79, 81-83.
//!
//! Only the subset actually exercised by the manual's own examples is
//! implemented; unsupported forms print a diagnostic and evaluate to
//! `false` rather than panicking.

use crate::err_eprintln;
use regex::Regex;
use std::io::IsTerminal;

/// Evaluates a `test`-style expression. Mirrors the documented subset:
/// bare string truthiness, `-n`/`-z`, `-e`/`-f`/`-d` file checks,
/// `=`/`!=` string comparison, and `-eq`/`-ne`/`-lt`/`-le`/`-gt`/`-ge`
/// numeric comparison.
pub fn eval_test(args: &[String]) -> bool {
    match args {
        [] => false,
        [s] => !s.is_empty(),

        [flag, s] if flag == "-n" => !s.is_empty(),
        [flag, s] if flag == "-z" => s.is_empty(),
        [flag, path] if flag == "-e" => std::path::Path::new(path).exists(),
        [flag, path] if flag == "-f" => std::path::Path::new(path).is_file(),
        [flag, path] if flag == "-d" => std::path::Path::new(path).is_dir(),

        [a, op, b] => eval_binary(a, op, b),

        _ => {
            err_eprintln!("ion: test: unsupported expression: {}", args.join(" "));
            false
        }
    }
}

fn eval_binary(a: &str, op: &str, b: &str) -> bool {
    match op {
        // ion-manual page 20's own "Scopes" worked example uses `==`
        // (`if test 1 == 1`) alongside `=` elsewhere in the manual — both
        // accepted as the same string-equality operator.
        "=" | "==" => a == b,
        "!=" => a != b,
        "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge" => match (a.parse::<f64>(), b.parse::<f64>())
        {
            (Ok(x), Ok(y)) => match op {
                "-eq" => x == y,
                "-ne" => x != y,
                "-lt" => x < y,
                "-le" => x <= y,
                "-gt" => x > y,
                "-ge" => x >= y,
                _ => unreachable!(),
            },
            _ => {
                err_eprintln!("ion: test: '{a}' or '{b}' is not a number");
                false
            }
        },
        _ => {
            err_eprintln!("ion: test: unsupported operator: {op}");
            false
        }
    }
}

/// `bool VALUE` (ion-manual page 68: "Returns true if the value given to it
/// is equal to '1' or 'true'.").
pub fn eval_bool(args: &[String]) -> bool {
    match args {
        [value] => value == "1" || value == "true",
        _ => {
            err_eprintln!("ion: bool: usage: bool VALUE");
            false
        }
    }
}

/// `contains <PATTERN> tests...` (ion-manual page 69): exit status 0 if
/// the first argument contains any of the remaining ones, else 1.
pub fn eval_contains(args: &[String]) -> bool {
    match args {
        [s, rest @ ..] if !rest.is_empty() => rest.iter().any(|v| s.contains(v.as_str())),
        _ => {
            err_eprintln!("ion: contains: usage: contains <PATTERN> tests...");
            false
        }
    }
}

/// `starts-with <PATTERN> tests...` (ion-manual page 79): exit status 0 if
/// the first argument starts with any of the remaining ones, else 1.
pub fn eval_starts_with(args: &[String]) -> bool {
    match args {
        [s, rest @ ..] if !rest.is_empty() => rest.iter().any(|v| s.starts_with(v.as_str())),
        _ => {
            err_eprintln!("ion: starts-with: usage: starts-with <PATTERN> tests...");
            false
        }
    }
}

/// `ends-with <PATTERN> tests...` (ion-manual page 71): exit status 0 if
/// the first argument ends with any of the remaining ones, else 1.
pub fn eval_ends_with(args: &[String]) -> bool {
    match args {
        [s, rest @ ..] if !rest.is_empty() => rest.iter().any(|v| s.ends_with(v.as_str())),
        _ => {
            err_eprintln!("ion: ends-with: usage: ends-with <PATTERN> tests...");
            false
        }
    }
}

/// `eq`/`is [not] VALUE VALUE` (ion-manual page 75): exit status 0 if the
/// two values are equal; with the leading `not` option, exit status 0 if
/// they're NOT equal.
pub fn eval_eq(args: &[String]) -> bool {
    match args {
        [a, b] => a == b,
        [flag, a, b] if flag == "not" => a != b,
        _ => {
            err_eprintln!("ion: is: usage: is [not] VALUE VALUE");
            false
        }
    }
}

/// `isatty [FD]` (ion-manual page 75): exit status 0 if the given file
/// descriptor is a real terminal, 1 otherwise. Matches upstream Ion's
/// actual behavior exactly (confirmed against `Linux/ion-master/src/lib/
/// builtins/mod.rs`, not just the manual's synopsis): with *no* argument
/// it always succeeds unconditionally — upstream doesn't default to
/// checking any particular descriptor — only an explicit `FD` number
/// triggers a real check. Windows has no portable way to check an
/// arbitrary raw file descriptor's tty-ness the way POSIX's `isatty(3)`
/// does, so only 0/1/2 (stdin/stdout/stderr) are supported; any other
/// number is reported as unsupported rather than guessed.
pub fn eval_isatty(args: &[String]) -> bool {
    match args {
        [] => true,
        [fd] => match fd.parse::<i32>() {
            Ok(0) => std::io::stdin().is_terminal(),
            Ok(1) => std::io::stdout().is_terminal(),
            Ok(2) => std::io::stderr().is_terminal(),
            Ok(n) => {
                err_eprintln!(
                    "ion-win: isatty: unsupported file descriptor {n} (only 0/1/2 are supported on Windows)"
                );
                false
            }
            Err(_) => {
                err_eprintln!("ion: isatty: given bad number");
                false
            }
        },
        _ => {
            err_eprintln!("ion: isatty: usage: isatty [FD]");
            false
        }
    }
}

/// `matches VALUE PATTERN` — regex-based boolean match (ion-manual page 50,
/// "a matches builtin that performs a regex-based boolean match").
pub fn eval_matches(args: &[String]) -> bool {
    match args {
        [value, pattern] => match Regex::new(pattern) {
            Ok(re) => re.is_match(value),
            Err(e) => {
                err_eprintln!("ion: matches: invalid pattern '{pattern}': {e}");
                false
            }
        },
        _ => {
            err_eprintln!("ion: matches: usage: matches VALUE PATTERN");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_string_equality() {
        assert!(eval_test(&["foo".into(), "=".into(), "foo".into()]));
        assert!(!eval_test(&["foo".into(), "=".into(), "bar".into()]));
    }

    #[test]
    fn test_double_equals_matches_manual_scopes_example() {
        // ion-manual page 20: `if test 1 == 1`
        assert!(eval_test(&["1".into(), "==".into(), "1".into()]));
        assert!(!eval_test(&["1".into(), "==".into(), "2".into()]));
    }

    #[test]
    fn test_numeric_comparison() {
        assert!(eval_test(&["1".into(), "-lt".into(), "6".into()]));
        assert!(!eval_test(&["6".into(), "-lt".into(), "6".into()]));
    }

    #[test]
    fn bool_matches_manual() {
        assert!(eval_bool(&["1".into()]));
        assert!(eval_bool(&["true".into()]));
        assert!(!eval_bool(&["0".into()]));
        assert!(!eval_bool(&["false".into()]));
        assert!(!eval_bool(&["yes".into()]));
    }

    #[test]
    fn contains_matches_manual() {
        assert!(eval_contains(&["hello world".into(), "world".into()]));
        assert!(eval_contains(&["hello world".into(), "xyz".into(), "world".into()]));
        assert!(!eval_contains(&["hello world".into(), "xyz".into()]));
    }

    #[test]
    fn starts_with_matches_manual() {
        assert!(eval_starts_with(&["hello world".into(), "hello".into()]));
        assert!(!eval_starts_with(&["hello world".into(), "world".into()]));
    }

    #[test]
    fn ends_with_matches_manual() {
        assert!(eval_ends_with(&["hello world".into(), "world".into()]));
        assert!(!eval_ends_with(&["hello world".into(), "hello".into()]));
    }

    #[test]
    fn eq_matches_manual() {
        assert!(eval_eq(&["foo".into(), "foo".into()]));
        assert!(!eval_eq(&["foo".into(), "bar".into()]));
        assert!(eval_eq(&["not".into(), "foo".into(), "bar".into()]));
        assert!(!eval_eq(&["not".into(), "foo".into(), "foo".into()]));
    }

    #[test]
    fn isatty_with_no_args_always_succeeds() {
        // Matches upstream Ion's real behavior exactly: bare `isatty`
        // succeeds unconditionally, regardless of whether stdin/stdout
        // actually are a terminal in the test harness (they aren't).
        assert!(eval_isatty(&[]));
    }

    #[test]
    fn isatty_rejects_unsupported_fd_and_bad_input() {
        assert!(!eval_isatty(&["7".into()]));
        assert!(!eval_isatty(&["not_a_number".into()]));
    }

    #[test]
    fn matches_regex() {
        assert!(eval_matches(&["xs".into(), "x".into()]));
        assert!(!eval_matches(&["x".into(), "xs".into()]));
        assert!(eval_matches(&["apple".into(), "[A-Ma-m]\\w+".into()]));
    }
}
