//! `compress` (`ARCHITECTURE.md` §25): ion-win's second manifest-driven
//! file-operation builtin, after `copy` (§24). Always produces a plain
//! standard `.zip` archive — the same format WinZip, 7-Zip, and Windows
//! Explorer's own "Extract All" all read natively — rather than offering
//! a `--format` flag: real Ion has no `compress` builtin at all to take a
//! cue from, and a single well-known default beats an unused choice
//! nobody asked for. Works two ways, mirroring `copy` exactly: an
//! explicit multi-file form (`compress a.txt b.txt out.zip`) and a
//! `Table`-consuming pipeline form (`TABLE | compress out.zip`) that
//! reads the same `path` column `stat` (§21) already produces.
//!
//! **Made concurrent the same way as `copy`, but for a real reason
//! specific to this builtin**: unlike a file copy, DEFLATE compression is
//! genuinely CPU-bound, so compressing many files one at a time on a
//! single thread would waste every core but one. Each file is
//! independently compressed, in parallel, into its own temporary one-entry
//! `.zip` on disk (via `tokio::task::spawn_blocking`) — a real, self-contained DEFLATE
//! stream, since separate zip entries never share compression state.
//! Splicing those entries into the one final archive still has to happen
//! on a single thread, sequentially, because the zip format's central
//! directory can only be assembled by one writer making one pass — but
//! that final step is cheap: `ZipWriter::raw_copy_file` copies each
//! entry's already-compressed bytes across without re-running DEFLATE,
//! so the actual expensive work (compression) is what runs in parallel,
//! not the bookkeeping.

use crate::fileset::FileSet;
use crate::table::Table;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

const COMPRESS_USAGE: &str =
    "compress: usage: compress [--force] SRC... DEST.zip  |  FILESET | compress [--force] DEST.zip  |  FILESET | compress [--force] --per-root DIRECTORY";

/// Splits `--force`/`-f` and `--help`/`-h` out of `args` — identical
/// shape to `copy::parse_flags`, kept as its own copy rather than shared
/// since the two commands' usage text differs and there's nothing else
/// to factor out of two four-line functions.
pub fn parse_flags(args: &[String]) -> Result<(bool, Vec<String>), String> {
    let mut force = false;
    let mut positional = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--force" | "-f" => force = true,
            "--help" | "-h" => return Err(COMPRESS_USAGE.to_string()),
            _ if arg.starts_with("--") => return Err(format!("compress: unknown option: {arg}")),
            _ => positional.push(arg.clone()),
        }
    }
    Ok((force, positional))
}

/// The explicit-arguments form, used standalone (`compress a.txt b.txt out.zip`).
pub async fn parse_and_compress_files(args: &[String]) -> Result<String, String> {
    let (force, mut positional) = parse_flags(args)?;
    if positional.len() < 2 {
        return Err(COMPRESS_USAGE.to_string());
    }
    let dest = positional.pop().expect("checked len >= 2 above");
    Ok(compress_files(&positional, &dest, force).await)
}

/// Compresses each of `sources` into a single new archive at `dest`,
/// stored by basename (a flat archive — matching `copy_files`' own
/// basename-based behavior for its explicit-args form).
pub async fn compress_files(sources: &[String], dest: &str, force: bool) -> String {
    let entries = sources
        .iter()
        .map(|src| {
            let name = Path::new(src)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| src.clone());
            (name, PathBuf::from(src))
        })
        .collect();
    compress_entries(entries, dest, force, 0).await
}

