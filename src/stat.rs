//! `stat FILE...` (`ARCHITECTURE.md` §21): gathers file metadata — and,
//! optionally, a content hash — into a `Table`, one row per file. This is
//! the motivating case for parallelizing anything in ion-win at all:
//! hashing many files is the one part of this that's genuinely CPU/IO-bound
//! and embarrassingly parallel, unlike everything else built so far.

use crate::fileset::{FileRecord, FileSet};
use sha2::{Digest, Sha256};
use std::io::Read;

/// Parses `stat`'s arguments into (file paths, optional hash algorithm).
/// Only `--hash sha256` is recognized — any other `--hash VALUE` or
/// unrecognized `--flag` is a clear error rather than being silently
/// treated as a file path.
pub fn parse_args(args: &[String]) -> Result<(Vec<String>, Option<String>), String> {
    let mut files = Vec::new();
    let mut hash_algo = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--hash" => {
                let algo = iter.next().ok_or_else(|| {
                    "stat: --hash requires an algorithm (e.g. 'sha256')".to_string()
                })?;
                if algo != "sha256" {
                    return Err(format!(
                        "stat: unsupported hash algorithm '{algo}' (only 'sha256' is supported)"
                    ));
                }
                hash_algo = Some(algo.clone());
            }
            _ if arg.starts_with("--") => return Err(format!("stat: unknown option: {arg}")),
            _ => files.push(arg.clone()),
        }
    }
    Ok((files, hash_algo))
}

/// One file's metadata row, computed synchronously — meant to run on a
/// blocking thread (see `build_table`), not the async runtime's own
/// worker threads. Returns an error string rather than bubbling up
/// through `?`, since a single file's failure shouldn't stop the others.
fn stat_one(path: &str, hash_algo: Option<&str>) -> Result<FileRecord, String> {
    if let Some(algo) = hash_algo {
        let digest = match algo {
            "sha256" => {
                let mut hasher = Sha256::new();
                let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
                let mut buffer = vec![0u8; 256 * 1024];
                loop {
                    if crate::jobctl::interrupt_requested() {
                        return Err("interrupted".to_string());
                    }
                    let count = file.read(&mut buffer).map_err(|e| e.to_string())?;
                    if count == 0 {
                        break;
                    }
                    hasher.update(&buffer[..count]);
                }
                format!("{:x}", hasher.finalize())
            }
            other => return Err(format!("unsupported hash algorithm '{other}'")),
        };
        return FileRecord::from_path(path.into(), Some(digest));
    }
    FileRecord::from_path(path.into(), None)
}

/// Builds a `Table` describing each file in `files`. Hashing (when
/// `hash_algo` is `Some`) runs concurrently across files via
/// `tokio::task::spawn_blocking` — reusing ion-win's existing tokio
/// runtime rather than a separate hand-rolled thread pool, since
/// `pipeline_exec.rs` is already fully async. One blocking task per file,
/// spawned in file order and awaited in that same order: each task
/// progresses concurrently on tokio's worker threads regardless of await
/// order, so wall-clock time is bounded by the slowest file rather than
/// the sum of all of them, while the resulting rows still come out in
/// stable input order. A file that can't be read is skipped — with a
/// printed warning — rather than aborting the whole scan: unlike `cat`'s
/// fail-fast policy, `stat` is describing a batch, and one file vanishing
/// mid-scan (a real race during a directory walk) shouldn't discard
/// results for every other file.
pub async fn build_fileset(files: &[String], hash_algo: Option<&str>) -> FileSet {
    let algo_owned = hash_algo.map(str::to_string);
    let concurrency = if hash_algo.is_some() {
        std::thread::available_parallelism().map_or(1, usize::from)
    } else {
        std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(4)
    };
    let mut pending = files.iter().cloned().enumerate();
    let mut active = tokio::task::JoinSet::new();
    let mut ordered: Vec<Option<FileRecord>> = vec![None; files.len()];
    let mut interrupted = false;

    loop {
        if crate::jobctl::interrupt_requested() {
            interrupted = true;
        }
        while active.len() < concurrency && !crate::jobctl::interrupt_requested() {
            let Some((index, path)) = pending.next() else {
                break;
            };
            let algo = algo_owned.clone();
            active.spawn_blocking(move || {
                let result = stat_one(&path, algo.as_deref());
                (index, path, result)
            });
        }
        if active.is_empty() {
            break;
        }
        match active.join_next().await.expect("active set is not empty") {
            Ok((index, _, Ok(row))) => ordered[index] = Some(row),
            Ok((_, _, Err(e))) if e == "interrupted" => interrupted = true,
            Ok((_, path, Err(e))) => crate::err_println!("ion-win: stat: {path}: {e}"),
            Err(e) => crate::err_println!("ion-win: stat: task failed: {e}"),
        }
        if crate::jobctl::interrupt_requested() {
            interrupted = true;
        }
    }
    if interrupted {
        let _ = crate::jobctl::take_interrupt();
    }
    FileSet::new(ordered.into_iter().flatten().collect(), "stat")
}

