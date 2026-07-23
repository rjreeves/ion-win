//! Native Windows filesystem conveniences missing from the base Ion
//! language: `mkdir`/`md`, `move`/`mv`, and `rename`/`ren`.

use std::fs;
use std::path::{Component, Path, PathBuf};

const MOVE_USAGE: &str = "move: usage: move [--force] SRC... DEST  |  TABLE | move [--force] DEST";
const RENAME_USAGE: &str = "rename: usage: rename [--force] SOURCE NEW_NAME";

pub fn mkdir(args: &[String]) -> Result<String, String> {
    if args.is_empty()
        || args
            .iter()
            .any(|arg| matches!(arg.as_str(), "-h" | "--help"))
    {
        return Err("mkdir: usage: mkdir DIR...".to_string());
    }
    if let Some(option) = args.iter().find(|arg| arg.starts_with('-')) {
        return Err(format!("mkdir: unknown option: {option}"));
    }

    let mut created = 0usize;
    let mut skipped = 0usize;
    for dir in args {
        let path = Path::new(dir);
        if path.is_dir() {
            skipped += 1;
            continue;
        }
        fs::create_dir_all(path).map_err(|e| format!("mkdir: {dir}: {e}"))?;
        created += 1;
    }
    Ok(if skipped > 0 {
        format!("ion-win: mkdir: created {created} folder(s), already existed {skipped}")
    } else {
        format!("ion-win: mkdir: created {created} folder(s)")
    })
}

pub fn parse_move_flags(args: &[String]) -> Result<(bool, Vec<String>), String> {
    let mut force = false;
    let mut positional = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--force" | "-f" => force = true,
            "--help" | "-h" => return Err(MOVE_USAGE.to_string()),
            _ if arg.starts_with('-') => return Err(format!("move: unknown option: {arg}")),
            _ => positional.push(arg.clone()),
        }
    }
    Ok((force, positional))
}

pub async fn parse_and_move(args: &[String]) -> Result<String, String> {
    let (force, mut positional) = parse_move_flags(args)?;
    if positional.len() < 2 {
        return Err(MOVE_USAGE.to_string());
    }
    let dest = positional.pop().expect("checked length");
    Ok(move_paths(&positional, &dest, force).await)
}

pub async fn move_paths(sources: &[String], dest: &str, force: bool) -> String {
    let dest_path = Path::new(dest);
    let dest_is_dir =
        dest_path.is_dir() || sources.len() > 1 || dest.ends_with('/') || dest.ends_with('\\');
    let mut handles = Vec::with_capacity(sources.len());
    for source in sources {
        let target = if dest_is_dir {
            dest_path.join(
                Path::new(source)
                    .file_name()
                    .unwrap_or_else(|| Path::new(source).as_os_str()),
            )
        } else {
            dest_path.to_path_buf()
        };
        let source = PathBuf::from(source);
        handles.push(tokio::task::spawn_blocking(move || {
            move_one(&source, &target, force)
        }));
    }
    await_moves(handles, 0).await
}

pub async fn move_table(table: &crate::table::Table, dest: &str, force: bool) -> String {
    let dest = Path::new(dest);
    let mut handles = Vec::with_capacity(table.rows.len());
    let mut skipped = 0usize;
    for row in &table.rows {
        let Some((_, source)) = row.iter().find(|(name, _)| name == "path") else {
            crate::err_println!("ion-win: move: row has no 'path' column");
            skipped += 1;
            continue;
        };
        let target = table_target(dest, source);
        let source = PathBuf::from(source);
        handles.push(tokio::task::spawn_blocking(move || {
            move_one(&source, &target, force)
        }));
    }
    await_moves(handles, skipped).await
}

async fn await_moves(
    handles: Vec<tokio::task::JoinHandle<Result<(), String>>>,
    mut skipped: usize,
) -> String {
    let mut moved = 0usize;
    for handle in handles {
        match handle.await {
            Ok(Ok(())) => moved += 1,
            Ok(Err(error)) => {
                crate::err_println!("ion-win: move: {error}");
                skipped += 1;
            }
            Err(error) => {
                crate::err_println!("ion-win: move: task failed: {error}");
                skipped += 1;
            }
        }
    }
    if skipped > 0 {
        format!("ion-win: move: moved {moved} item(s), skipped {skipped}")
    } else {
        format!("ion-win: move: moved {moved} item(s)")
    }
}

fn table_target(dest: &Path, source: &str) -> PathBuf {
    let relative: PathBuf = Path::new(source)
        .components()
        .filter(|part| !matches!(part, Component::Prefix(_) | Component::RootDir))
        .collect();
    dest.join(relative)
}

fn move_one(source: &Path, target: &Path, force: bool) -> Result<(), String> {
    let metadata =
        fs::symlink_metadata(source).map_err(|error| format!("{}: {error}", source.display()))?;

    if source == target
        || (target.exists()
            && source
                .canonicalize()
                .ok()
                .zip(target.canonicalize().ok())
                .is_some_and(|(source, target)| source == target))
    {
        return Err(format!(
            "{}: source and destination are the same item",
            source.display()
        ));
    }

    if target.exists() {
        if !force {
            return Err(format!(
                "{}: destination already exists (use --force to replace a file)",
                target.display()
            ));
        }
        let target_metadata = fs::symlink_metadata(target)
            .map_err(|error| format!("{}: {error}", target.display()))?;
        if target_metadata.is_dir() {
            return Err(format!(
                "{}: refusing to replace an existing directory",
                target.display()
            ));
        }
        fs::remove_file(target).map_err(|error| format!("{}: {error}", target.display()))?;
    }

    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
        }
    }

    match fs::rename(source, target) {
        Ok(()) => Ok(()),
        Err(rename_error) if metadata.is_file() => {
            fs::copy(source, target).map_err(|copy_error| {
                format!(
                    "{}: move failed ({rename_error}); copy fallback failed ({copy_error})",
                    source.display()
                )
            })?;
            fs::remove_file(source).map_err(|error| {
                let _ = fs::remove_file(target);
                format!(
                    "{}: copied but could not remove source; destination rolled back: {error}",
                    source.display()
                )
            })
        }
        Err(error) => Err(format!("{}: {error}", source.display())),
    }
}

