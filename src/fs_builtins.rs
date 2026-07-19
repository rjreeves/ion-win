use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
enum EntryKind {
    Dir,
    File,
}

pub fn capture(name: &str, args: &[String]) -> Option<Result<String, String>> {
    match name {
        "pwd" => Some(pwd(args)),
        "dirs" | "folders" => Some(list_entries(args, EntryKind::Dir)),
        "files" => Some(list_entries(args, EntryKind::File)),
        "cat" => Some(cat(args)),
        "find" => Some(find(args)),
        _ => None,
    }
}

/// Reads one or more files as UTF-8 (lossy — a file that isn't valid
/// UTF-8 still produces *something* rather than erroring outright,
/// consistent with `from-json`'s own `String::from_utf8_lossy` on piped
/// bytes elsewhere in this codebase) and concatenates them in argument
/// order, matching real `cat`'s multi-file behavior. Stops at the first
/// unreadable file rather than skipping it and continuing — every other
/// error path in this module (and in `pipeline_exec.rs`'s structured
/// pipeline stages) is fail-fast, and a partial read silently missing a
/// file's content would be a worse surprise than just stopping.
fn cat(args: &[String]) -> Result<String, String> {
    if args.is_empty() {
        return Err("cat: usage: cat FILE...".to_string());
    }
    let mut out = String::new();
    for path in args {
        let bytes = fs::read(path).map_err(|e| format!("cat: {path}: {e}"))?;
        out.push_str(&String::from_utf8_lossy(&bytes));
    }
    Ok(out)
}

const FIND_USAGE: &str = "find: usage: find [--all] [--recurse] [PATH]";

/// `find [--all] [--recurse] [PATH]` (ion-win extension, `ARCHITECTURE.md`
/// §22): lists files under `PATH` (defaulting to `.`), optionally
/// recursing into subdirectories. Files only — not the directories
/// themselves — matching the motivating use case (gathering files for a
/// manifest, piping into `stat`), and dotfiles are skipped unless
/// `--all`/`-a` is given, mirroring `files`/`folders`'s existing
/// convention. Output paths use forward slashes regardless of platform
/// (`sub/nested.txt`, not `sub\nested.txt` or a `./`-prefixed top level)
/// so they look the same piped to `stat` as they do printed to a terminal.
fn find(args: &[String]) -> Result<String, String> {
    let mut include_dot = false;
    let mut recurse = false;
    let mut path: Option<String> = None;

    for arg in args {
        match arg.as_str() {
            "--all" | "-a" => include_dot = true,
            "--recurse" | "-r" => recurse = true,
            "--help" | "-h" => return Err(FIND_USAGE.to_string()),
            _ if arg.starts_with('-') => return Err(format!("find: unknown option: {arg}")),
            _ if path.is_none() => path = Some(arg.clone()),
            _ => return Err(FIND_USAGE.to_string()),
        }
    }

    // The accumulated prefix must start with the given PATH itself, not
    // empty — otherwise a recursive result like "top.txt" would silently
    // mean "find_smoke_root/top.txt" without saying so, an invalid path
    // relative to the caller's actual cwd (exactly the kind of thing that
    // then breaks piping straight into `stat`). The default `.` is the one
    // exception: real Unix `find .` technically prints `./file`, but a
    // bare `file` is just as valid relative to cwd and reads cleaner.
    let (base, initial_prefix) = match &path {
        Some(p) => (PathBuf::from(p), format!("{}/", p.trim_end_matches(['/', '\\']))),
        None => (PathBuf::from("."), String::new()),
    };
    // The starting path failing is a clear usage error (bad argument), not
    // a mid-scan race the way a subdirectory failing partway through the
    // walk is — checked once, up front, and propagated as a hard error
    // rather than silently returning an empty list.
    if let Err(e) = fs::read_dir(&base) {
        return Err(format!("find: {}: {e}", base.display()));
    }

    let mut names = Vec::new();
    walk_files(&base, &initial_prefix, recurse, include_dot, &mut names);
    names.sort();
    Ok(names.join("\n"))
}

