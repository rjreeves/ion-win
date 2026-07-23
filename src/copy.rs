//! `copy`/`cp` (`ARCHITECTURE.md` §24): the first of ion-win's
//! file-operation builtins that act on a manifest — copying is the
//! deliberately-chosen starting point, since (unlike the later `delete`
//! builtin) it can't destroy the source. Works two ways: a plain multi-file copy
//! (matching real `cp`/PowerShell's `Copy-Item`), and as a pipeline stage
//! consuming a `Table`'s `path` column directly — the same column name
//! `stat` (§21) already produces — so `manifest | where size -lt 1000000
//! | copy backup/` needs no separate scalar-extraction step at all.
//!
//! Both forms refuse to overwrite an existing destination file unless
//! `--force`/`-f` is given (safer than real `cp`'s silent-overwrite
//! default — a deliberate choice, not an oversight, since this is
//! ion-win's own extension with no manual precedent to match), and skip a
//! source that fails to copy with a printed warning rather than aborting
//! the whole batch, the same "batch operation, don't let one bad spot
//! ruin the rest" reasoning `stat`/`find` already use.
//!
//! Copying multiple files runs concurrently across ion-win's existing
//! tokio runtime, the same `spawn_blocking`-per-item pattern `stat.rs`
//! (§21) established for hashing: one blocking task per file, spawned in
//! order and awaited in that same order, so wall-clock time for a batch
//! is bounded by the slowest single copy rather than the sum of all of
//! them, while the printed copied/skipped tally stays deterministic
//! regardless of which task actually finishes first.

use std::fs;
use std::path::{Component, Path, PathBuf};

const COPY_USAGE: &str =
    "copy: usage: copy [--force] SRC... DEST  |  TABLE | copy [--force] DEST";

/// Splits `--force`/`-f` and `--help`/`-h` out of `args`, returning
/// (force, remaining positional arguments). Shared between `copy`'s
/// explicit-file-arguments form and its `Table`-consuming pipeline form,
/// since both accept the same flags.
pub fn parse_flags(args: &[String]) -> Result<(bool, Vec<String>), String> {
    let mut force = false;
    let mut positional = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--force" | "-f" => force = true,
            "--help" | "-h" => return Err(COPY_USAGE.to_string()),
            _ if arg.starts_with("--") => return Err(format!("copy: unknown option: {arg}")),
            _ => positional.push(arg.clone()),
        }
    }
    Ok((force, positional))
}

/// The explicit-arguments form, used standalone (`copy a.txt b.txt`):
/// parses flags, requires at least one source plus a destination, then
/// copies.
pub async fn parse_and_copy_files(args: &[String]) -> Result<String, String> {
    let (force, mut positional) = parse_flags(args)?;
    if positional.len() < 2 {
        return Err(COPY_USAGE.to_string());
    }
    let dest = positional.pop().expect("checked len >= 2 above");
    Ok(copy_files(&positional, &dest, force).await)
}

/// Copies each of `sources` into `dest`. `dest` is treated as a directory
/// — each source copied in by its own basename — when it already exists
/// as one, when there's more than one source, or when it ends in a path
/// separator (an explicit "this is a directory" signal even if it
/// doesn't exist yet); otherwise `dest` is the exact target file path for
/// the single source (a rename-style copy). Each source's copy runs on
/// its own blocking task, concurrently with the others.
pub async fn copy_files(sources: &[String], dest: &str, force: bool) -> String {
    let dest_path = Path::new(dest);
    let dest_is_dir =
        dest_path.is_dir() || sources.len() > 1 || dest.ends_with('/') || dest.ends_with('\\');

    let mut handles = Vec::with_capacity(sources.len());
    for src in sources {
        let target = if dest_is_dir {
            let name = Path::new(src)
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(src));
            dest_path.join(name)
        } else {
            dest_path.to_path_buf()
        };
        let src = src.clone();
        handles.push(tokio::task::spawn_blocking(move || copy_one(&src, &target, force)));
    }
    await_copies(handles, 0).await
}

