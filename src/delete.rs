//! Safe file deletion for ion-win. The default operation sends items to
//! the Windows Recycle Bin; irreversible removal is available only with
//! the explicit pair `--permanent --force`. Directories additionally
//! require `--recurse`, and permanent recursion never traverses a symlink
//! or Windows reparse point.

use std::fs;
use std::path::{Path, PathBuf};

const DELETE_USAGE: &str = "delete: usage: delete [--recurse] PATH...  |  TABLE | delete [--recurse]\n       permanent: add --permanent --force";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeleteOptions {
    pub permanent: bool,
    pub force: bool,
    pub recurse: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum DeleteResult {
    Deleted,
    Skipped(String),
    Failed(String),
}

pub fn parse_flags(args: &[String]) -> Result<(DeleteOptions, Vec<String>), String> {
    let mut options = DeleteOptions::default();
    let mut positional = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--permanent" => options.permanent = true,
            "--force" | "-f" => options.force = true,
            "--recurse" | "-r" => options.recurse = true,
            "--help" | "-h" => return Err(DELETE_USAGE.to_string()),
            _ if arg.starts_with('-') => return Err(format!("delete: unknown option: {arg}")),
            _ => positional.push(arg.clone()),
        }
    }
    if options.permanent && !options.force {
        return Err("delete: permanent deletion requires both --permanent and --force".to_string());
    }
    Ok((options, positional))
}

pub fn parse_pipeline_flags(args: &[String]) -> Result<(DeleteOptions, bool, Vec<String>), String> {
    let mut plan = false;
    let remaining = args.iter().filter_map(|arg| {
        if arg == "--plan" { plan = true; None } else { Some(arg.clone()) }
    }).collect::<Vec<_>>();
    let (options, positional) = parse_flags(&remaining)?;
    if plan && options.permanent { return Err("delete: --plan does not support permanent deletion".to_string()); }
    Ok((options, plan, positional))
}

pub(crate) fn validate_planned(path: &Path, options: DeleteOptions) -> Result<(), String> {
    let meta = fs::symlink_metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let target_is_dir = meta.is_dir() || (is_reparse_point(&meta) && fs::metadata(path).map(|value| value.is_dir()).unwrap_or(false));
    if !is_reparse_point(&meta) && !meta.file_type().is_symlink() { reject_broad_target(path)?; }
    if target_is_dir && !options.recurse { return Err(format!("{}: is a directory (use --recurse)", path.display())); }
    Ok(())
}

pub(crate) fn delete_planned(path: &Path, options: DeleteOptions) -> Result<(), String> {
    match validate_then_delete(path, options) {
        DeleteResult::Deleted => Ok(()),
        DeleteResult::Skipped(error) | DeleteResult::Failed(error) => Err(error),
    }
}

pub async fn parse_and_delete_files(args: &[String]) -> Result<String, String> {
    let (options, paths) = parse_flags(args)?;
    if paths.is_empty() {
        return Err(DELETE_USAGE.to_string());
    }
    Ok(delete_paths(&paths, options).await)
}

pub async fn delete_table(table: &crate::table::Table, options: DeleteOptions) -> String {
    let mut paths = Vec::with_capacity(table.rows.len());
    let mut skipped = 0usize;
    for row in &table.rows {
        match row.iter().find(|(name, _)| name == "path") {
            Some((_, path)) => paths.push(path.clone()),
            None => {
                crate::err_println!("ion-win: delete: row has no 'path' column");
                skipped += 1;
            }
        }
    }
    delete_paths_with_initial_skips(&paths, options, skipped).await
}

pub async fn delete_paths(paths: &[String], options: DeleteOptions) -> String {
    delete_paths_with_initial_skips(paths, options, 0).await
}

async fn delete_paths_with_initial_skips(
    paths: &[String],
    options: DeleteOptions,
    initial_skipped: usize,
) -> String {
    let paths = paths.to_vec();
    match tokio::task::spawn_blocking(move || delete_paths_blocking(&paths, options, initial_skipped))
        .await
    {
        Ok(summary) => summary,
        Err(e) => format!("ion-win: delete: deleted 0 item(s), skipped {initial_skipped}, failed 1 (task failed: {e})"),
    }
}