/// Pipeline-only extension of `parse_flags`. `--per-root` needs FileSet
/// provenance, so the standalone explicit-source form intentionally keeps
/// rejecting it.
pub fn parse_pipeline_flags(
    args: &[String],
) -> Result<(bool, bool, bool, bool, Option<String>, Vec<String>), String> {
    let mut per_root = false;
    let mut plan = false;
    let mut apply = false;
    let mut backup = None;
    let mut remaining = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--per-root" => per_root = true,
            "--plan" => plan = true,
            "--apply" => apply = true,
            "--backup" => {
                backup = Some(
                    iter.next()
                        .ok_or_else(|| "compress: --backup requires a directory".to_string())?
                        .clone(),
                );
            }
            _ => remaining.push(arg.clone()),
        }
    }
    if plan && apply {
        return Err("compress: --plan and --apply are mutually exclusive".to_string());
    }
    if (plan || apply || backup.is_some()) && !per_root {
        return Err("compress: --plan/--apply/--backup require --per-root".to_string());
    }
    let (force, positional) = parse_flags(&remaining)?;
    Ok((force, per_root, plan, apply, backup, positional))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivePlanItem {
    pub root: PathBuf,
    pub archive: PathBuf,
    pub backup: Option<PathBuf>,
    entries: Vec<ArchivePlanEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivePlanEntry {
    pub archive_name: String,
    pub source: crate::fileset::FileRecord,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivePlan {
    pub items: Vec<ArchivePlanItem>,
}

impl ArchivePlan {
    pub fn to_table(&self) -> Table {
        Table {
            rows: self
                .items
                .iter()
                .map(|item| {
                    vec![
                        ("operation".to_string(), "compress".to_string()),
                        ("root".to_string(), item.root.to_string_lossy().into_owned()),
                        ("file_count".to_string(), item.entries.len().to_string()),
                        (
                            "archive".to_string(),
                            item.archive.to_string_lossy().into_owned(),
                        ),
                        (
                            "backup".to_string(),
                            item.backup
                                .as_ref()
                                .map(|path| path.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                        ),
                        (
                            "archive_exists".to_string(),
                            item.archive.exists().to_string(),
                        ),
                        (
                            "backup_exists".to_string(),
                            item.backup
                                .as_ref()
                                .is_some_and(|path| path.exists())
                                .to_string(),
                        ),
                    ]
                })
                .collect(),
        }
    }
}

/// Native FileSet pipeline form. It reads each record's `PathBuf` directly,
/// preserving typed filesystem identity and avoiding the legacy display-table
/// adapter used by `compress_table`.
pub async fn compress_fileset(fileset: &FileSet, dest: &str, force: bool) -> String {
    let entries = fileset
        .files
        .iter()
        .map(|record| (path_entry_name(&record.path), record.path.clone()))
        .collect();
    compress_entries(entries, dest, force, 0).await
}

/// Creates one archive per provenance root. Archive names are derived from
/// each root's final path component and entries are relative to that root,
/// rather than embedding an absolute Windows path inside the ZIP.
pub async fn compress_fileset_per_root(
    fileset: &FileSet,
    destination_directory: &str,
    force: bool,
) -> Result<String, String> {
    let plan = plan_fileset_per_root(fileset, destination_directory, None)?;
    apply_archive_plan(&plan, force).await
}

pub fn plan_fileset_per_root(
    fileset: &FileSet,
    destination_directory: &str,
    backup_directory: Option<&str>,
) -> Result<ArchivePlan, String> {
    if fileset.provenance.roots.is_empty() {
        return Err(
            "compress: --per-root requires root provenance (pipe directories through 'find')"
                .to_string(),
        );
    }

    let destination = Path::new(destination_directory);
    if destination.exists() && !destination.is_dir() {
        return Err(format!(
            "compress: --per-root destination is not a directory: {}",
            destination.display()
        ));
    }
    let backup_destination = backup_directory.map(Path::new);
    if let Some(backup) = backup_destination {
        if backup.exists() && !backup.is_dir() {
            return Err(format!(
                "compress: backup destination is not a directory: {}",
                backup.display()
            ));
        }
    }

    let mut archive_names = HashSet::new();
    let mut groups = Vec::with_capacity(fileset.provenance.roots.len());
    for root in &fileset.provenance.roots {
        let name = root
            .file_name()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| format!("compress: cannot derive an archive name from {}", root.display()))?
            .to_string_lossy()
            .into_owned();
        let archive_name = format!("{name}.zip");
        if !archive_names.insert(archive_name.to_lowercase()) {
            return Err(format!(
                "compress: multiple roots would create the same archive name: {archive_name}"
            ));
        }
        let archive = destination.join(&archive_name);
        let backup = backup_destination.map(|directory| directory.join(&archive_name));
        groups.push(ArchivePlanItem {
            root: root.clone(),
            archive,
            backup,
            entries: Vec::new(),
        });
    }

    for record in &fileset.files {
        let Some((index, root)) = fileset
            .provenance
            .roots
            .iter()
            .enumerate()
            .filter(|(_, root)| record.path.starts_with(root))
            .max_by_key(|(_, root)| root.components().count())
        else {
            return Err(format!(
                "compress: file is outside every provenance root: {}",
                record.path.display()
            ));
        };
        let relative = record.path.strip_prefix(root).expect("matched root prefix");
        groups[index]
            .entries
            .push(ArchivePlanEntry {
                archive_name: path_entry_name(relative),
                source: record.clone(),
            });
    }

    Ok(ArchivePlan { items: groups })
}

pub async fn apply_archive_plan(plan: &ArchivePlan, force: bool) -> Result<String, String> {
    validate_archive_plan(plan)?;
    if !force {
        for item in &plan.items {
            if item.archive.exists() {
                return Err(format!(
                    "compress: {}: destination already exists (use --force to overwrite)",
                    item.archive.display()
                ));
            }
            if let Some(backup) = &item.backup {
                if backup.exists() {
                    return Err(format!(
                        "compress: {}: backup already exists (use --force to overwrite)",
                        backup.display()
                    ));
                }
            }
        }
    }

    let mut summaries = Vec::with_capacity(plan.items.len());
    for item in &plan.items {
        let result = compress_entries(
            item.entries
                .iter()
                .map(|entry| (entry.archive_name.clone(), entry.source.path.clone()))
                .collect(),
            &item.archive.to_string_lossy(),
            force,
            0,
        )
        .await;
        if let Some(backup) = &item.backup {
            if let Some(parent) = backup.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    format!("compress: could not create {}: {error}", parent.display())
                })?;
            }
            fs::copy(&item.archive, backup).map_err(|error| {
                format!(
                    "compress: could not back up {} to {}: {error}",
                    item.archive.display(),
                    backup.display()
                )
            })?;
        }
        summaries.push(format!(
            "{} -> {}{}: {result}",
            item.root.display(),
            item.archive.display(),
            item.backup
                .as_ref()
                .map(|backup| format!(" -> {}", backup.display()))
                .unwrap_or_default()
        ));
    }
    Ok(summaries.join("\n"))
}

