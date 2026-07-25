//! Persistent command history (ion-manual pages 84-87).
//!
//! History is shared live across concurrent ion-win windows. Accepted
//! commands are appended immediately under a cross-process lock, and each
//! prompt reloads the file so Up-arrow sees commands entered in other
//! windows. `HISTFILE`, `HISTFILE_ENABLED`, `HISTFILE_SIZE`,
//! `HISTORY_SIZE`, `HISTORY_SESSION_ID`, `HISTORY_IGNORE`, and
//! `HISTORY_TIMESTAMP` are seeded as ordinary ion
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
use crate::interp::Interpreter;
use crate::types::{validate, TypeTag};
use regex::Regex;
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const HISTFILE_VAR: &str = "HISTFILE";
const HISTFILE_ENABLED_VAR: &str = "HISTFILE_ENABLED";
const HISTFILE_SIZE_VAR: &str = "HISTFILE_SIZE";
const HISTORY_IGNORE_VAR: &str = "HISTORY_IGNORE";
const HISTORY_SESSION_ID_VAR: &str = "HISTORY_SESSION_ID";
const HISTORY_SIZE_VAR: &str = "HISTORY_SIZE";
const HISTORY_TIMESTAMP_VAR: &str = "HISTORY_TIMESTAMP";
const DEFAULT_HISTFILE_SIZE: usize = 100_000;
const DEFAULT_HISTORY_SIZE: usize = 1_000;
const SESSION_MARKER: &str = "#@session=";
static SESSION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
struct HistoryRecord {
    timestamp: Option<u64>,
    session: Option<String>,
    command: String,
}

/// Seeds history configuration and the process-session identifier as ordinary
/// Ion variables, visible to scripts and live-overridable where appropriate.
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
    if interp.get_scalar(HISTFILE_SIZE_VAR).is_none() {
        interp.set_scalar(
            HISTFILE_SIZE_VAR.to_string(),
            DEFAULT_HISTFILE_SIZE.to_string(),
        );
    }
    if interp.get_scalar(HISTORY_SIZE_VAR).is_none() {
        interp.set_scalar(
            HISTORY_SIZE_VAR.to_string(),
            DEFAULT_HISTORY_SIZE.to_string(),
        );
    }
    if interp.get_scalar(HISTORY_SESSION_ID_VAR).is_none() {
        interp.set_scalar(HISTORY_SESSION_ID_VAR.to_string(), new_session_id());
    }
    if interp.get_array(HISTORY_IGNORE_VAR).is_none() {
        interp.set_array(HISTORY_IGNORE_VAR.to_string(), default_ignore_rules());
    }
    if interp.get_scalar(HISTORY_TIMESTAMP_VAR).is_none() {
        interp.set_scalar(HISTORY_TIMESTAMP_VAR.to_string(), "0".to_string());
    }
}

fn new_session_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let sequence = SESSION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("s-{nanos:032x}-{:08x}-{sequence:08x}", std::process::id())
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

fn configured_limit(interp: &Interpreter, name: &str, default: usize) -> usize {
    interp
        .get_scalar(name)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
}