fn delete_paths_blocking(paths: &[String], options: DeleteOptions, mut skipped: usize) -> String {
    let mut deleted = 0usize;
    let mut failed = 0usize;
    for path in paths {
        let result = validate_then_delete(Path::new(path), options);
        match result {
            DeleteResult::Deleted => deleted += 1,
            DeleteResult::Skipped(message) => {
                crate::err_println!("ion-win: delete: {message}");
                skipped += 1;
            }
            DeleteResult::Failed(message) => {
                crate::err_println!("ion-win: delete: {message}");
                failed += 1;
            }
        }
    }
    summary(deleted, skipped, failed, options.permanent)
}

fn validate_then_delete(path: &Path, options: DeleteOptions) -> DeleteResult {
    let link_meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return DeleteResult::Skipped(format!("{}: not found", path.display()));
        }
        Err(e) => return DeleteResult::Failed(format!("{}: {e}", path.display())),
    };

    let target_is_dir = link_meta.is_dir()
        || (is_reparse_point(&link_meta)
            && fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false));

    // Never let a recursive delete target a filesystem root, the shell's
    // current directory, or one of its ancestors. Reparse points are
    // excluded from this canonical-target check because deleting the link
    // itself is safe and must not be confused with deleting its target.
    if !is_reparse_point(&link_meta) && !link_meta.file_type().is_symlink() {
        if let Err(reason) = reject_broad_target(path) {
            return DeleteResult::Failed(reason);
        }
    }
    if target_is_dir && !options.recurse {
        return DeleteResult::Failed(format!(
            "{}: is a directory (use --recurse)",
            path.display()
        ));
    }

    let result = if options.permanent {
        permanent_delete(path, &link_meta)
    } else {
        recycle(path)
    };
    match result {
        Ok(()) => DeleteResult::Deleted,
        Err(e) => DeleteResult::Failed(format!("{}: {e}", path.display())),
    }
}

fn reject_broad_target(path: &Path) -> Result<(), String> {
    let canonical = path
        .canonicalize()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if canonical.parent().is_none() {
        return Err(format!(
            "{}: refusing to delete a filesystem root",
            path.display()
        ));
    }
    if let Ok(current) = std::env::current_dir().and_then(|p| p.canonicalize()) {
        if current == canonical || current.starts_with(&canonical) {
            return Err(format!(
                "{}: refusing to delete the current directory or one of its ancestors",
                path.display()
            ));
        }
    }
    Ok(())
}

fn permanent_delete(path: &Path, meta: &fs::Metadata) -> Result<(), String> {
    if is_reparse_point(meta) || meta.file_type().is_symlink() {
        // Remove the link/junction itself. Never recurse into its target.
        return if fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false) {
            fs::remove_dir(path).map_err(|e| e.to_string())
        } else {
            fs::remove_file(path).map_err(|e| e.to_string())
        };
    }
    if meta.is_dir() {
        for entry in fs::read_dir(path).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let child = entry.path();
            let child_meta = fs::symlink_metadata(&child).map_err(|e| e.to_string())?;
            permanent_delete(&child, &child_meta)?;
        }
        fs::remove_dir(path).map_err(|e| e.to_string())
    } else {
        fs::remove_file(path).map_err(|e| e.to_string())
    }
}

#[cfg(windows)]
fn is_reparse_point(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(meta: &fs::Metadata) -> bool {
    meta.file_type().is_symlink()
}

#[cfg(windows)]
fn recycle(path: &Path) -> Result<(), String> {
    use windows::core::HSTRING;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{
        FileOperation, IFileOperation, IShellItem, SHCreateItemFromParsingName,
        FOFX_RECYCLEONDELETE, FOF_NOCONFIRMATION, FOF_SILENT,
    };

    let absolute: PathBuf = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(path)
    };
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).map_err(|e| e.to_string())?;
        let result = (|| -> Result<(), String> {
            let operation: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)
                .map_err(|e| format!("creating Shell file operation: {e}"))?;
            operation
                .SetOperationFlags(FOFX_RECYCLEONDELETE | FOF_NOCONFIRMATION | FOF_SILENT)
                .map_err(|e| format!("setting recycle-only flags: {e}"))?;
            // Shell namespace parsing expects native separators even though
            // ion-win accepts `/` in user-facing paths.
            let path_string = absolute.to_string_lossy().replace('/', "\\");
            let item: IShellItem =
                SHCreateItemFromParsingName(&HSTRING::from(path_string.as_str()), None)
                    .map_err(|e| format!("opening Shell item '{path_string}': {e}"))?;
            operation
                .DeleteItem(&item, None)
                .map_err(|e| format!("queuing Recycle Bin item: {e}"))?;
            operation
                .PerformOperations()
                .map_err(|e| format!("performing Recycle Bin operation: {e}"))?;
            if operation
                .GetAnyOperationsAborted()
                .map_err(|e| format!("checking Recycle Bin result: {e}"))?
                .as_bool()
            {
                return Err("Recycle Bin operation was aborted".to_string());
            }
            Ok(())
        })();
        CoUninitialize();
        result
    }
}