/// Builds one planned archive at a caller-provided staging path and verifies
/// that the resulting ZIP contains every planned entry. The operation layer
/// publishes this file with an atomic rename only after this succeeds.
pub(crate) async fn build_planned_archive(
    item: &ArchivePlanItem,
    staging: &Path,
) -> Result<String, String> {
    if staging.exists() {
        fs::remove_file(staging).map_err(|error| {
            format!("compress: could not clear staging file {}: {error}", staging.display())
        })?;
    }
    let result = compress_entries(
        item.entries
            .iter()
            .map(|entry| (entry.archive_name.clone(), entry.source.path.clone()))
            .collect(),
        &staging.to_string_lossy(),
        false,
        0,
    )
    .await;
    let verification = (|| -> Result<(), String> {
        let file = fs::File::open(staging).map_err(|error| {
            format!("compress: staging archive was not created: {}: {error}", staging.display())
        })?;
        let archive = ZipArchive::new(file).map_err(|error| {
            format!("compress: staging archive is invalid: {}: {error}", staging.display())
        })?;
        if archive.len() != item.entries.len() {
            return Err(format!(
                "compress: staging archive {} contains {} of {} planned entries",
                staging.display(), archive.len(), item.entries.len()
            ));
        }
        Ok(())
    })();
    if let Err(error) = verification {
        let _ = fs::remove_file(staging);
        return Err(error);
    }
    Ok(result)
}

/// Reopens every planned source before any destination is created. Windows
/// handle identity detects replacement at the same path; size and modified
/// time detect content drift while preserving a clear cross-platform check.
pub fn validate_archive_plan(plan: &ArchivePlan) -> Result<(), String> {
    for item in &plan.items {
        for entry in &item.entries {
            let expected = &entry.source;
            let current = crate::fileset::FileRecord::from_path(expected.path.clone(), None)
                .map_err(|error| {
                    format!(
                        "apply: source drift: {} is unavailable: {error}",
                        expected.path.display()
                    )
                })?;
            if expected.identity != current.identity {
                return Err(format!(
                    "apply: source drift: file identity changed: {}",
                    expected.path.display()
                ));
            }
            if expected.kind != current.kind
                || expected.size != current.size
                || expected.modified != current.modified
            {
                return Err(format!(
                    "apply: source drift: metadata changed: {}",
                    expected.path.display()
                ));
            }
        }
    }
    Ok(())
}

