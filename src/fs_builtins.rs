use std::fs;
use std::path::PathBuf;

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
}