#[cfg(not(windows))]
fn recycle(_path: &Path) -> Result<(), String> {
    Err("Recycle Bin deletion is only supported on Windows".to_string())
}

fn summary(deleted: usize, skipped: usize, failed: usize, permanent: bool) -> String {
    let action = if permanent {
        "permanently deleted"
    } else {
        "recycled"
    };
    format!("ion-win: delete: {action} {deleted} item(s), skipped {skipped}, failed {failed}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::Table;

    fn temp_dir(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("ion-win-delete-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn permanent_requires_force_and_unknown_flags_are_rejected() {
        let err = parse_flags(&["--permanent".to_string(), "a.txt".to_string()]).unwrap_err();
        assert!(
            err.contains("requires both --permanent and --force"),
            "{err}"
        );
        let err = parse_flags(&["--bogus".to_string()]).unwrap_err();
        assert!(err.contains("unknown option"), "{err}");
    }

    #[tokio::test]
    async fn permanent_file_delete_requires_explicit_pair_and_deletes() {
        let dir = temp_dir("file");
        let file = dir.join("gone.txt");
        fs::write(&file, "data").unwrap();
        let result = delete_paths(
            &[file.to_string_lossy().into_owned()],
            DeleteOptions {
                permanent: true,
                force: true,
                recurse: false,
            },
        )
        .await;
        assert_eq!(
            result,
            "ion-win: delete: permanently deleted 1 item(s), skipped 0, failed 0"
        );
        assert!(!file.exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn directory_requires_recurse_then_deletes_tree() {
        let dir = temp_dir("directory");
        let tree = dir.join("tree");
        fs::create_dir_all(tree.join("nested")).unwrap();
        fs::write(tree.join("nested").join("file.txt"), "data").unwrap();
        let path = tree.to_string_lossy().into_owned();
        let refused = delete_paths(
            std::slice::from_ref(&path),
            DeleteOptions {
                permanent: true,
                force: true,
                recurse: false,
            },
        )
        .await;
        assert_eq!(
            refused,
            "ion-win: delete: permanently deleted 0 item(s), skipped 0, failed 1"
        );
        assert!(tree.exists());
        let deleted = delete_paths(
            &[path],
            DeleteOptions {
                permanent: true,
                force: true,
                recurse: true,
            },
        )
        .await;
        assert_eq!(
            deleted,
            "ion-win: delete: permanently deleted 1 item(s), skipped 0, failed 0"
        );
        assert!(!tree.exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn missing_paths_and_table_rows_are_counted_as_skipped() {
        let dir = temp_dir("skips");
        let missing = dir.join("missing.txt");
        let table = Table {
            rows: vec![
                vec![("path".to_string(), missing.to_string_lossy().into_owned())],
                vec![("name".to_string(), "no-path".to_string())],
            ],
        };
        let result = delete_table(
            &table,
            DeleteOptions {
                permanent: true,
                force: true,
                recurse: false,
            },
        )
        .await;
        assert_eq!(
            result,
            "ion-win: delete: permanently deleted 0 item(s), skipped 2, failed 0"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn current_directory_and_filesystem_root_are_rejected() {
        let current = std::env::current_dir().unwrap();
        assert!(reject_broad_target(&current).is_err());
        let root = current.ancestors().last().unwrap();
        assert!(reject_broad_target(root).is_err());
    }
}