/// The `Table`-consuming pipeline form (`TABLE | compress DEST.zip`):
/// compresses every row's `path` column into `dest`, storing each file
/// under its relative path *inside* the archive (so extracting the
/// result reconstructs the original directory structure) rather than
/// flattening to a bare basename — the same reasoning `copy_table` uses
/// for its destination paths on disk, applied here to an in-archive name.
pub async fn compress_table(table: &Table, dest: &str, force: bool) -> String {
    let mut entries = Vec::with_capacity(table.rows.len());
    let mut skipped = 0usize;
    for row in &table.rows {
        let Some((_, path)) = row.iter().find(|(k, _)| k == "path") else {
            crate::err_println!("ion-win: compress: row has no 'path' column");
            skipped += 1;
            continue;
        };
        entries.push((table_row_entry_name(path), PathBuf::from(path)));
    }
    compress_entries(entries, dest, force, skipped).await
}

/// Converts a table row's `path` column into the name stored *inside*
/// the archive. Zip entry names are conventionally POSIX-style ('/'
/// separators, no drive letter or root) regardless of host OS, so any
/// `Component::Prefix`/`RootDir` is dropped — a no-op for the
/// already-relative paths `find`/`stat` normally produce, but keeps an
/// absolute path from writing a nonsensical `C:\...` entry name into the
/// archive. `Component::ParentDir` (`..`) is dropped too rather than
/// specially handled: `find`/`stat` never produce one, so this only
/// matters for a hand-built table with a deliberately unusual `path`
/// value, a deliberately unhandled edge case rather than an oversight.
fn table_row_entry_name(path: &str) -> String {
    path_entry_name(Path::new(path))
}

fn path_entry_name(path: &Path) -> String {
    path
        .components()
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Shared by both public entry points: compresses every `(entry_name,
/// src_path)` pair concurrently into its own temporary mini-archive, then
/// splices all of them into one real archive file at `dest` sequentially.
/// `skipped` seeds the tally with anything already rejected before this
/// point (e.g. `compress_table`'s no-`path`-column rows), so it's folded
/// into the final count rather than needing a second counter threaded
/// back out — the same shape `copy.rs`'s `await_copies` uses.
async fn compress_entries(
    entries: Vec<(String, PathBuf)>,
    dest: &str,
    force: bool,
    mut skipped: usize,
) -> String {
    let dest_path = Path::new(dest);
    if dest_path.exists() && !force {
        return format!(
            "ion-win: compress: {}: destination already exists (use --force to overwrite)",
            dest_path.display()
        );
    }

    if let Some(parent) = dest_path.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = fs::create_dir_all(parent) {
                return format!("ion-win: compress: {dest}: {e}");
            }
        }
    }
    let file = match fs::File::create(dest_path) {
        Ok(f) => f,
        Err(e) => return format!("ion-win: compress: {dest}: {e}"),
    };

    let mut writer = ZipWriter::new(file);
    let mut compressed = 0usize;

    // Keep only one batch per logical CPU active. Each worker streams its
    // input into a temporary one-entry ZIP on disk, so memory use is bounded
    // by the ZIP library's buffers rather than the combined size of all files.
    let concurrency = std::thread::available_parallelism().map_or(1, usize::from);
    let temp_dir = compression_temp_dir();
    if let Err(e) = fs::create_dir_all(&temp_dir) {
        drop(writer);
        let _ = fs::remove_file(dest_path);
        return format!("ion-win: compress: could not create temporary directory: {e}");
    }

    let mut pending = entries.into_iter();
    let mut interrupted = false;
    loop {
        if crate::jobctl::interrupt_requested() {
            interrupted = true;
            break;
        }

        let mut handles = Vec::with_capacity(concurrency);
        for _ in 0..concurrency {
            let Some((name, src)) = pending.next() else {
                break;
            };
            let mini_path = temp_dir.join(format!("{}.zip", next_temp_id()));
            handles.push(tokio::task::spawn_blocking(move || {
                let result = compress_one(&src, &name, &mini_path);
                (mini_path, result)
            }));
        }
        if handles.is_empty() {
            break;
        }

        for handle in handles {
            let (mini_path, result) = match handle.await {
                Ok(value) => value,
                Err(e) => {
                    crate::err_println!("ion-win: compress: task failed: {e}");
                    skipped += 1;
                    continue;
                }
            };
            match result {
                Ok(()) if !crate::jobctl::interrupt_requested() => {
                    match splice_entry(&mut writer, &mini_path) {
                        Ok(()) => compressed += 1,
                        Err(e) => {
                            crate::err_println!("ion-win: compress: {e}");
                            skipped += 1;
                        }
                    }
                }
                Ok(()) => interrupted = true,
                Err(e) if e == "interrupted" => interrupted = true,
                Err(e) => {
                    crate::err_println!("ion-win: compress: {e}");
                    skipped += 1;
                }
            }
            let _ = fs::remove_file(&mini_path);
        }
        if interrupted {
            break;
        }
    }

    if interrupted {
        drop(writer);
        let _ = fs::remove_file(dest_path);
        let _ = fs::remove_dir_all(&temp_dir);
        let _ = crate::jobctl::take_interrupt();
        return "ion-win: compress: interrupted".to_string();
    }

    if let Err(e) = writer.finish() {
        let _ = fs::remove_dir_all(&temp_dir);
        return format!("ion-win: compress: {dest}: {e}");
    }
    let _ = fs::remove_dir_all(&temp_dir);
    summary(compressed, skipped)
}