/// The `Table`-consuming pipeline form (`TABLE | copy DEST`): copies
/// every row's `path` column into `dest`, preserving each row's full
/// relative path underneath it rather than flattening to a bare
/// filename — unlike `copy_files`, this is meant for a recursive
/// manifest (`find --recurse | stat`), where multiple files can share a
/// basename in different subdirectories; flattening them would silently
/// collide. Like `copy_files`, every row's copy runs on its own blocking
/// task concurrently — the real payoff case, since a manifest can easily
/// list thousands of files.
pub async fn copy_table(table: &crate::table::Table, dest: &str, force: bool) -> String {
    let dest_path = Path::new(dest);
    let mut handles = Vec::with_capacity(table.rows.len());
    let mut skipped = 0usize;
    for row in &table.rows {
        let Some((_, path)) = row.iter().find(|(k, _)| k == "path") else {
            crate::err_println!("ion-win: copy: row has no 'path' column");
            skipped += 1;
            continue;
        };
        let target = table_row_target(dest_path, path);
        let path = path.clone();
        handles.push(tokio::task::spawn_blocking(move || copy_one(&path, &target, force)));
    }
    // `skipped` seeds the tally with rows that had no `path` column at
    // all, so they're never spawned as a task in the first place.
    await_copies(handles, skipped).await
}

/// Awaits every blocking copy task in the order it was spawned (each
/// already runs concurrently on tokio's blocking thread pool regardless
/// of await order) and tallies the result on top of `skipped`, an initial
/// count of items that never became a task at all — shared by both
/// `copy_files` and `copy_table`.
async fn await_copies(
    handles: Vec<tokio::task::JoinHandle<Result<(), String>>>,
    mut skipped: usize,
) -> String {
    let mut copied = 0usize;
    for handle in handles {
        match handle.await {
            Ok(Ok(())) => copied += 1,
            Ok(Err(e)) => {
                crate::err_println!("ion-win: copy: {e}");
                skipped += 1;
            }
            Err(e) => {
                crate::err_println!("ion-win: copy: task failed: {e}");
                skipped += 1;
            }
        }
    }
    summary(copied, skipped)
}

/// Computes where a table row's `path` column lands under `dest`,
/// stripping any Windows drive-prefix/root-directory component first so
/// joining always concatenates — `Path::join` otherwise *replaces* the
/// base entirely when its argument is absolute, which would silently
/// write a file right back to its own original absolute location (or,
/// since that already exists, just get skipped as a spurious
/// "destination already exists"). `C:\Users\Bob\data.txt` becomes
/// `Users\Bob\data.txt` before being joined onto `dest`, landing at
/// `dest\Users\Bob\data.txt` instead. A no-op for the already-relative
/// paths `find`/`stat` normally produce. Split out from `copy_table` as
/// its own pure function (no file I/O) so it's testable without touching
/// the process's actual current directory.
fn table_row_target(dest: &Path, path: &str) -> PathBuf {
    let stripped: PathBuf = Path::new(path)
        .components()
        .filter(|c| !matches!(c, Component::Prefix(_) | Component::RootDir))
        .collect();
    dest.join(stripped)
}

fn summary(copied: usize, skipped: usize) -> String {
    if skipped > 0 {
        format!("ion-win: copy: copied {copied} file(s), skipped {skipped}")
    } else {
        format!("ion-win: copy: copied {copied} file(s)")
    }
}

