//! Persistent command history (ion-manual pages 84-87).
//!
//! History is shared live across concurrent ion-win windows. Accepted
//! commands are appended immediately under a cross-process lock, and each
//! prompt reloads the file so Up-arrow sees commands entered in other
//! windows. `HISTFILE`, `HISTFILE_ENABLED`,
//! `HISTORY_IGNORE`, and `HISTORY_TIMESTAMP` are seeded as ordinary ion
//! variables at startup (via `seed_defaults`) so `echo $HISTFILE` etc. work
//! immediately, and `let HISTORY_IGNORE = [ ... ]` naturally overrides them
//! for the rest of the session — there's no separate out-of-band config
//! struct.
//!
//! Implemented `HISTORY_IGNORE` rules: `all`, `whitespace`, `duplicates`,
//! `no_such_command`,
//! (per the manual's exact wording — *all* earlier occurrences of a
//! repeated command are dropped, keeping only the latest, not just
//! adjacent-consecutive dedup), and `regex:PATTERN`.
//!
//! NOT implemented:
//! - `HISTFILE_SIZE`/`HISTORY_SIZE` enforcement — per the manual, these are
//!   also "(Currently ignored)" in upstream Ion, so not enforcing them
//!   matches documented behavior rather than being a gap.

use crate::interp::Interpreter;
use crate::types::{validate, TypeTag};
use regex::Regex;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const HISTFILE_VAR: &str = "HISTFILE";
const HISTFILE_ENABLED_VAR: &str = "HISTFILE_ENABLED";
const HISTORY_IGNORE_VAR: &str = "HISTORY_IGNORE";
const HISTORY_TIMESTAMP_VAR: &str = "HISTORY_TIMESTAMP";

/// Seeds `HISTFILE`/`HISTFILE_ENABLED`/`HISTORY_IGNORE`/`HISTORY_TIMESTAMP`
/// as real ion variables with their documented defaults, so they're
/// visible/overridable like any other variable rather than hidden config.
pub fn seed_defaults(interp: &mut Interpreter) {
    if interp.get_scalar(HISTFILE_VAR).is_none() {
        interp.set_scalar(
            HISTFILE_VAR.to_string(),
            default_histfile_path().to_string_lossy().into_owned(),
        );
    }
    if interp.get_scalar(HISTFILE_ENABLED_VAR).is_none() {
        interp.set_scalar(HISTFILE_ENABLED_VAR.to_string(), "1".to_string());
    }
    if interp.get_array(HISTORY_IGNORE_VAR).is_none() {
        interp.set_array(HISTORY_IGNORE_VAR.to_string(), default_ignore_rules());
    }
    if interp.get_scalar(HISTORY_TIMESTAMP_VAR).is_none() {
        interp.set_scalar(HISTORY_TIMESTAMP_VAR.to_string(), "0".to_string());
    }
}

fn default_ignore_rules() -> Vec<String> {
    vec![
        "no_such_command".to_string(),
        "whitespace".to_string(),
        "duplicates".to_string(),
    ]
}

/// `%APPDATA%\ion-win\history` on Windows, matching `state.rs`'s
/// `%APPDATA%\ion-win\state.redb` convention; falls back to a relative
/// path for non-Windows local development.
fn default_histfile_path() -> PathBuf {
    if let Ok(appdata) = std::env::var("APPDATA") {
        return PathBuf::from(appdata).join("ion-win").join("history");
    }
    PathBuf::from("ion-win-history")
}

fn histfile_path(interp: &Interpreter) -> PathBuf {
    match interp.get_scalar(HISTFILE_VAR) {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => default_histfile_path(),
    }
}

fn is_enabled(interp: &Interpreter) -> bool {
    match interp.get_scalar(HISTFILE_ENABLED_VAR) {
        Some(v) => validate(v, TypeTag::Bool)
            .map(|b| b == "true")
            .unwrap_or(true),
        None => true,
    }
}

fn is_timestamped(interp: &Interpreter) -> bool {
    match interp.get_scalar(HISTORY_TIMESTAMP_VAR) {
        Some(v) => validate(v, TypeTag::Bool)
            .map(|b| b == "true")
            .unwrap_or(false),
        None => false,
    }
}

#[cfg(windows)]
struct HistoryLock(isize);

#[cfg(windows)]
impl HistoryLock {
    fn acquire(path: &std::path::Path) -> Option<Self> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        use windows_sys::Win32::Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{CreateMutexW, WaitForSingleObject, INFINITE};

        let mut hasher = DefaultHasher::new();
        path.to_string_lossy().to_lowercase().hash(&mut hasher);
        let name = format!("Local\\ion-win-history-{:016x}", hasher.finish());
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
        if handle == 0 {
            return None;
        }
        let wait = unsafe { WaitForSingleObject(handle, INFINITE) };
        if wait == WAIT_OBJECT_0 || wait == WAIT_ABANDONED {
            Some(Self(handle))
        } else {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(handle);
            }
            None
        }
    }
}