/// Runs on a blocking task: streams `src` into a complete one-entry ZIP in
/// temporary storage. It polls Ctrl+C between chunks, keeping both memory use
/// and interrupt latency bounded independently of the source file's size.
fn compress_one(src: &Path, name: &str, mini_path: &Path) -> Result<(), String> {
    let mut input = fs::File::open(src).map_err(|e| format!("{}: {e}", src.display()))?;
    let output = fs::File::create(mini_path).map_err(|e| format!("{}: {e}", src.display()))?;
    let mut mini = ZipWriter::new(output);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    mini.start_file(name, options)
        .map_err(|e| format!("{}: {e}", src.display()))?;

    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        if crate::jobctl::interrupt_requested() {
            return Err("interrupted".to_string());
        }
        let count = input
            .read(&mut buffer)
            .map_err(|e| format!("{}: {e}", src.display()))?;
        if count == 0 {
            break;
        }
        mini.write_all(&buffer[..count])
            .map_err(|e| format!("{}: {e}", src.display()))?;
    }
    mini.finish()
        .map_err(|e| format!("{}: {e}", src.display()))?;
    Ok(())
}

/// Reads a one-entry mini-archive back (as built by `compress_one`) and
/// copies its single entry into `writer` via `raw_copy_file` — the part
/// of the `zip` crate's public API that lets an already-compressed entry
/// be added to a different archive without decompressing and
/// recompressing it.
fn splice_entry(writer: &mut ZipWriter<fs::File>, mini_path: &Path) -> Result<(), String> {
    let mini = fs::File::open(mini_path).map_err(|e| e.to_string())?;
    let mut archive = ZipArchive::new(mini).map_err(|e| e.to_string())?;
    let file = archive.by_index(0).map_err(|e| e.to_string())?;
    writer.raw_copy_file(file).map_err(|e| e.to_string())
}

static TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn next_temp_id() -> u64 {
    TEMP_ID.fetch_add(1, Ordering::Relaxed)
}

fn compression_temp_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "ion-win-compress-{}-{}",
        std::process::id(),
        next_temp_id()
    ))
}