fn copy_one(src: &str, dest: &Path, force: bool) -> Result<(), String> {
    if dest.exists() && !force {
        return Err(format!(
            "{}: destination already exists (use --force to overwrite)",
            dest.display()
        ));
    }
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| format!("{src}: {e}"))?;
        }
    }
    fs::copy(src, dest).map_err(|e| format!("{src}: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::Table;
    use std::io::Write;

    fn temp_dir(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("ion-win-copy-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = fs::File::create(path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
    }

    #[tokio::test]
    async fn copy_files_single_source_to_exact_destination_path() {
        let dir = temp_dir("single");
        let src = dir.join("a.txt");
        write_file(&src, "hello");
        let dest = dir.join("renamed.txt");

        let result =
            copy_files(&[src.to_string_lossy().into_owned()], &dest.to_string_lossy(), false).await;
        assert_eq!(result, "ion-win: copy: copied 1 file(s)");
        assert_eq!(fs::read_to_string(&dest).unwrap(), "hello");

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn copy_files_multiple_sources_go_into_a_directory_by_basename() {
        let dir = temp_dir("multi");
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        write_file(&a, "A");
        write_file(&b, "B");
        let out = dir.join("out");

        let result = copy_files(
            &[a.to_string_lossy().into_owned(), b.to_string_lossy().into_owned()],
            &out.to_string_lossy(),
            false,
        )
        .await;
        assert_eq!(result, "ion-win: copy: copied 2 file(s)");
        assert_eq!(fs::read_to_string(out.join("a.txt")).unwrap(), "A");
        assert_eq!(fs::read_to_string(out.join("b.txt")).unwrap(), "B");

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn copy_files_refuses_to_overwrite_without_force() {
        let dir = temp_dir("overwrite");
        let src = dir.join("a.txt");
        let dest = dir.join("b.txt");
        write_file(&src, "new");
        write_file(&dest, "old");

        let result =
            copy_files(&[src.to_string_lossy().into_owned()], &dest.to_string_lossy(), false).await;
        assert_eq!(result, "ion-win: copy: copied 0 file(s), skipped 1");
        assert_eq!(fs::read_to_string(&dest).unwrap(), "old", "must not have been overwritten");

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn copy_files_overwrites_when_force_is_given() {
        let dir = temp_dir("force");
        let src = dir.join("a.txt");
        let dest = dir.join("b.txt");
        write_file(&src, "new");
        write_file(&dest, "old");

        let result =
            copy_files(&[src.to_string_lossy().into_owned()], &dest.to_string_lossy(), true).await;
        assert_eq!(result, "ion-win: copy: copied 1 file(s)");
        assert_eq!(fs::read_to_string(&dest).unwrap(), "new");

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn copy_files_skips_a_missing_source_and_continues() {
        let dir = temp_dir("missing-source");
        let a = dir.join("a.txt");
        write_file(&a, "A");
        let missing = dir.join("does-not-exist.txt");
        let out = dir.join("out");

        let result = copy_files(
            &[missing.to_string_lossy().into_owned(), a.to_string_lossy().into_owned()],
            &out.to_string_lossy(),
            false,
        )
        .await;
        assert_eq!(result, "ion-win: copy: copied 1 file(s), skipped 1");
        assert_eq!(fs::read_to_string(out.join("a.txt")).unwrap(), "A");

        let _ = fs::remove_dir_all(dir);
    }

    /// `find`/`stat` normally produce already-relative paths (`sub/
    /// nested.txt`), so this is the common case: no stripping needed,
    /// `dest.join(path)` concatenates directly. A pure test of
    /// `table_row_target` itself — no file I/O, no dependence on (and no
    /// mutation of) the process's actual current directory, unlike
    /// `copy_table`'s own end-to-end behavior, which this project's
    /// testing philosophy keeps out of in-process `#[test]`s.
    #[test]
    fn table_row_target_concatenates_a_relative_path_directly() {
        let dest = Path::new("dest");
        assert_eq!(table_row_target(dest, "top.txt"), dest.join("top.txt"));
        assert_eq!(
            table_row_target(dest, "sub/nested.txt"),
            dest.join("sub").join("nested.txt")
        );
    }

    /// A `path` column that happens to be absolute (not what `find`/`stat`
    /// normally produce, but nothing stops a table from having one) must
    /// still land *under* `dest`, not get silently written back to its
    /// own original location — the real bug `table_row_target` exists to
    /// fix, caught by this exact test failing before the fix was applied.
    #[test]
    fn table_row_target_strips_the_root_from_an_absolute_path() {
        let dest = Path::new("dest");
        let absolute = if cfg!(windows) { r"C:\Users\Bob\data.txt" } else { "/home/bob/data.txt" };
        let target = table_row_target(dest, absolute);
        assert!(
            target.starts_with(dest),
            "expected {target:?} to land under {dest:?}, not replace it"
        );
        assert!(!target.is_absolute(), "expected {target:?} to no longer be absolute");
    }

    /// End-to-end confirmation via real file I/O with absolute source
    /// paths (already necessarily absolute, since they're built from a
    /// real temp directory) — the fuller version of the pure test above.
    #[tokio::test]
    async fn copy_table_strips_the_root_from_an_absolute_path_column() {
        let dir = temp_dir("table-absolute");
        let src_root = dir.join("src");
        write_file(&src_root.join("top.txt"), "top");
        write_file(&src_root.join("sub").join("nested.txt"), "nested");
        let dest_root = dir.join("dest");

        let table = Table {
            rows: vec![
                vec![("path".to_string(), src_root.join("top.txt").to_string_lossy().into_owned())],
                vec![(
                    "path".to_string(),
                    src_root.join("sub").join("nested.txt").to_string_lossy().into_owned(),
                )],
            ],
        };
        let result = copy_table(&table, &dest_root.to_string_lossy(), false).await;
        assert_eq!(result, "ion-win: copy: copied 2 file(s)");
        assert_eq!(
            fs::read_to_string(dest_root.join(src_root.join("top.txt"))).unwrap(),
            "top"
        );
        assert_eq!(
            fs::read_to_string(dest_root.join(src_root.join("sub").join("nested.txt"))).unwrap(),
            "nested"
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn copy_table_skips_a_row_with_no_path_column() {
        let dir = temp_dir("no-path-column");
        let table = Table {
            rows: vec![vec![("name".to_string(), "no-path-here".to_string())]],
        };
        let result = copy_table(&table, &dir.join("dest").to_string_lossy(), false).await;
        assert_eq!(result, "ion-win: copy: copied 0 file(s), skipped 1");
        let _ = fs::remove_dir_all(dir);
    }

    /// A manifest with both a good row and a no-`path`-column row: proves
    /// the pre-spawn `skipped` count (rows rejected before any task
    /// exists) and the post-await tally from real copy tasks add up
    /// together correctly, not just each in isolation.
    #[tokio::test]
    async fn copy_table_tallies_pre_spawn_skips_alongside_real_copies() {
        let dir = temp_dir("mixed-skip");
        let src_root = dir.join("src");
        write_file(&src_root.join("ok.txt"), "ok");
        let dest_root = dir.join("dest");

        let table = Table {
            rows: vec![
                vec![("path".to_string(), src_root.join("ok.txt").to_string_lossy().into_owned())],
                vec![("name".to_string(), "no-path-here".to_string())],
            ],
        };
        let result = copy_table(&table, &dest_root.to_string_lossy(), false).await;
        assert_eq!(result, "ion-win: copy: copied 1 file(s), skipped 1");

        let _ = fs::remove_dir_all(dir);
    }

    /// The actual motivation for this change: copying many files at once
    /// finishes even though each individual copy is spawned as its own
    /// blocking task, and every file genuinely lands at its destination —
    /// concurrency doesn't drop or corrupt any of them.
    #[tokio::test]
    async fn copy_files_concurrently_copies_many_files_correctly() {
        let dir = temp_dir("concurrent");
        let mut sources = Vec::new();
        for i in 0..32 {
            let src = dir.join(format!("in-{i}.txt"));
            write_file(&src, &format!("contents-{i}"));
            sources.push(src.to_string_lossy().into_owned());
        }
        let out = dir.join("out");

        let result = copy_files(&sources, &out.to_string_lossy(), false).await;
        assert_eq!(result, "ion-win: copy: copied 32 file(s)");
        for i in 0..32 {
            assert_eq!(
                fs::read_to_string(out.join(format!("in-{i}.txt"))).unwrap(),
                format!("contents-{i}")
            );
        }

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn parse_flags_rejects_unknown_flag() {
        let err = parse_flags(&["--bogus".to_string()]).unwrap_err();
        assert!(err.contains("unknown option"), "{err}");
    }

    #[tokio::test]
    async fn parse_and_copy_files_requires_at_least_one_source_and_a_destination() {
        let err = parse_and_copy_files(&["only-one-arg".to_string()]).await.unwrap_err();
        assert!(err.contains("usage"), "{err}");
    }
}