pub async fn rename(args: &[String]) -> Result<String, String> {
    let (force, positional) = parse_rename_flags(args)?;
    let [source, new_name] = positional.as_slice() else {
        return Err(RENAME_USAGE.to_string());
    };
    let new_name_path = Path::new(new_name);
    if new_name_path.is_absolute() || new_name_path.components().count() != 1 {
        return Err(
            "rename: NEW_NAME must be a name, not a path (use move to relocate)".to_string(),
        );
    }
    let source_path = PathBuf::from(source);
    let target = source_path
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(new_name_path);
    let source_for_task = source_path.clone();
    tokio::task::spawn_blocking(move || move_one(&source_for_task, &target, force))
        .await
        .map_err(|error| format!("rename: task failed: {error}"))?
        .map_err(|error| format!("rename: {error}"))?;
    Ok(format!(
        "ion-win: rename: {} -> {}",
        source_path.display(),
        new_name
    ))
}

fn parse_rename_flags(args: &[String]) -> Result<(bool, Vec<String>), String> {
    let mut force = false;
    let mut positional = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--force" | "-f" => force = true,
            "--help" | "-h" => return Err(RENAME_USAGE.to_string()),
            _ if arg.starts_with('-') => return Err(format!("rename: unknown option: {arg}")),
            _ => positional.push(arg.clone()),
        }
    }
    Ok((force, positional))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ion-win-fs-ops-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut file = fs::File::create(path).unwrap();
        file.write_all(contents.as_bytes()).unwrap();
    }

    #[test]
    fn mkdir_creates_parents_and_tallies_existing_folders() {
        let root = temp_dir("mkdir");
        let nested = root.join("a").join("b");
        assert_eq!(
            mkdir(&[nested.to_string_lossy().into_owned()]).unwrap(),
            "ion-win: mkdir: created 1 folder(s)"
        );
        assert!(nested.is_dir());
        assert!(mkdir(&[nested.to_string_lossy().into_owned()])
            .unwrap()
            .contains("already existed 1"));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn move_refuses_overwrite_then_force_replaces_file() {
        let root = temp_dir("overwrite");
        let source = root.join("source.txt");
        let target = root.join("target.txt");
        write_file(&source, "new");
        write_file(&target, "old");
        let skipped = move_paths(
            &[source.to_string_lossy().into_owned()],
            &target.to_string_lossy(),
            false,
        )
        .await;
        assert!(skipped.contains("skipped 1"));
        assert_eq!(fs::read_to_string(&target).unwrap(), "old");
        let moved = move_paths(
            &[source.to_string_lossy().into_owned()],
            &target.to_string_lossy(),
            true,
        )
        .await;
        assert_eq!(moved, "ion-win: move: moved 1 item(s)");
        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
        assert!(!source.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn forced_move_to_self_is_refused_without_data_loss() {
        let root = temp_dir("same-target");
        let source = root.join("keep.txt");
        write_file(&source, "keep");
        let result = move_paths(
            &[source.to_string_lossy().into_owned()],
            &source.to_string_lossy(),
            true,
        )
        .await;
        assert!(result.contains("skipped 1"));
        assert_eq!(fs::read_to_string(&source).unwrap(), "keep");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn move_supports_directories() {
        let root = temp_dir("directory");
        let source = root.join("old");
        write_file(&source.join("nested.txt"), "nested");
        let target = root.join("new");
        let result = move_paths(
            &[source.to_string_lossy().into_owned()],
            &target.to_string_lossy(),
            false,
        )
        .await;
        assert_eq!(result, "ion-win: move: moved 1 item(s)");
        assert!(!source.exists());
        assert_eq!(
            fs::read_to_string(target.join("nested.txt")).unwrap(),
            "nested"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn table_target_preserves_relative_layout() {
        assert_eq!(
            table_target(Path::new("archive"), "sub/folder/item.txt"),
            Path::new("archive")
                .join("sub")
                .join("folder")
                .join("item.txt")
        );
    }

    #[tokio::test]
    async fn move_table_preserves_relative_layout() {
        let root = temp_dir("table");
        let source_root = root.join("source");
        let dest = root.join("dest");
        let source = source_root.join("sub").join("item.txt");
        write_file(&source, "item");
        let source_text = source.to_string_lossy().into_owned();
        let expected = table_target(&dest, &source_text);
        let table = crate::table::Table {
            rows: vec![vec![("path".to_string(), source_text)]],
        };
        let result = move_table(&table, &dest.to_string_lossy(), false).await;
        assert_eq!(result, "ion-win: move: moved 1 item(s)");
        assert_eq!(fs::read_to_string(expected).unwrap(), "item");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn rename_rejects_paths_and_renames_in_place() {
        let root = temp_dir("rename");
        let source = root.join("old.txt");
        write_file(&source, "hello");
        assert!(rename(&[
            source.to_string_lossy().into_owned(),
            "nested/new.txt".to_string()
        ])
        .await
        .unwrap_err()
        .contains("not a path"));
        rename(&[source.to_string_lossy().into_owned(), "new.txt".to_string()])
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(root.join("new.txt")).unwrap(), "hello");
        let _ = fs::remove_dir_all(root);
    }
}