fn session_id(interp: &Interpreter) -> String {
    interp
        .get_scalar(HISTORY_SESSION_ID_VAR)
        .cloned()
        .unwrap_or_else(new_session_id)
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
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

fn parse_records(contents: &str) -> Vec<HistoryRecord> {
    let mut session = None;
    let mut timestamp = None;
    let mut records = Vec::new();
    for line in contents.lines() {
        if let Some(id) = line.strip_prefix(SESSION_MARKER) {
            session = (!id.is_empty()).then(|| id.to_string());
        } else if is_timestamp_marker(line) {
            timestamp = line[1..].parse::<u64>().ok();
        } else {
            records.push(HistoryRecord {
                timestamp: timestamp.take(),
                session: session.clone(),
                command: line.to_string(),
            });
        }
    }
    records
}

fn serialize_records(records: &[HistoryRecord]) -> String {
    let mut output = String::new();
    let mut active_session: Option<&str> = None;
    for record in records {
        let record_session = record.session.as_deref();
        if record_session != active_session {
            if let Some(session) = record_session {
                output.push_str(SESSION_MARKER);
                output.push_str(session);
                output.push('\n');
            } else {
                output.push_str(SESSION_MARKER);
                output.push('\n');
            }
            active_session = record_session;
        }
        if let Some(timestamp) = record.timestamp {
            output.push('#');
            output.push_str(&timestamp.to_string());
            output.push('\n');
        }
        output.push_str(&record.command);
        output.push('\n');
    }
    output
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
    let entries: Vec<String> = parse_records(&contents)
        .into_iter()
        .map(|record| record.command)
        .collect();
    let rules = interp
        .get_array(HISTORY_IGNORE_VAR)
        .cloned()
        .unwrap_or_else(default_ignore_rules);
    let mut entries = apply_ignore_rules(&entries, &rules);
    retain_latest(&mut entries, configured_limit(interp, HISTORY_SIZE_VAR, DEFAULT_HISTORY_SIZE));
    entries
}

/// Returns a fresh shared-history snapshot while persistence is enabled.
/// `None` means the live `HISTFILE_ENABLED` setting is off, so callers
/// should retain their in-memory session history.
pub fn refresh(interp: &Interpreter) -> Option<Vec<String>> {
    is_enabled(interp).then(|| load(interp))
}

/// Appends newly accepted commands as one locked record batch, tags them with
/// the current process-session ID, then compacts to `HISTFILE_SIZE` while the
/// same cross-process lock is held.
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
    let timestamped = is_timestamped(interp);
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut out = String::new();
    out.push_str(SESSION_MARKER);
    out.push_str(&session_id(interp));
    out.push('\n');
    for line in filtered {
        if timestamped {
            out.push_str(&format!("#{epoch}\n"));
        }
        out.push_str(&line);
        out.push('\n');
    }
    {
        let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) else {
            return;
        };
        if file.write_all(out.as_bytes()).is_err() || file.flush().is_err() {
            return;
        }
    }
    let limit = configured_limit(interp, HISTFILE_SIZE_VAR, DEFAULT_HISTFILE_SIZE);
    let _ = compact_locked(&path, limit);
}

fn retain_latest<T>(items: &mut Vec<T>, limit: usize) {
    if items.len() > limit {
        items.drain(..items.len() - limit);
    }
}

fn compact_locked(path: &std::path::Path, limit: usize) -> std::io::Result<usize> {
    let contents = std::fs::read_to_string(path).unwrap_or_default();
    let mut records = parse_records(&contents);
    if records.len() <= limit {
        return Ok(records.len());
    }
    retain_latest(&mut records, limit);
    let output = serialize_records(&records);
    let temp = path.with_extension(format!(
        "compact-{}-{}",
        std::process::id(),
        SESSION_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&temp, output)?;
    if let Err(error) = replace_file(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    Ok(records.len())
}

#[cfg(windows)]
fn replace_file(source: &std::path::Path, destination: &std::path::Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let ok = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file(source: &std::path::Path, destination: &std::path::Path) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

/// Renders persisted history or session summaries for the `history` builtin.
pub fn inspect(interp: &Interpreter, args: &[String]) -> Result<String, String> {
    if !is_enabled(interp) {
        return Ok(String::new());
    }
    let path = histfile_path(interp);
    let Some(_lock) = HistoryLock::acquire(&path) else {
        return Err("history: could not acquire the history lock".to_string());
    };

    if args == ["--compact"] {
        let kept = compact_locked(
            &path,
            configured_limit(interp, HISTFILE_SIZE_VAR, DEFAULT_HISTFILE_SIZE),
        )
        .map_err(|error| format!("history: compaction failed: {error}"))?;
        return Ok(format!("history: compacted to {kept} entries"));
    }

    let contents = std::fs::read_to_string(&path).unwrap_or_default();
    let mut records = parse_records(&contents);
    if args == ["--sessions"] {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for record in records {
            *counts
                .entry(record.session.unwrap_or_else(|| "<legacy>".to_string()))
                .or_default() += 1;
        }
        return Ok(counts
            .into_iter()
            .map(|(session, count)| format!("{session}\t{count}"))
            .collect::<Vec<_>>()
            .join("\n"));
    }

    let mut requested_session = None;
    let mut display_limit = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--session" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "history: --session requires an ID or current".to_string())?;
                requested_session = Some(if value == "current" {
                    session_id(interp)
                } else {
                    value.clone()
                });
                index += 2;
            }
            "--limit" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "history: --limit requires a number".to_string())?;
                display_limit = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| format!("history: invalid limit '{value}'"))?,
                );
                index += 2;
            }
            option => return Err(format!("history: unknown option '{option}'")),
        }
    }
    if let Some(session) = requested_session {
        records.retain(|record| record.session.as_deref() == Some(session.as_str()));
    }
    let rules = interp
        .get_array(HISTORY_IGNORE_VAR)
        .cloned()
        .unwrap_or_else(default_ignore_rules);
    records = apply_ignore_rules_to_records(records, &rules);
    retain_latest(
        &mut records,
        configured_limit(interp, HISTORY_SIZE_VAR, DEFAULT_HISTORY_SIZE),
    );
    if let Some(limit) = display_limit {
        retain_latest(&mut records, limit);
    }
    Ok(records
        .iter()
        .enumerate()
        .map(|(index, record)| format!("{:>5}  {}", index + 1, record.command))
        .collect::<Vec<_>>()
        .join("\n"))
}

