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
        _ => None,
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