/// Recurses into `dir`, appending each file's path (prefixed with
/// `prefix`, the accumulated relative path so far) to `out`. A
/// subdirectory that fails to read partway through the walk (permissions,
/// a race during the scan) is skipped with a printed warning rather than
/// aborting the whole scan — `find` is describing a batch, the same
/// reasoning `stat` (§21) uses for a single unreadable file.
fn walk_files(dir: &Path, prefix: &str, recurse: bool, include_dot: bool, out: &mut Vec<String>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            crate::err_println!("ion-win: find: {}: {e}", dir.display());
            return;
        }
    };

    for entry in entries {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if !include_dot && name.starts_with('.') {
            continue;
        }
        let rel = format!("{prefix}{name}");
        let entry_path = entry.path();
        if entry_path.is_dir() {
            if recurse {
                walk_files(&entry_path, &format!("{rel}/"), recurse, include_dot, out);
            }
        } else {
            out.push(rel);
        }
    }
}

fn pwd(args: &[String]) -> Result<String, String> {
    if !args.is_empty() {
        return Err("pwd: usage: pwd".to_string());
    }
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| format!("pwd: {e}"))
}

fn list_entries(args: &[String], kind: EntryKind) -> Result<String, String> {
    let mut include_dot = false;
    let mut full_paths = false;
    let mut path: Option<String> = None;

    for arg in args {
        match arg.as_str() {
            "--all" | "-a" => include_dot = true,
            "--full" | "-f" => full_paths = true,
            "--help" | "-h" => return Err(usage(kind).to_string()),
            _ if arg.starts_with('-') => {
                return Err(format!("{}: unknown option: {arg}", command_name(kind)))
            }
            _ if path.is_none() => path = Some(arg.clone()),
            _ => return Err(usage(kind).to_string()),
        }
    }

    let base = path
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let full_base = match path.as_ref().map(PathBuf::from) {
        None => std::env::current_dir().map_err(|e| format!("{}: {e}", command_name(kind)))?,
        Some(path) if path.is_absolute() => path,
        Some(path) => std::env::current_dir()
            .map_err(|e| format!("{}: {e}", command_name(kind)))?
            .join(path),
    };
    let entries = fs::read_dir(&base)
        .map_err(|e| format!("{}: {}: {e}", command_name(kind), base.display()))?;
    let mut names = Vec::new();

    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", command_name(kind)))?;
        let entry_path = entry.path();
        let matches_kind = match kind {
            EntryKind::Dir => entry_path.is_dir(),
            EntryKind::File => entry_path.is_file(),
        };
        if !matches_kind {
            continue;
        }

        let name = entry.file_name().to_string_lossy().into_owned();
        if !include_dot && name.starts_with('.') {
            continue;
        }

        if full_paths {
            names.push(full_base.join(&name).to_string_lossy().into_owned());
        } else {
            names.push(name);
        }
    }

    names.sort_by_key(|name| name.to_lowercase());
    Ok(names.join("\n"))
}

fn command_name(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::Dir => "folders",
        EntryKind::File => "files",
    }
}

