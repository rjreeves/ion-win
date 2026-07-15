//! `$(cmd)` / `@(cmd)` process expansion (ion-manual page 28): runs a
//! single external command and captures its stdout, either as a whole
//! string (`$`) or split by whitespace into words (`@`).
//!
//! Scope: only a single external command is supported inside the
//! parens — no pipes/redirection (that would need the async pipeline
//! engine; this expansion path is synchronous, matching how `interpolate`
//! itself is synchronous).

use std::process::Command;

/// Runs `args[0] args[1..]` and returns its captured stdout, with a
/// trailing newline trimmed (matching how POSIX shells strip the trailing
/// newline from command substitution).
pub fn capture(args: &[String]) -> Result<String, String> {
    let Some(program) = args.first() else {
        return Err("empty command in process expansion".to_string());
    };

    let output = Command::new(program)
        .args(&args[1..])
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                format!("command not found: {program}")
            } else {
                format!("failed to run '{program}': {e}")
            }
        })?;

    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    while text.ends_with('\n') || text.ends_with('\r') {
        text.pop();
    }
    Ok(text)
}