fn summary(compressed: usize, skipped: usize) -> String {
    if skipped > 0 {
        format!("ion-win: compress: compressed {compressed} file(s), skipped {skipped}")
    } else {
        format!("ion-win: compress: compressed {compressed} file(s)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fileset::{FileRecord, FileSet};
    use std::io::Read;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("ion-win-compress-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    /// Reads every entry name + contents back out of a real `.zip` file
    /// on disk, sorted by name, for assertions that don't care about
    /// archive-internal ordering.
    fn read_archive(path: &Path) -> Vec<(String, String)> {
        let file = fs::File::open(path).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        let mut out = Vec::new();
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).unwrap();
            let name = entry.name().to_string();
            let mut contents = String::new();
            entry.read_to_string(&mut contents).unwrap();
            out.push((name, contents));
        }
        out.sort();
        out
    }

    #[tokio::test]
    async fn compress_files_writes_a_real_zip_with_basenamed_entries() {
        let dir = temp_dir("basic");
        let a = dir.join("a.txt");
        let b = dir.join("nested").join("b.txt");
        write_file(&a, "AAA");
        write_file(&b, "BBB");
        let out = dir.join("out.zip");

        let result = compress_files(
            &[a.to_string_lossy().into_owned(), b.to_string_lossy().into_owned()],
            &out.to_string_lossy(),
            false,
        )
        .await;
        assert_eq!(result, "ion-win: compress: compressed 2 file(s)");

        let entries = read_archive(&out);
        assert_eq!(
            entries,
            vec![("a.txt".to_string(), "AAA".to_string()), ("b.txt".to_string(), "BBB".to_string())]
        );

        let _ = fs::remove_dir_all(dir);
    }

    /// Reads real files via their absolute paths (so the test never has
    /// to mutate the process's actual current directory, which this
    /// project's testing philosophy rules out for in-process `#[test]`s)
    /// but stores each under a `path` column that only *contains* the
    /// intended relative shape as its final component(s) — proving
    /// `table_row_entry_name` strips the absolute prefix down to a
    /// sensible in-archive name rather than embedding the whole
    /// temp-directory path into the zip.
    #[tokio::test]
    async fn compress_table_preserves_relative_paths_as_entry_names() {
        let dir = temp_dir("table");
        let top = dir.join("top.txt");
        let nested = dir.join("sub").join("nested.txt");
        write_file(&top, "top");
        write_file(&nested, "nested");
        let out = dir.join("out.zip");

        let table = Table {
            rows: vec![
                vec![("path".to_string(), top.to_string_lossy().into_owned())],
                vec![("path".to_string(), nested.to_string_lossy().into_owned())],
            ],
        };
        let result = compress_table(&table, &out.to_string_lossy(), false).await;

        assert_eq!(result, "ion-win: compress: compressed 2 file(s)");
        let entries = read_archive(&out);
        // The exact entry name mirrors the absolute source path with its
        // root stripped (see `table_row_entry_name`'s own doc comment),
        // so what matters here is that both files' *contents* made it in
        // and that neither entry name is rooted/carries a drive letter.
        assert_eq!(entries.len(), 2);
        for (name, _) in &entries {
            assert!(!name.contains(':'), "entry name must not carry a drive letter: {name}");
            assert!(!name.starts_with('/'), "entry name must not be rooted: {name}");
        }
        assert!(entries.iter().any(|(_, c)| c == "top"));
        assert!(entries.iter().any(|(_, c)| c == "nested"));

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn compress_fileset_consumes_native_paths() {
        let dir = temp_dir("fileset");
        let source = dir.join("folder with spaces").join("native.txt");
        write_file(&source, "native fileset");
        let fileset = FileSet::new(
            vec![FileRecord::from_path(source, None).unwrap()],
            "test",
        );
        let out = dir.join("out.zip");

        let result = compress_fileset(&fileset, &out.to_string_lossy(), false).await;

        assert_eq!(result, "ion-win: compress: compressed 1 file(s)");
        assert_eq!(read_archive(&out).len(), 1);
        assert_eq!(read_archive(&out)[0].1, "native fileset");
        assert_eq!(fileset.provenance.producer, "test");
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn compress_fileset_per_root_names_archives_and_uses_relative_entries() {
        let dir = temp_dir("per-root");
        let alpha = dir.join("alpha");
        let beta = dir.join("beta folder");
        let alpha_file = alpha.join("nested").join("one.txt");
        let beta_file = beta.join("two.txt");
        write_file(&alpha_file, "alpha");
        write_file(&beta_file, "beta");
        let fileset = FileSet::new(
            vec![
                FileRecord::from_path(alpha_file, None).unwrap(),
                FileRecord::from_path(beta_file, None).unwrap(),
            ],
            "find",
        )
        .with_roots(vec![alpha, beta]);
        let output = dir.join("archives");

        let result = compress_fileset_per_root(&fileset, &output.to_string_lossy(), false)
            .await
            .unwrap();

        assert!(result.contains("alpha.zip"), "{result}");
        assert!(result.contains("beta folder.zip"), "{result}");
        assert_eq!(
            read_archive(&output.join("alpha.zip")),
            vec![("nested/one.txt".to_string(), "alpha".to_string())]
        );
        assert_eq!(
            read_archive(&output.join("beta folder.zip")),
            vec![("two.txt".to_string(), "beta".to_string())]
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn compress_fileset_per_root_rejects_case_insensitive_name_collisions() {
        let dir = temp_dir("per-root-collision");
        let first = dir.join("one").join("Data");
        let second = dir.join("two").join("data");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        let fileset = FileSet::new(Vec::new(), "find").with_roots(vec![first, second]);

        let error = compress_fileset_per_root(
            &fileset,
            &dir.join("archives").to_string_lossy(),
            false,
        )
        .await
        .unwrap_err();

        assert!(error.contains("same archive name"), "{error}");
        assert!(!dir.join("archives").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn archive_plan_previews_without_writing_then_apply_creates_zip_and_backup() {
        let dir = temp_dir("plan-apply");
        let root = dir.join("source");
        let source = root.join("nested").join("file.txt");
        write_file(&source, "planned");
        let fileset = FileSet::new(
            vec![FileRecord::from_path(source, None).unwrap()],
            "find",
        )
        .with_roots(vec![root]);
        let archives = dir.join("archives");
        let backups = dir.join("backups");

        let plan = plan_fileset_per_root(
            &fileset,
            &archives.to_string_lossy(),
            Some(&backups.to_string_lossy()),
        )
        .unwrap();
        let preview = plan.to_table();

        assert_eq!(preview.rows.len(), 1);
        assert!(!archives.exists(), "planning must not create the archive directory");
        assert!(!backups.exists(), "planning must not create the backup directory");

        apply_archive_plan(&plan, false).await.unwrap();
        let archive = archives.join("source.zip");
        let backup = backups.join("source.zip");
        assert_eq!(
            read_archive(&archive),
            vec![("nested/file.txt".to_string(), "planned".to_string())]
        );
        assert_eq!(fs::read(&archive).unwrap(), fs::read(&backup).unwrap());
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn archive_plan_rejects_source_drift_before_creating_destinations() {
        let dir = temp_dir("plan-drift");
        let root = dir.join("source");
        let source = root.join("file.txt");
        write_file(&source, "before");
        let fileset = FileSet::new(
            vec![FileRecord::from_path(source.clone(), None).unwrap()],
            "find",
        )
        .with_roots(vec![root]);
        let archives = dir.join("archives");
        let backups = dir.join("backups");
        let plan = plan_fileset_per_root(
            &fileset,
            &archives.to_string_lossy(),
            Some(&backups.to_string_lossy()),
        )
        .unwrap();

        // A different size guarantees metadata drift even on filesystems
        // whose timestamp granularity cannot distinguish these writes.
        write_file(&source, "after-and-a-different-size");
        let error = apply_archive_plan(&plan, false).await.unwrap_err();

        assert!(error.contains("source drift"), "{error}");
        assert!(!archives.exists(), "validation must finish before archive writes");
        assert!(!backups.exists(), "validation must finish before backup writes");
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn archive_plan_detects_same_path_replacement_by_windows_file_identity() {
        let dir = temp_dir("plan-identity-drift");
        let root = dir.join("source");
        let source = root.join("file.txt");
        write_file(&source, "same-size");
        let original = FileRecord::from_path(source.clone(), None).unwrap();
        let fileset = FileSet::new(vec![original.clone()], "find").with_roots(vec![root]);
        let archives = dir.join("archives");
        let plan = plan_fileset_per_root(&fileset, &archives.to_string_lossy(), None).unwrap();

        fs::remove_file(&source).unwrap();
        write_file(&source, "new-value");
        let replacement = FileRecord::from_path(source, None).unwrap();
        assert_ne!(original.identity, replacement.identity);

        let error = apply_archive_plan(&plan, false).await.unwrap_err();
        assert!(error.contains("file identity changed"), "{error}");
        assert!(!archives.exists(), "identity validation must precede writes");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn pipeline_flags_parse_plan_apply_and_backup_safely() {
        let parsed = parse_pipeline_flags(&[
            "--per-root".to_string(),
            "--plan".to_string(),
            "--backup".to_string(),
            "backup".to_string(),
            "archives".to_string(),
        ])
        .unwrap();
        assert_eq!(parsed, (false, true, true, false, Some("backup".to_string()), vec!["archives".to_string()]));
        assert!(parse_pipeline_flags(&[
            "--per-root".to_string(),
            "--plan".to_string(),
            "--apply".to_string(),
            "archives".to_string(),
        ])
        .unwrap_err()
        .contains("mutually exclusive"));
    }

    #[tokio::test]
    async fn compress_files_refuses_to_overwrite_without_force() {
        let dir = temp_dir("overwrite");
        let a = dir.join("a.txt");
        write_file(&a, "AAA");
        let out = dir.join("out.zip");
        write_file(&out, "not actually a zip");

        let result = compress_files(&[a.to_string_lossy().into_owned()], &out.to_string_lossy(), false).await;
        assert!(result.contains("destination already exists"), "{result}");
        assert_eq!(fs::read_to_string(&out).unwrap(), "not actually a zip", "must not have been overwritten");

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn compress_files_overwrites_when_force_is_given() {
        let dir = temp_dir("force");
        let a = dir.join("a.txt");
        write_file(&a, "AAA");
        let out = dir.join("out.zip");
        write_file(&out, "not actually a zip");

        let result = compress_files(&[a.to_string_lossy().into_owned()], &out.to_string_lossy(), true).await;
        assert_eq!(result, "ion-win: compress: compressed 1 file(s)");
        assert_eq!(read_archive(&out), vec![("a.txt".to_string(), "AAA".to_string())]);

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn compress_files_skips_a_missing_source_and_continues() {
        let dir = temp_dir("missing-source");
        let a = dir.join("a.txt");
        write_file(&a, "AAA");
        let missing = dir.join("does-not-exist.txt");
        let out = dir.join("out.zip");

        let result = compress_files(
            &[missing.to_string_lossy().into_owned(), a.to_string_lossy().into_owned()],
            &out.to_string_lossy(),
            false,
        )
        .await;
        assert_eq!(result, "ion-win: compress: compressed 1 file(s), skipped 1");
        assert_eq!(read_archive(&out), vec![("a.txt".to_string(), "AAA".to_string())]);

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn compress_table_skips_a_row_with_no_path_column() {
        let dir = temp_dir("no-path-column");
        let out = dir.join("out.zip");
        let table = Table { rows: vec![vec![("name".to_string(), "no-path-here".to_string())]] };

        let result = compress_table(&table, &out.to_string_lossy(), false).await;
        assert_eq!(result, "ion-win: compress: compressed 0 file(s), skipped 1");

        let _ = fs::remove_dir_all(dir);
    }

    /// Compresses many files at once, each on its own concurrent
    /// blocking task, and checks every single one's contents survive the
    /// splice into the final archive correctly — the concurrency-specific
    /// regression guard, mirroring `copy.rs`'s equivalent test.
    #[tokio::test]
    async fn compress_files_concurrently_compresses_many_files_correctly() {
        let dir = temp_dir("concurrent");
        let mut sources = Vec::new();
        for i in 0..32 {
            let src = dir.join(format!("in-{i}.txt"));
            write_file(&src, &format!("contents-{i}"));
            sources.push(src.to_string_lossy().into_owned());
        }
        let out = dir.join("out.zip");

        let result = compress_files(&sources, &out.to_string_lossy(), false).await;
        assert_eq!(result, "ion-win: compress: compressed 32 file(s)");

        let mut entries = read_archive(&out);
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for i in 0..32 {
            let expected = (format!("in-{i}.txt"), format!("contents-{i}"));
            assert!(entries.contains(&expected), "missing or wrong entry for file {i}");
        }

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn compression_honors_interrupt_and_removes_partial_archive() {
        let dir = temp_dir("interrupt");
        let src = dir.join("large-enough-to-schedule.bin");
        fs::write(&src, vec![42u8; 1024 * 1024]).unwrap();
        let out = dir.join("out.zip");

        // Simulates the flag set by the real Ctrl+C handler. Compression must
        // consume it, abandon the operation, and not leave a corrupt archive.
        crate::jobctl::request_interrupt();
        let result = compress_files(
            &[src.to_string_lossy().into_owned()],
            &out.to_string_lossy(),
            false,
        )
        .await;

        assert_eq!(result, "ion-win: compress: interrupted");
        assert!(!out.exists(), "an interrupted archive must be removed");
        assert!(!crate::jobctl::interrupt_requested());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn parse_flags_rejects_unknown_flag() {
        let err = parse_flags(&["--bogus".to_string()]).unwrap_err();
        assert!(err.contains("unknown option"), "{err}");
    }

    #[tokio::test]
    async fn parse_and_compress_files_requires_at_least_one_source_and_a_destination() {
        let err = parse_and_compress_files(&["only-one-arg".to_string()]).await.unwrap_err();
        assert!(err.contains("usage"), "{err}");
    }

    #[test]
    fn table_row_entry_name_converts_a_relative_path_directly() {
        assert_eq!(table_row_entry_name("top.txt"), "top.txt");
        assert_eq!(table_row_entry_name("sub/nested.txt"), "sub/nested.txt");
    }

    #[test]
    fn table_row_entry_name_strips_the_root_from_an_absolute_path() {
        let absolute = if cfg!(windows) { r"C:\Users\Bob\data.txt" } else { "/home/bob/data.txt" };
        let name = table_row_entry_name(absolute);
        assert!(!name.contains(':'), "entry name must not carry a drive letter: {name}");
        assert!(!name.starts_with('/'), "entry name must not be rooted: {name}");
    }
}