fn usage(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::Dir => "folders: usage: folders [--all] [--full] [PATH]",
        EntryKind::File => "files: usage: files [--all] [--full] [PATH]",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A uniquely-named temp file, so parallel `#[test]` runs in this same
    /// binary never collide on the path.
    fn temp_file(name: &str, contents: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("ion-win-test-{}-{name}", std::process::id()));
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    #[test]
    fn cat_reads_a_single_file_verbatim() {
        let path = temp_file("cat-single.txt", "hello world\n");
        assert_eq!(
            capture("cat", &[path.to_string_lossy().into_owned()]),
            Some(Ok("hello world\n".to_string()))
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn cat_concatenates_multiple_files_in_argument_order() {
        let a = temp_file("cat-a.txt", "first\n");
        let b = temp_file("cat-b.txt", "second\n");
        let args = vec![a.to_string_lossy().into_owned(), b.to_string_lossy().into_owned()];
        assert_eq!(capture("cat", &args), Some(Ok("first\nsecond\n".to_string())));
        let _ = fs::remove_file(a);
        let _ = fs::remove_file(b);
    }

    #[test]
    fn cat_requires_at_least_one_file() {
        assert_eq!(
            capture("cat", &[]),
            Some(Err("cat: usage: cat FILE...".to_string()))
        );
    }

    #[test]
    fn cat_reports_a_clear_error_for_a_missing_file() {
        let missing = "ion-win-definitely-does-not-exist-12345.txt";
        let result = capture("cat", &[missing.to_string()]);
        assert!(matches!(result, Some(Err(ref e)) if e.contains("cat:") && e.contains(missing)));
    }

    #[test]
    fn cat_stops_at_the_first_unreadable_file_rather_than_silently_skipping_it() {
        let a = temp_file("cat-stop-a.txt", "first\n");
        let args = vec![
            a.to_string_lossy().into_owned(),
            "ion-win-definitely-does-not-exist-67890.txt".to_string(),
        ];
        assert!(matches!(capture("cat", &args), Some(Err(_))));
        let _ = fs::remove_file(a);
    }

    /// A small tree for `find`'s tests: `top.txt`, `.hidden.txt`, and
    /// `sub/nested.txt` — enough to exercise recursion, dotfile filtering,
    /// and path prefixing all at once. Uniquely named per test (like
    /// `temp_file`) so parallel test runs never collide.
    fn temp_dir_tree(name: &str) -> PathBuf {
        let mut root = std::env::temp_dir();
        root.push(format!("ion-win-find-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("top.txt"), "top").unwrap();
        fs::write(root.join(".hidden.txt"), "hidden").unwrap();
        fs::write(root.join("sub").join("nested.txt"), "nested").unwrap();
        root
    }

    /// Results are prefixed with whatever `PATH` was actually given
    /// (`ARCHITECTURE.md` §22 — a real bug caught by the real-binary smoke
    /// test: an earlier version returned bare names like `top.txt` with no
    /// prefix at all when `PATH` wasn't `.`, which look valid but resolve
    /// to the wrong location relative to the caller's actual cwd — exactly
    /// the kind of thing that then breaks piping straight into `stat`).
    /// Normalized to forward slashes here since `temp_dir()` is
    /// backslash-separated on Windows; `find` itself just preserves
    /// whatever separator style the given path already used.
    #[test]
    fn find_non_recursive_lists_only_the_top_level_visible_files() {
        let root = temp_dir_tree("nonrecursive");
        let root_str = root.to_string_lossy().replace('\\', "/");
        let result = capture("find", &[root_str.clone()]).unwrap().unwrap();
        assert_eq!(result, format!("{root_str}/top.txt"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn find_recurse_includes_nested_files_with_slash_separated_paths() {
        let root = temp_dir_tree("recursive");
        let root_str = root.to_string_lossy().replace('\\', "/");
        let result = capture("find", &[root_str.clone(), "--recurse".to_string()])
            .unwrap()
            .unwrap();
        assert_eq!(result, format!("{root_str}/sub/nested.txt\n{root_str}/top.txt"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn find_all_includes_dotfiles() {
        let root = temp_dir_tree("dotfiles");
        let root_str = root.to_string_lossy().replace('\\', "/");
        let result = capture("find", &[root_str.clone(), "--all".to_string()]).unwrap().unwrap();
        assert_eq!(result, format!("{root_str}/.hidden.txt\n{root_str}/top.txt"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn find_reports_a_clear_error_for_a_missing_starting_path() {
        let result = capture("find", &["ion-win-definitely-does-not-exist-dir".to_string()]);
        assert!(matches!(result, Some(Err(ref e)) if e.contains("find:")));
    }

    #[test]
    fn find_rejects_unknown_flag() {
        let result = capture("find", &["--bogus".to_string()]);
        assert!(matches!(result, Some(Err(ref e)) if e.contains("unknown option")));
    }
}