#[cfg(windows)]
impl Drop for HistoryLock {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::System::Threading::ReleaseMutex(self.0);
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(not(windows))]
struct HistoryLock;

#[cfg(not(windows))]
impl HistoryLock {
    fn acquire(_path: &std::path::Path) -> Option<Self> {
        Some(Self)
    }
}

/// Whether the current rules request post-execution removal of inputs that
/// failed specifically because an executable could not be found.
pub fn ignores_no_such_command(interp: &Interpreter) -> bool {
    interp
        .get_array(HISTORY_IGNORE_VAR)
        .map(|rules| rules.iter().any(|r| r == "no_such_command"))
        .unwrap_or_else(|| {
            default_ignore_rules()
                .iter()
                .any(|r| r == "no_such_command")
        })
}

/// A `#<digits>` line is a timestamp marker, not a command — skip it when
/// loading (ion-manual page 87: "The timestamp is indicated with a #").
fn is_timestamp_marker(line: &str) -> bool {
    line.strip_prefix('#')
        .map(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
        .unwrap_or(false)
}

/// Loads history entries from `HISTFILE`, or an empty list if disabled,
/// unset, or unreadable (first run).
pub fn load(interp: &Interpreter) -> Vec<String> {
    if !is_enabled(interp) {
        return Vec::new();
    }
    let path = histfile_path(interp);
    let Some(_lock) = HistoryLock::acquire(&path) else {
        return Vec::new();
    };
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let entries: Vec<String> = contents
        .lines()
        .filter(|l| !is_timestamp_marker(l))
        .map(str::to_string)
        .collect();
    let rules = interp
        .get_array(HISTORY_IGNORE_VAR)
        .cloned()
        .unwrap_or_else(default_ignore_rules);
    apply_ignore_rules(&entries, &rules)
}

/// Returns a fresh shared-history snapshot while persistence is enabled.
/// `None` means the live `HISTFILE_ENABLED` setting is off, so callers
/// should retain their in-memory session history.
pub fn refresh(interp: &Interpreter) -> Option<Vec<String>> {
    is_enabled(interp).then(|| load(interp))
}

/// Appends newly accepted commands as one locked record batch. Filtering is
/// applied before writing; duplicate cleanup is applied when snapshots load,
/// avoiding a destructive whole-file rewrite while other windows are active.
pub fn append(interp: &Interpreter, entries: &[String]) {
    if !is_enabled(interp) || entries.is_empty() {
        return;
    }
    let rules = interp
        .get_array(HISTORY_IGNORE_VAR)
        .cloned()
        .unwrap_or_else(default_ignore_rules);
    let filtered = apply_ignore_rules(entries, &rules);
    if filtered.is_empty() {
        return;
    }

    let path = histfile_path(interp);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Some(_lock) = HistoryLock::acquire(&path) else {
        return;
    };
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let timestamped = is_timestamped(interp);
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut out = String::new();
    for line in filtered {
        if timestamped {
            out.push_str(&format!("#{epoch}\n"));
        }
        out.push_str(&line);
        out.push('\n');
    }
    let _ = file.write_all(out.as_bytes());
    let _ = file.flush();
}

/// Applies `HISTORY_IGNORE` rules to `entries`, in the order the manual
/// documents them. `duplicates` drops *all* earlier occurrences of a
/// command each time it's seen again, keeping only the final, most-recent
/// occurrence (not simple adjacent dedup).
fn apply_ignore_rules(entries: &[String], rules: &[String]) -> Vec<String> {
    if rules.iter().any(|r| r == "all") {
        return Vec::new();
    }

    let skip_whitespace = rules.iter().any(|r| r == "whitespace");
    let dedup = rules.iter().any(|r| r == "duplicates");
    let regexes: Vec<Regex> = rules
        .iter()
        .filter_map(|r| r.strip_prefix("regex:"))
        .filter_map(|pattern| Regex::new(pattern).ok())
        .collect();

    let mut out: Vec<String> = Vec::with_capacity(entries.len());
    for entry in entries {
        if skip_whitespace && entry.starts_with(char::is_whitespace) {
            continue;
        }
        if regexes.iter().any(|re| re.is_match(entry)) {
            continue;
        }
        if dedup {
            out.retain(|kept| kept != entry);
        }
        out.push(entry.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn temp_history(name: &str) -> (Interpreter, PathBuf) {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "ion-win-history-{name}-{}-{unique}",
            std::process::id()
        ));
        let mut interp = Interpreter::new();
        seed_defaults(&mut interp);
        interp.set_scalar(
            HISTFILE_VAR.to_string(),
            path.to_string_lossy().into_owned(),
        );
        (interp, path)
    }

    #[test]
    fn all_rule_drops_everything() {
        let entries = v(&["echo hi", "echo bye"]);
        assert!(apply_ignore_rules(&entries, &v(&["all"])).is_empty());
    }

    #[test]
    fn whitespace_rule_skips_space_prefixed_lines() {
        let entries = v(&["echo kept", " echo secret", "echo also kept"]);
        assert_eq!(
            apply_ignore_rules(&entries, &v(&["whitespace"])),
            v(&["echo kept", "echo also kept"])
        );
    }

    /// Matches the manual's exact wording: "All preceding duplicate
    /// commands are removed/ignored from the history after a matching
    /// command is entered" — not just adjacent dedup.
    #[test]
    fn duplicates_rule_keeps_only_latest_occurrence() {
        let entries = v(&["echo a", "echo b", "echo a", "echo c"]);
        assert_eq!(
            apply_ignore_rules(&entries, &v(&["duplicates"])),
            v(&["echo b", "echo a", "echo c"])
        );
    }

    #[test]
    fn regex_rule_filters_matching_lines() {
        let entries = v(&["echo keep", "echo secret_token", "echo also_keep"]);
        assert_eq!(
            apply_ignore_rules(&entries, &v(&["regex:secret"])),
            v(&["echo keep", "echo also_keep"])
        );
    }

    #[test]
    fn empty_rules_keep_everything() {
        let entries = v(&["echo a", "echo a", " echo b"]);
        assert_eq!(apply_ignore_rules(&entries, &[]), entries);
    }

    #[test]
    fn timestamp_marker_detection() {
        assert!(is_timestamp_marker("#1700000000"));
        assert!(!is_timestamp_marker("#not-a-number"));
        assert!(!is_timestamp_marker("echo hi"));
    }

    #[test]
    fn seed_defaults_populates_expected_variables() {
        let mut interp = Interpreter::new();
        seed_defaults(&mut interp);
        assert!(interp.get_scalar(HISTFILE_VAR).is_some());
        assert_eq!(interp.get_scalar(HISTFILE_ENABLED_VAR).unwrap(), "1");
        assert_eq!(
            interp.get_array(HISTORY_IGNORE_VAR).unwrap(),
            &default_ignore_rules()
        );
        assert_eq!(interp.get_scalar(HISTORY_TIMESTAMP_VAR).unwrap(), "0");
    }

    #[test]
    fn seed_defaults_does_not_override_user_values() {
        let mut interp = Interpreter::new();
        interp.set_scalar(HISTFILE_ENABLED_VAR.to_string(), "0".to_string());
        seed_defaults(&mut interp);
        assert_eq!(interp.get_scalar(HISTFILE_ENABLED_VAR).unwrap(), "0");
    }

    #[test]
    fn no_such_command_rule_is_detected_from_current_interpreter_state() {
        let mut interp = Interpreter::new();
        seed_defaults(&mut interp);
        assert!(ignores_no_such_command(&interp));
        interp.set_array(HISTORY_IGNORE_VAR.to_string(), v(&["duplicates"]));
        assert!(!ignores_no_such_command(&interp));
    }

    #[test]
    fn append_preserves_existing_commands_and_load_applies_duplicate_rule() {
        let (interp, path) = temp_history("append");
        append(&interp, &v(&["echo first", "echo shared"]));
        append(&interp, &v(&["echo second", "echo shared"]));
        assert_eq!(
            load(&interp),
            v(&["echo first", "echo second", "echo shared"])
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn append_respects_live_filter_and_timestamp_settings() {
        let (mut interp, path) = temp_history("rules");
        interp.set_array(
            HISTORY_IGNORE_VAR.to_string(),
            v(&["whitespace", "regex:secret"]),
        );
        interp.set_scalar(HISTORY_TIMESTAMP_VAR.to_string(), "1".to_string());
        append(&interp, &v(&["echo kept", " echo private", "echo secret"]));
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.lines().next().is_some_and(is_timestamp_marker));
        assert_eq!(load(&interp), v(&["echo kept"]));
        let _ = std::fs::remove_file(path);
    }

    #[cfg(windows)]
    #[test]
    fn concurrent_appenders_do_not_overwrite_each_other() {
        let (interp, path) = temp_history("concurrent");
        let path_text = interp.get_scalar(HISTFILE_VAR).unwrap().to_string();
        let threads: Vec<_> = (0..12)
            .map(|i| {
                let path_text = path_text.clone();
                std::thread::spawn(move || {
                    let mut writer = Interpreter::new();
                    seed_defaults(&mut writer);
                    writer.set_scalar(HISTFILE_VAR.to_string(), path_text);
                    writer.set_array(HISTORY_IGNORE_VAR.to_string(), Vec::new());
                    append(&writer, &[format!("echo window-{i}")]);
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let mut loaded = load(&interp);
        loaded.sort();
        let mut expected: Vec<_> = (0..12).map(|i| format!("echo window-{i}")).collect();
        expected.sort();
        assert_eq!(loaded, expected);
        let _ = std::fs::remove_file(path);
    }
}