fn apply_ignore_rules_to_records(
    records: Vec<HistoryRecord>,
    rules: &[String],
) -> Vec<HistoryRecord> {
    let kept_commands = apply_ignore_rules(
        &records
            .iter()
            .map(|record| record.command.clone())
            .collect::<Vec<_>>(),
        rules,
    );
    let mut remaining = kept_commands;
    let mut output = Vec::new();
    for record in records.into_iter().rev() {
        if let Some(position) = remaining.iter().rposition(|command| command == &record.command) {
            remaining.remove(position);
            output.push(record);
        }
    }
    output.reverse();
    output
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
    fn legacy_and_session_tagged_records_parse_together() {
        let records = parse_records(
            "echo legacy\n#@session=window-a\n#1700000000\necho tagged\n#@session=window-b\necho other\n",
        );
        assert_eq!(
            records,
            vec![
                HistoryRecord {
                    timestamp: None,
                    session: None,
                    command: "echo legacy".to_string(),
                },
                HistoryRecord {
                    timestamp: Some(1_700_000_000),
                    session: Some("window-a".to_string()),
                    command: "echo tagged".to_string(),
                },
                HistoryRecord {
                    timestamp: None,
                    session: Some("window-b".to_string()),
                    command: "echo other".to_string(),
                },
            ]
        );
        assert_eq!(parse_records(&serialize_records(&records)), records);
    }

    #[test]
    fn seed_defaults_populates_expected_variables() {
        let mut interp = Interpreter::new();
        seed_defaults(&mut interp);
        assert!(interp.get_scalar(HISTFILE_VAR).is_some());
        assert_eq!(interp.get_scalar(HISTFILE_ENABLED_VAR).unwrap(), "1");
        assert_eq!(interp.get_scalar(HISTFILE_SIZE_VAR).unwrap(), "100000");
        assert_eq!(interp.get_scalar(HISTORY_SIZE_VAR).unwrap(), "1000");
        assert!(interp
            .get_scalar(HISTORY_SESSION_ID_VAR)
            .is_some_and(|id| id.starts_with("s-")));
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
    fn session_id_is_stable_per_interpreter_and_unique_between_sessions() {
        let mut first = Interpreter::new();
        seed_defaults(&mut first);
        let original = first.get_scalar(HISTORY_SESSION_ID_VAR).unwrap().clone();
        seed_defaults(&mut first);
        assert_eq!(first.get_scalar(HISTORY_SESSION_ID_VAR).unwrap(), &original);

        let mut second = Interpreter::new();
        seed_defaults(&mut second);
        assert_ne!(
            second.get_scalar(HISTORY_SESSION_ID_VAR).unwrap(),
            &original
        );
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
        assert!(raw.lines().next().unwrap().starts_with(SESSION_MARKER));
        assert!(raw.lines().any(is_timestamp_marker));
        assert_eq!(load(&interp), v(&["echo kept"]));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn history_size_limits_the_live_recall_snapshot() {
        let (mut interp, path) = temp_history("memory-limit");
        interp.set_array(HISTORY_IGNORE_VAR.to_string(), Vec::new());
        interp.set_scalar(HISTORY_SIZE_VAR.to_string(), "2".to_string());
        append(&interp, &v(&["echo one", "echo two", "echo three"]));
        assert_eq!(load(&interp), v(&["echo two", "echo three"]));
        assert_eq!(parse_records(&std::fs::read_to_string(&path).unwrap()).len(), 3);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn histfile_size_compacts_to_latest_records_with_metadata() {
        let (mut interp, path) = temp_history("file-limit");
        interp.set_array(HISTORY_IGNORE_VAR.to_string(), Vec::new());
        interp.set_scalar(HISTFILE_SIZE_VAR.to_string(), "2".to_string());
        interp.set_scalar(HISTORY_TIMESTAMP_VAR.to_string(), "1".to_string());
        append(&interp, &v(&["echo one", "echo two"]));
        interp.set_scalar(HISTORY_SESSION_ID_VAR.to_string(), "second-window".to_string());
        append(&interp, &v(&["echo three"]));

        let records = parse_records(&std::fs::read_to_string(&path).unwrap());
        assert_eq!(
            records.iter().map(|r| r.command.as_str()).collect::<Vec<_>>(),
            vec!["echo two", "echo three"]
        );
        assert!(records.iter().all(|record| record.timestamp.is_some()));
        assert_eq!(records[1].session.as_deref(), Some("second-window"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn history_inspection_filters_sessions_and_lists_counts() {
        let (mut interp, path) = temp_history("inspect");
        interp.set_array(HISTORY_IGNORE_VAR.to_string(), Vec::new());
        interp.set_scalar(HISTORY_SESSION_ID_VAR.to_string(), "window-a".to_string());
        append(&interp, &v(&["echo a1", "echo a2"]));
        interp.set_scalar(HISTORY_SESSION_ID_VAR.to_string(), "window-b".to_string());
        append(&interp, &v(&["echo b"]));

        let current = inspect(&interp, &v(&["--session", "current"])).unwrap();
        assert!(current.contains("echo b"));
        assert!(!current.contains("echo a"));
        let sessions = inspect(&interp, &v(&["--sessions"])).unwrap();
        assert!(sessions.contains("window-a\t2"));
        assert!(sessions.contains("window-b\t1"));
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

    #[cfg(windows)]
    #[test]
    fn concurrent_compacting_appenders_leave_a_valid_bounded_file() {
        let (interp, path) = temp_history("concurrent-compact");
        let path_text = interp.get_scalar(HISTFILE_VAR).unwrap().to_string();
        let threads: Vec<_> = (0..12)
            .map(|i| {
                let path_text = path_text.clone();
                std::thread::spawn(move || {
                    let mut writer = Interpreter::new();
                    seed_defaults(&mut writer);
                    writer.set_scalar(HISTFILE_VAR.to_string(), path_text);
                    writer.set_scalar(HISTFILE_SIZE_VAR.to_string(), "5".to_string());
                    writer.set_array(HISTORY_IGNORE_VAR.to_string(), Vec::new());
                    append(&writer, &[format!("echo compact-window-{i}")]);
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let records = parse_records(&std::fs::read_to_string(&path).unwrap());
        assert_eq!(records.len(), 5);
        assert!(records
            .iter()
            .all(|record| record.command.starts_with("echo compact-window-")));
        let _ = std::fs::remove_file(path);
    }
}
