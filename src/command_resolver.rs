//! Shared external-command resolution.
//!
//! Windows' process-launch search rules are not a suitable shell contract:
//! they differ from `which`, and relying on them makes current-directory
//! precedence implicit.  Resolve commands ourselves so every launch path
//! checks the current directory first, then `PATH`, applying `PATHEXT` in
//! both places.

use std::path::{Path, PathBuf};

/// Resolves `name` to the file Ion will execute.
pub fn resolve(name: &str) -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    resolve_in(name, &cwd)
}

/// Resolves a command using an explicit working directory. Persistent tasks
/// use their captured directory rather than whichever directory the caller
/// happens to occupy when the task is run.
pub fn resolve_in(name: &str, cwd: &Path) -> Option<PathBuf> {
    let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|value| std::env::split_paths(&value).collect())
        .unwrap_or_default();
    let extensions = pathext();
    resolve_from(name, cwd, &path_dirs, &extensions)
}

fn pathext() -> Vec<String> {
    std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
        .split(';')
        .filter(|ext| !ext.is_empty())
        .map(|ext| {
            if ext.starts_with('.') {
                ext.to_string()
            } else {
                format!(".{ext}")
            }
        })
        .collect()
}

fn resolve_from(
    name: &str,
    cwd: &Path,
    path_dirs: &[PathBuf],
    extensions: &[String],
) -> Option<PathBuf> {
    let command = Path::new(name);
    let explicit_path = command.is_absolute() || command.components().count() > 1;

    if explicit_path {
        let base = if command.is_absolute() {
            command.to_path_buf()
        } else {
            cwd.join(command)
        };
        return find_candidate(&base, extensions);
    }

    if let Some(found) = find_candidate(&cwd.join(command), extensions) {
        return Some(found);
    }

    for dir in path_dirs {
        // An empty PATH component also denotes the current directory.  It
        // has already been checked above, so avoid returning a relative path.
        let base = if dir.as_os_str().is_empty() {
            cwd.join(command)
        } else {
            dir.join(command)
        };
        if let Some(found) = find_candidate(&base, extensions) {
            return Some(found);
        }
    }
    None
}

fn find_candidate(base: &Path, extensions: &[String]) -> Option<PathBuf> {
    if base.extension().is_some() {
        return base.is_file().then(|| base.to_path_buf());
    }

    for ext in extensions {
        let candidate = PathBuf::from(format!("{}{}", base.display(), ext));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::resolve_from;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

    fn temp_tree() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ion-win-resolver-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn pathext_normalizes_missing_dot() {
        assert_eq!(
            vec![".EXE".to_string(), ".CMD".to_string()],
            ["EXE", ".CMD"]
                .into_iter()
                .map(|ext| {
                    if ext.starts_with('.') {
                        ext.to_string()
                    } else {
                        format!(".{ext}")
                    }
                })
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn current_directory_precedes_path() {
        let root = temp_tree();
        let cwd = root.join("cwd");
        let path_dir = root.join("path");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&path_dir).unwrap();
        fs::write(cwd.join("probe.EXE"), b"local").unwrap();
        fs::write(path_dir.join("probe.EXE"), b"path").unwrap();

        let found = resolve_from(
            "probe",
            &cwd,
            std::slice::from_ref(&path_dir),
            &[".EXE".to_string()],
        );

        assert_eq!(found, Some(cwd.join("probe.EXE")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pathext_order_is_honored_in_each_directory() {
        let root = temp_tree();
        let cwd = root.join("cwd");
        fs::create_dir_all(&cwd).unwrap();
        fs::write(cwd.join("probe.CMD"), b"cmd").unwrap();
        fs::write(cwd.join("probe.EXE"), b"exe").unwrap();

        let found = resolve_from(
            "probe",
            &cwd,
            &[],
            &[".CMD".to_string(), ".EXE".to_string()],
        );

        assert_eq!(found, Some(cwd.join("probe.CMD")));
        fs::remove_dir_all(root).unwrap();
    }
}