pub async fn build_table(files: &[String], hash_algo: Option<&str>) -> crate::table::Table {
    build_fileset(files, hash_algo).await.to_table()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::Row;

    fn field(row: &Row, key: &str) -> Option<String> {
        row.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    #[test]
    fn parse_args_separates_files_from_hash_flag() {
        let (files, algo) = parse_args(&[
            "a.txt".to_string(),
            "--hash".to_string(),
            "sha256".to_string(),
            "b.txt".to_string(),
        ])
        .unwrap();
        assert_eq!(files, vec!["a.txt".to_string(), "b.txt".to_string()]);
        assert_eq!(algo, Some("sha256".to_string()));
    }

    #[test]
    fn parse_args_with_no_flags_is_all_files() {
        let (files, algo) = parse_args(&["a.txt".to_string(), "b.txt".to_string()]).unwrap();
        assert_eq!(files, vec!["a.txt".to_string(), "b.txt".to_string()]);
        assert_eq!(algo, None);
    }

    #[test]
    fn parse_args_rejects_unsupported_algorithm() {
        let err = parse_args(&["--hash".to_string(), "md5".to_string()]).unwrap_err();
        assert!(err.contains("unsupported hash algorithm"), "{err}");
    }

    #[test]
    fn parse_args_rejects_hash_with_no_value() {
        let err = parse_args(&["--hash".to_string()]).unwrap_err();
        assert!(err.contains("requires an algorithm"), "{err}");
    }

    #[test]
    fn parse_args_rejects_unknown_flag() {
        let err = parse_args(&["--bogus".to_string()]).unwrap_err();
        assert!(err.contains("unknown option"), "{err}");
    }

    #[tokio::test]
    async fn build_table_stats_files_and_skips_missing_ones_with_a_warning() {
        let mut path = std::env::temp_dir();
        path.push(format!("ion-win-stat-test-{}.txt", std::process::id()));
        std::fs::write(&path, b"hello").unwrap();

        let files = vec![
            path.to_string_lossy().into_owned(),
            "ion-win-definitely-does-not-exist-99999.txt".to_string(),
        ];
        let table = build_table(&files, None).await;
        assert_eq!(
            table.rows.len(),
            1,
            "the missing file should be skipped, not error out"
        );
        assert_eq!(field(&table.rows[0], "size"), Some("5".to_string()));
        assert_eq!(field(&table.rows[0], "is_dir"), Some("false".to_string()));

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn build_table_computes_sha256_hash_when_requested() {
        let mut path = std::env::temp_dir();
        path.push(format!("ion-win-stat-hash-test-{}.txt", std::process::id()));
        std::fs::write(&path, b"hello").unwrap();

        let expected = {
            let mut hasher = Sha256::new();
            hasher.update(b"hello");
            format!("{:x}", hasher.finalize())
        };

        let files = vec![path.to_string_lossy().into_owned()];
        let table = build_table(&files, Some("sha256")).await;
        assert_eq!(field(&table.rows[0], "sha256"), Some(expected));

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn build_table_preserves_input_order_across_concurrent_hashing() {
        let mut paths = Vec::new();
        for i in 0..8 {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "ion-win-stat-order-test-{}-{i}.txt",
                std::process::id()
            ));
            std::fs::write(&path, format!("file-{i}")).unwrap();
            paths.push(path);
        }
        let files: Vec<String> = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();

        let table = build_table(&files, Some("sha256")).await;
        let got_paths: Vec<String> = table
            .rows
            .iter()
            .map(|row| field(row, "path").unwrap())
            .collect();
        assert_eq!(got_paths, files);

        for path in paths {
            let _ = std::fs::remove_file(path);
        }
    }
}
