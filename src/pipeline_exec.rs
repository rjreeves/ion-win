//! Executes a parsed `pipeline::Pipeline` against real OS processes.
//!
//! Supported: `|`/`^|`/`&|` piping between external commands, `>`/`>>`/
//! `^>`/`&>` redirection to files, `echo` as a producer (writing into the
//! next stage's stdin or a redirect target), and `&`/`&!` (spawn without
//! waiting — `&` registers one background execution with `ExecutionManager`
//! for `jobs`/`wait`/`disown`; `&!` remains untracked. There is no `fg`/`bg`
//! because Windows has no faithful POSIX stop/resume equivalent.
//!
//! Not supported as a pipeline stage: most other builtins (`pvar`, `dmark`,
//! `test`, `matches`, `read`, ...) or a user-defined `fn` —
//! encountering one there aborts the pipeline with a clear message rather
//! than silently doing something surprising. This is because those
//! builtins write straight to the real process stdout/stderr today; giving
//! them a redirectable/pipeable output would need refactoring every one of
//! them to write through an injected sink, which is future work.
//!
//! `&|` (combined stdout+stderr piped into the next stage) is approximated:
//! both streams are forwarded by separate threads into the next stage's
//! stdin, so the combined bytes are concatenated (each stream internally
//! intact) rather than interleaved in real chronological order. True
//! interleaving would need OS-level fd duplication, which isn't exposed
//! portably by stable `std::process`.

use crate::err_println;
use crate::execution;
use crate::fileset::FileSet;
use crate::interp::Interpreter;
use crate::jobctl;
use crate::pipeline::{PipeKind, Pipeline, Redirect, Stream};
use crate::state::StateHandle;
use crate::table::Table;
use std::io::{self, Read, Write};
use std::process::{Child, ChildStderr, ChildStdout, Stdio};
use std::sync::{Arc, Mutex};

enum Kind {
    /// Args plus whether the trailing newline is suppressed (`echo -n`).
    Echo(Vec<String>, bool),
    /// `cat FILE...` (ion-win extension, `ARCHITECTURE.md` §20) as a
    /// pipeline producer — the file paths (already expanded), read and
    /// concatenated at execution time via `fs_builtins::capture`, the
    /// same in-process routine `shell.rs`'s standalone `cat` and
    /// `interp.rs`'s `$(cat ...)` capture both already go through.
    Cat(Vec<String>),
    /// `find [--all] [--recurse] [PATH]` (ion-win extension,
    /// `ARCHITECTURE.md` §22) as a pipeline producer — one path per line,
    /// meant to feed `stat` (§21). Unlike `Cat`, its output is a
    /// synthesized list (like `Echo`'s), not raw file bytes, so it gets
    /// exactly one trailing newline added when printed/written rather
    /// than `Cat`'s transparent passthrough.
    Find(Vec<String>),
    /// `files`/`folders` used in a structured pipeline are native FileSet
    /// providers. Their standalone shell forms remain newline-delimited.
    ListEntries(String, Vec<String>),
    /// `copy`/`cp [--force] ...` (ion-win extension, `ARCHITECTURE.md`
    /// §24) — raw args, resolved at execution time into one of two forms
    /// depending on how many positional arguments remain after stripping
    /// flags: `SRC... DEST` (explicit files, ignores whatever was piped
    /// in, matching `Cat`'s "explicit args win" precedent) or just `DEST`
    /// (sources come from the incoming `Table`'s `path` column instead).
    Copy(Vec<String>),
    /// `move`/`mv [--force] ...`: explicit `SRC... DEST`, or one `DEST`
    /// consuming a table's `path` column. A moved manifest is not
    /// forwarded because its recorded paths are stale afterward.
    Move(Vec<String>),
    /// `compress [--force] ...` (ion-win extension, `ARCHITECTURE.md`
    /// §25) — raw args, resolved at execution time exactly like `Copy`:
    /// `SRC... DEST.zip` (explicit files) or just `DEST.zip` (sources
    /// come from the incoming `Table`'s `path` column instead).
    Compress(Vec<String>),
    /// `delete [--recurse] [--permanent --force] [PATH...]`. With
    /// explicit paths it ignores pipeline input; with no paths it consumes
    /// the incoming table's `path` column. Unlike copy/compress, it does
    /// not forward a now-stale manifest after deletion.
    Delete(Vec<String>),
    External(Vec<String>),
    /// Structured-data pipeline stages (`ARCHITECTURE.md` §17) — an
    /// in-process object bridge, since external processes only ever see
    /// JSON text, never a `Table` value directly.
    FromJson,
    ToJson,
    /// `from-csv`/`to-csv` (`ARCHITECTURE.md` §23) — the same explicit
    /// boundary-adapter pattern as `FromJson`/`ToJson`, just CSV text
    /// instead of JSON text.
    FromCsv,
    ToCsv,
    Select(Vec<String>),
    /// `where`/`filter COLUMN OP VALUE` — validated at execution time
    /// (arg count and operator), not here, matching how `FromJson`
    /// validates JSON-parseability during execution rather than
    /// classification.
    Where(Vec<String>),
    /// `date-column SOURCE DEST OP [ARGS...]` transforms one temporal
    /// field across every row and forwards the resulting table.
    DateColumn(Vec<String>),
    /// `stat FILE... [--hash sha256]` (`ARCHITECTURE.md` §21) — a `Table`
    /// producer, like `FromJson`, but sourced from real file metadata
    /// instead of parsed JSON text. Raw args, validated (via
    /// `crate::stat::parse_args`) at execution time.
    Stat(Vec<String>),
    /// A bare reference to a `Table` variable (`ARCHITECTURE.md` §18),
    /// used as an independent pipeline source — like `Echo`, it ignores
    /// whatever fed it rather than trying to merge the two. Resolved once
    /// at classification time (the table is cloned in), not re-looked-up
    /// at execution time.
    TableSource(Table),
    FileSetSource(FileSet),
    /// A builtin/function name that isn't yet supported as a pipeline
    /// stage. Empty string means the stage had no command at all (e.g. a
    /// stray `|`).
    Unsupported(String),
}

/// `where`/`filter`'s recognized comparison operators — the exact same set
/// `test`/`if` accept (`src/builtins.rs`'s `eval_binary`), checked upfront
/// so an unrecognized operator gets a clearly-attributed `where:` error
/// instead of `eval_test`'s own `test:`-prefixed one.
const SUPPORTED_WHERE_OPS: &[&str] = &["=", "==", "!=", "-eq", "-ne", "-lt", "-le", "-gt", "-ge"];

/// What feeds the *next* stage's stdin.
enum Carry {
    None,
    /// A single already-open pipe end from the previous external stage.
    Stdio(Stdio),
    /// Literal bytes from a previous `echo` stage, written after the next
    /// stage is spawned with a piped stdin.
    Bytes(Vec<u8>),
    /// Both streams of the previous external stage (for `&|`), forwarded
    /// by background threads once the next stage is spawned.
    Merge(ChildStdout, ChildStderr),
    /// A structured table from a previous `from-json`/`select` stage.
    Table(Table),
    FileSet(FileSet),
}

pub enum CapturedStructured {
    Table(Table),
    FileSet(FileSet),
}

enum EitherStructured {
    Table(Table),
    FileSet(FileSet),
}

/// Whether `kind` needs to read its input in-process (a `Table`, or raw
/// text it'll parse itself) rather than just being handed a raw OS pipe
/// handle to pass along unread — used to decide, right when an `External`
/// stage's stdout is claimed, whether to keep the zero-copy `Carry::Stdio`
/// fast path or read it into memory instead. This has to happen at that
/// exact point: once a `ChildStdout` is wrapped as a `Stdio` (for handing
/// to the *next* spawned process), it can no longer be read directly —
/// there's no going back to get the readable handle out again.
fn next_stage_needs_materialized_input(kind: Option<&Kind>) -> bool {
    matches!(
        kind,
        Some(Kind::FromJson)
            | Some(Kind::Select(_))
            | Some(Kind::ToJson)
            | Some(Kind::Where(_))
            | Some(Kind::DateColumn(_))
            | Some(Kind::Stat(_))
            | Some(Kind::FromCsv)
    )
}

/// A `Write` impl that locks a shared `ChildStdin` per call, letting two
/// forwarder threads (stdout-copier and stderr-copier) safely share one
/// destination without needing OS-level handle duplication.
struct LockedWriter(Arc<Mutex<std::process::ChildStdin>>);

impl Write for LockedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

/// Runs `pipeline` to completion (unless backgrounded/disowned), returning
/// whether it succeeded — the last stage's exit status for a foreground
/// run, or `true` immediately for a backgrounded one.
pub async fn run(pipeline: &Pipeline, interp: &mut Interpreter, state: &StateHandle) -> bool {
    run_impl(pipeline, interp, state, None).await
}

/// Same as `run`, but if the pipeline's *last* stage produces a `Table`
/// (rather than printing/writing one out — `finish_table_stage`'s
/// terminal-stage behavior), it's captured and returned instead of shown,
/// for `let NAME = PIPELINE` (`ARCHITECTURE.md` §18) to store. `None` if
/// the pipeline never reached a table-producing terminal stage (e.g. it
/// ended in `to-json`, an external command, or failed outright).
pub async fn run_capturing_table(
    pipeline: &Pipeline,
    interp: &mut Interpreter,
    state: &StateHandle,
) -> (bool, Option<CapturedStructured>) {
    let mut captured = None;
    let ok = run_impl(pipeline, interp, state, Some(&mut captured)).await;
    (ok, captured)
}

async fn run_impl(
    pipeline: &Pipeline,
    interp: &mut Interpreter,
    _state: &StateHandle,
    mut capture: Option<&mut Option<CapturedStructured>>,
) -> bool {
    if pipeline.stages.is_empty() || pipeline.stages.iter().all(|s| s.tokens.is_empty()) {
        return true;
    }

    let kinds = classify_stages(pipeline, interp);
    if let Some(name) = kinds.iter().find_map(|k| match k {
        Kind::Unsupported(name) => Some(name),
        _ => None,
    }) {
        if name.is_empty() {
            err_println!("ion-win: empty command in pipeline");
        } else {
            println!(
                "ion-win: '{name}': piping/redirection is only supported for 'echo' and \
                 external commands so far, not this builtin"
            );
        }
        return false;
    }

    // Only a foreground pipeline's children should be interruptible by
    // Ctrl+C — a backgrounded (`&`) or disowned (`&!`) one keeps running
    // regardless, matching real shell job-control semantics.
    let is_foreground = !pipeline.background && !pipeline.disown;

    let pipeline_execution = if is_foreground {
        match execution::begin_foreground_pipeline(pipeline_display(pipeline)) {
            Ok(id) => Some(id),
            Err(error) => {
                err_println!("ion-win: could not register foreground pipeline: {error}");
                return false;
            }
        }
    } else if pipeline.background {
        match execution::begin_background_pipeline(pipeline_display(pipeline)) {
            Ok(id) => Some(id),
            Err(error) => {
                err_println!("ion-win: could not register background pipeline: {error}");
                return false;
            }
        }
    } else {
        None
    };

    let mut children: Vec<Child> = Vec::new();
    // Parallel to `children` (same index), for the execution manager's job
    // display text if this pipeline turns out to be backgrounded.
    let mut command_texts: Vec<String> = Vec::new();
    let mut merge_threads: Vec<std::thread::JoinHandle<()>> = Vec::new();
    let mut carry = Carry::None;

    // Unregisters whatever's been spawned so far — used on every early-exit
    // path below so a mid-pipeline failure doesn't leave stale entries in
    // the foreground registry.
    macro_rules! unregister_spawned {
        () => {
            if let Some(id) = pipeline_execution {
                execution::fail_pipeline_execution(id, "pipeline setup failed");
            }
        };
    }

    for (i, kind) in kinds.iter().enumerate() {
        let is_last = i == kinds.len() - 1;
        let (stdout_file, stderr_file) = match resolve_stage_stdio(&pipeline.stages[i].redirects) {
            Ok(v) => v,
            Err(e) => {
                err_println!("ion-win: redirection error: {e}");
                unregister_spawned!();
                return false;
            }
        };
        let incoming = std::mem::replace(&mut carry, Carry::None);

        match kind {
            Kind::Echo(args, no_newline) => {
                let mut text = args.join(" ");
                if !no_newline {
                    text.push('\n');
                }
                drop(incoming); // echo never reads stdin; drop whatever fed it
                if let Some(mut f) = stdout_file {
                    let _ = f.write_all(text.as_bytes());
                } else if is_last {
                    print!("{text}");
                } else {
                    carry = Carry::Bytes(text.into_bytes());
                }
            }
            Kind::Cat(files) => {
                drop(incoming); // reads named files, not stdin
                match crate::fs_builtins::capture("cat", files) {
                    Some(Ok(text)) => {
                        // No synthesized trailing newline (unlike Echo):
                        // this is a file's own bytes passed through as-is.
                        if let Some(mut f) = stdout_file {
                            let _ = f.write_all(text.as_bytes());
                        } else if is_last {
                            print!("{text}");
                        } else {
                            carry = Carry::Bytes(text.into_bytes());
                        }
                    }
                    Some(Err(e)) => {
                        err_println!("ion-win: {e}");
                        unregister_spawned!();
                        return false;
                    }
                    None => unreachable!("fs_builtins::capture always recognizes \"cat\""),
                }
            }
            Kind::Find(find_args) => {
                let incoming = match incoming {
                    Carry::FileSet(roots) => {
                        match crate::fs_builtins::find_in_fileset(find_args, &roots) {
                            Ok(fileset) => {
                                carry = finish_fileset_stage(
                                    fileset,
                                    is_last,
                                    stdout_file,
                                    capture.as_mut().map(|slot| &mut **slot),
                                );
                                continue;
                            }
                            Err(error) => {
                                err_println!("ion-win: {error}");
                                unregister_spawned!();
                                return false;
                            }
                        }
                    }
                    other => other,
                };
                drop(incoming);
                match crate::fs_builtins::capture("find", find_args) {
                    Some(Ok(mut text)) => {
                        // Unlike Cat: this is a synthesized list (like
                        // Echo's), so it gets exactly one trailing
                        // newline — but only if there's anything to print,
                        // so an empty result doesn't add a stray blank line.
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        if let Some(mut f) = stdout_file {
                            let _ = f.write_all(text.as_bytes());
                        } else if is_last {
                            print!("{text}");
                        } else {
                            carry = Carry::Bytes(text.into_bytes());
                        }
                    }
                    Some(Err(e)) => {
                        err_println!("ion-win: {e}");
                        unregister_spawned!();
                        return false;
                    }
                    None => unreachable!("fs_builtins::capture always recognizes \"find\""),
                }
            }
            Kind::ListEntries(name, args) => {
                drop(incoming);
                match crate::fs_builtins::capture_fileset(name, args) {
                    Some(Ok(fileset)) => {
                        carry = finish_fileset_stage(
                            fileset,
                            is_last,
                            stdout_file,
                            capture.as_mut().map(|slot| &mut **slot),
                        );
                    }
                    Some(Err(error)) => {
                        err_println!("ion-win: {error}");
                        unregister_spawned!();
                        return false;
                    }
                    None => unreachable!("ListEntries only contains files/folders/dirs"),
                }
            }
            Kind::Copy(copy_args) => {
                let (force, mut positional) = match crate::copy::parse_flags(copy_args) {
                    Ok(v) => v,
                    Err(e) => {
                        drop(incoming);
                        err_println!("ion-win: {e}");
                        unregister_spawned!();
                        return false;
                    }
                };

                // When sourced from a table, that same table is forwarded
                // to the next stage after this one finishes — so several
                // manifest-driven operations can be chained in one pipe
                // (`manifest | compress out.zip | copy backup`) rather
                // than each needing its own separate `manifest | ...`
                // statement. The explicit-files form never had a table to
                // begin with, so there's nothing to forward in that case.
                let mut forwarded_table: Option<Table> = None;
                let mut forwarded_fileset: Option<FileSet> = None;

                let mut text = if positional.len() >= 2 {
                    // Explicit files: SRC... DEST. Ignores whatever was
                    // piped in, matching Cat's "explicit args win"
                    // precedent.
                    drop(incoming);
                    let dest = positional.pop().expect("checked len >= 2 above");
                    crate::copy::copy_files(&positional, &dest, force).await
                } else if positional.len() == 1 {
                    // Just DEST: sources come from the incoming table's
                    // "path" column.
                    let dest = &positional[0];
                    match incoming {
                        Carry::Table(t) => {
                            let result = crate::copy::copy_table(&t, dest, force).await;
                            forwarded_table = Some(t);
                            result
                        }
                        Carry::FileSet(fileset) => {
                            let result = crate::copy::copy_fileset(&fileset, dest, force).await;
                            forwarded_fileset = Some(fileset);
                            result
                        }
                        Carry::None => {
                            err_println!(
                                "ion-win: copy: no table piped in (pipe through 'stat' first) \
                                 and no source files given"
                            );
                            unregister_spawned!();
                            return false;
                        }
                        _ => {
                            err_println!(
                                "ion-win: copy: expected a table (pipe through 'stat' first)"
                            );
                            unregister_spawned!();
                            return false;
                        }
                    }
                } else {
                    drop(incoming);
                    err_println!(
                        "ion-win: copy: usage: copy [--force] SRC... DEST  |  TABLE | copy [--force] DEST"
                    );
                    unregister_spawned!();
                    return false;
                };

                text.push('\n');
                // Copy is side-effecting: its result is printed
                // immediately regardless of pipeline position, not only
                // when it happens to be the last stage — unlike a pure
                // transform (`select`/`where`), "did the files actually
                // get copied" shouldn't be swallowed just because
                // something follows it in the pipe.
                if let Some(mut f) = stdout_file {
                    let _ = f.write_all(text.as_bytes());
                } else {
                    print!("{text}");
                }
                carry = if let Some(fileset) = forwarded_fileset { Carry::FileSet(fileset) }
                    else if let Some(table) = forwarded_table { Carry::Table(table) }
                    else { Carry::None };
            }
            Kind::Move(move_args) => {
                let (force, mut positional) = match crate::fs_ops::parse_move_flags(move_args) {
                    Ok(value) => value,
                    Err(error) => {
                        drop(incoming);
                        err_println!("ion-win: {error}");
                        unregister_spawned!();
                        return false;
                    }
                };
                let mut text = if positional.len() >= 2 {
                    drop(incoming);
                    let destination = positional.pop().expect("checked length");
                    crate::fs_ops::move_paths(&positional, &destination, force).await
                } else if positional.len() == 1 {
                    match incoming {
                        Carry::Table(table) => {
                            crate::fs_ops::move_table(&table, &positional[0], force).await
                        }
                        Carry::FileSet(fileset) => crate::fs_ops::move_table(&fileset.to_table(), &positional[0], force).await,
                        Carry::None => {
                            err_println!(
                                "ion-win: move: no table piped in and no source paths given"
                            );
                            unregister_spawned!();
                            return false;
                        }
                        _ => {
                            err_println!("ion-win: move: expected a table");
                            unregister_spawned!();
                            return false;
                        }
                    }
                } else {
                    drop(incoming);
                    err_println!(
                        "ion-win: move: usage: move [--force] SRC... DEST  |  TABLE | move [--force] DEST"
                    );
                    unregister_spawned!();
                    return false;
                };
                text.push('\n');
                if let Some(mut file) = stdout_file {
                    let _ = file.write_all(text.as_bytes());
                } else {
                    print!("{text}");
                }
                carry = Carry::None;
            }
            Kind::Compress(compress_args) => {
                let (force, per_root, plan_only, _apply, backup, mut positional) = match crate::compress::parse_pipeline_flags(compress_args) {
                    Ok(v) => v,
                    Err(e) => {
                        drop(incoming);
                        err_println!("ion-win: {e}");
                        unregister_spawned!();
                        return false;
                    }
                };

                // See `Kind::Copy`'s identical comment: a table source is
                // forwarded onward so operations can be chained in one
                // pipe (`manifest | compress out.zip | copy backup`); the
                // explicit-files form has no table to forward.
                let mut forwarded_table: Option<Table> = None;
                let mut forwarded_fileset: Option<FileSet> = None;

                if plan_only {
                    if positional.len() != 1 {
                        drop(incoming);
                        err_println!("ion-win: compress: --plan requires one archive directory");
                        unregister_spawned!();
                        return false;
                    }
                    let plan = match incoming {
                        Carry::FileSet(fileset) => match crate::compress::plan_fileset_per_root(
                            &fileset,
                            &positional[0],
                            backup.as_deref(),
                        ) {
                            Ok(plan) => plan,
                            Err(error) => {
                                err_println!("ion-win: {error}");
                                unregister_spawned!();
                                return false;
                            }
                        },
                        _ => {
                            err_println!("ion-win: compress: --plan requires a FileSet with root provenance");
                            unregister_spawned!();
                            return false;
                        }
                    };
                    carry = finish_table_stage(
                        plan.to_table(),
                        is_last,
                        stdout_file,
                        capture.as_mut().map(|slot| &mut **slot),
                    );
                    continue;
                }

                let mut text = if per_root {
                    if positional.len() != 1 {
                        drop(incoming);
                        err_println!("ion-win: compress: --per-root requires one destination directory");
                        unregister_spawned!();
                        return false;
                    }
                    match incoming {
                        Carry::FileSet(fileset) => {
                            let plan = match crate::compress::plan_fileset_per_root(
                                &fileset,
                                &positional[0],
                                backup.as_deref(),
                            ) {
                                Ok(plan) => plan,
                                Err(error) => {
                                    err_println!("ion-win: {error}");
                                    unregister_spawned!();
                                    return false;
                                }
                            };
                            let result = match crate::compress::apply_archive_plan(&plan, force).await {
                                Ok(result) => result,
                                Err(error) => {
                                    err_println!("ion-win: {error}");
                                    unregister_spawned!();
                                    return false;
                                }
                            };
                            forwarded_fileset = Some(fileset);
                            result
                        }
                        _ => {
                            err_println!("ion-win: compress: --per-root requires a FileSet with root provenance");
                            unregister_spawned!();
                            return false;
                        }
                    }
                } else if positional.len() >= 2 {
                    // Explicit files: SRC... DEST.zip. Ignores whatever
                    // was piped in, matching Copy's "explicit args win"
                    // precedent.
                    drop(incoming);
                    let dest = positional.pop().expect("checked len >= 2 above");
                    crate::compress::compress_files(&positional, &dest, force).await
                } else if positional.len() == 1 {
                    // Just DEST.zip: sources come from the incoming
                    // table's "path" column.
                    let dest = &positional[0];
                    match incoming {
                        Carry::Table(t) => {
                            let result = crate::compress::compress_table(&t, dest, force).await;
                            forwarded_table = Some(t);
                            result
                        }
                        Carry::FileSet(fileset) => {
                            let result = crate::compress::compress_fileset(&fileset, dest, force).await;
                            forwarded_fileset = Some(fileset);
                            result
                        }
                        Carry::None => {
                            err_println!(
                                "ion-win: compress: no FileSet/table piped in (pipe through 'stat' first) \
                                 and no source files given"
                            );
                            unregister_spawned!();
                            return false;
                        }
                        _ => {
                            err_println!(
                                "ion-win: compress: expected a FileSet or table (pipe through 'stat' first)"
                            );
                            unregister_spawned!();
                            return false;
                        }
                    }
                } else {
                    drop(incoming);
                    err_println!(
                        "ion-win: compress: usage: compress [--force] SRC... DEST.zip  |  FILESET | compress [--force] DEST.zip"
                    );
                    unregister_spawned!();
                    return false;
                };

                text.push('\n');
                // Compress is side-effecting, same reasoning as Copy: its
                // result is printed immediately regardless of pipeline
                // position, not only when it happens to be the last stage.
                if let Some(mut f) = stdout_file {
                    let _ = f.write_all(text.as_bytes());
                } else {
                    print!("{text}");
                }
                carry = if let Some(fileset) = forwarded_fileset { Carry::FileSet(fileset) }
                    else if let Some(table) = forwarded_table { Carry::Table(table) }
                    else { Carry::None };
            }
            Kind::Delete(delete_args) => {
                let (options, positional) = match crate::delete::parse_flags(delete_args) {
                    Ok(v) => v,
                    Err(e) => {
                        drop(incoming);
                        err_println!("ion-win: {e}");
                        unregister_spawned!();
                        return false;
                    }
                };

                let mut text = if !positional.is_empty() {
                    drop(incoming);
                    crate::delete::delete_paths(&positional, options).await
                } else {
                    match incoming {
                        Carry::Table(t) => crate::delete::delete_table(&t, options).await,
                        Carry::FileSet(f) => crate::delete::delete_table(&f.to_table(), options).await,
                        Carry::None => {
                            err_println!("ion-win: delete: no paths given and no table piped in");
                            unregister_spawned!();
                            return false;
                        }
                        _ => {
                            err_println!("ion-win: delete: expected a table");
                            unregister_spawned!();
                            return false;
                        }
                    }
                };

                text.push('\n');
                if let Some(mut f) = stdout_file {
                    let _ = f.write_all(text.as_bytes());
                } else {
                    print!("{text}");
                }
                carry = Carry::None;
            }
            Kind::External(args) => {
                let Some(resolved) = crate::command_resolver::resolve(&args[0]) else {
                    interp.mark_command_not_found();
                    err_println!("ion-win: command not found: {}", args[0]);
                    unregister_spawned!();
                    return false;
                };
                let mut command = jobctl::new_command(&resolved);
                command.args(&args[1..]);

                let mut post_spawn_bytes = None;
                let mut post_spawn_merge = None;
                match incoming {
                    Carry::None => {
                        command.stdin(if i == 0 {
                            Stdio::inherit()
                        } else {
                            Stdio::null()
                        });
                    }
                    Carry::Stdio(stdio) => {
                        command.stdin(stdio);
                    }
                    Carry::Bytes(bytes) => {
                        command.stdin(Stdio::piped());
                        post_spawn_bytes = Some(bytes);
                    }
                    Carry::Merge(out, err) => {
                        command.stdin(Stdio::piped());
                        post_spawn_merge = Some((out, err));
                    }
                    Carry::Table(table) => {
                        // An external process only ever sees JSON text,
                        // never a `Table` value directly.
                        command.stdin(Stdio::piped());
                        post_spawn_bytes = Some(table.to_json().into_bytes());
                    }
                    Carry::FileSet(_) => {
                        err_println!("ion-win: typed FileSet cannot be passed to an external command; convert it with 'to-json' or 'to-csv'");
                        unregister_spawned!();
                        return false;
                    }
                }

                let next_pipe_kind = pipeline.pipes.get(i).copied();

                if let Some(f) = stdout_file {
                    command.stdout(Stdio::from(f));
                } else if !is_last {
                    command.stdout(Stdio::piped());
                } else {
                    command.stdout(Stdio::inherit());
                }

                let stderr_feeds_pipe = !is_last
                    && matches!(
                        next_pipe_kind,
                        Some(PipeKind::Stderr) | Some(PipeKind::Combined)
                    );
                if let Some(f) = stderr_file {
                    command.stderr(Stdio::from(f));
                } else if stderr_feeds_pipe {
                    command.stderr(Stdio::piped());
                } else {
                    command.stderr(Stdio::inherit());
                }

                let mut child = match command.spawn() {
                    Ok(c) => c,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        interp.mark_command_not_found();
                        err_println!("ion-win: command not found: {}", args[0]);
                        unregister_spawned!();
                        return false;
                    }
                    Err(e) => {
                        err_println!("ion-win: failed to run '{}': {e}", args[0]);
                        unregister_spawned!();
                        return false;
                    }
                };
                if let Some(id) = pipeline_execution {
                    let _ = execution::try_assign_job_object(id, &child);
                }
                if let Some(id) = pipeline_execution {
                    if let Err(error) = execution::register_pipeline_process_with_display(
                        id,
                        child.id(),
                        args.join(" "),
                    ) {
                        err_println!("ion-win: could not register pipeline process: {error}");
                        unregister_spawned!();
                        return false;
                    }
                }

                if let Some(bytes) = post_spawn_bytes {
                    if let Some(mut stdin) = child.stdin.take() {
                        let _ = stdin.write_all(&bytes);
                        // `stdin` drops here, closing it so the child sees EOF.
                    }
                }
                if let Some((mut out, mut err)) = post_spawn_merge {
                    if let Some(stdin) = child.stdin.take() {
                        let stdin = Arc::new(Mutex::new(stdin));
                        let s1 = stdin.clone();
                        merge_threads.push(std::thread::spawn(move || {
                            let _ = io::copy(&mut out, &mut LockedWriter(s1));
                        }));
                        let s2 = stdin.clone();
                        merge_threads.push(std::thread::spawn(move || {
                            let _ = io::copy(&mut err, &mut LockedWriter(s2));
                        }));
                        // Drop our own reference so the child's stdin closes
                        // once both forwarder threads finish and drop theirs.
                    }
                }

                let next_needs_materialized = next_stage_needs_materialized_input(kinds.get(i + 1));
                carry = match next_pipe_kind {
                    None => Carry::None,
                    Some(PipeKind::Stdout) => match child.stdout.take() {
                        Some(mut out) if next_needs_materialized => {
                            let mut buf = Vec::new();
                            let _ = out.read_to_end(&mut buf);
                            Carry::Bytes(buf)
                        }
                        Some(out) => Carry::Stdio(Stdio::from(out)),
                        None => Carry::None,
                    },
                    Some(PipeKind::Stderr) => child
                        .stderr
                        .take()
                        .map(Stdio::from)
                        .map(Carry::Stdio)
                        .unwrap_or(Carry::None),
                    Some(PipeKind::Combined) => match (child.stdout.take(), child.stderr.take()) {
                        (Some(out), Some(err)) => Carry::Merge(out, err),
                        _ => Carry::None,
                    },
                };

                command_texts.push(args.join(" "));
                children.push(child);
            }
            Kind::TableSource(table) => {
                drop(incoming); // an independent producer, like echo
                carry = finish_table_stage(
                    table.clone(),
                    is_last,
                    stdout_file,
                    capture.as_mut().map(|s| &mut **s),
                );
            }
            Kind::FileSetSource(fileset) => {
                drop(incoming);
                carry = finish_fileset_stage(fileset.clone(), is_last, stdout_file, capture.as_mut().map(|s| &mut **s));
            }
            Kind::FromJson => {
                let bytes = match incoming {
                    Carry::Bytes(b) => b,
                    Carry::None => {
                        err_println!("ion-win: from-json: no input piped in");
                        unregister_spawned!();
                        return false;
                    }
                    Carry::Table(_) => {
                        err_println!("ion-win: from-json: input is already a table");
                        unregister_spawned!();
                        return false;
                    }
                    Carry::FileSet(_) => {
                        err_println!("ion-win: from-json: input is already a FileSet");
                        unregister_spawned!();
                        return false;
                    }
                    Carry::Stdio(_) | Carry::Merge(_, _) => {
                        err_println!(
                            "ion-win: from-json: only supported right after 'echo' or a plain \
                             '|' pipe, not '^|'/'&|' yet"
                        );
                        unregister_spawned!();
                        return false;
                    }
                };
                match Table::from_json(&String::from_utf8_lossy(&bytes)) {
                    Ok(table) => {
                        carry = finish_table_stage(
                            table,
                            is_last,
                            stdout_file,
                            capture.as_mut().map(|s| &mut **s),
                        )
                    }
                    Err(e) => {
                        err_println!("ion-win: from-json: {e}");
                        unregister_spawned!();
                        return false;
                    }
                }
            }
            Kind::FromCsv => {
                let bytes = match incoming {
                    Carry::Bytes(b) => b,
                    Carry::None => {
                        err_println!("ion-win: from-csv: no input piped in");
                        unregister_spawned!();
                        return false;
                    }
                    Carry::Table(_) => {
                        err_println!("ion-win: from-csv: input is already a table");
                        unregister_spawned!();
                        return false;
                    }
                    Carry::FileSet(_) => {
                        err_println!("ion-win: from-csv: input is already a FileSet");
                        unregister_spawned!();
                        return false;
                    }
                    Carry::Stdio(_) | Carry::Merge(_, _) => {
                        err_println!(
                            "ion-win: from-csv: only supported right after 'echo'/'cat' or a \
                             plain '|' pipe, not '^|'/'&|' yet"
                        );
                        unregister_spawned!();
                        return false;
                    }
                };
                match Table::from_csv(&String::from_utf8_lossy(&bytes)) {
                    Ok(table) => {
                        carry = finish_table_stage(
                            table,
                            is_last,
                            stdout_file,
                            capture.as_mut().map(|s| &mut **s),
                        )
                    }
                    Err(e) => {
                        err_println!("ion-win: {e}");
                        unregister_spawned!();
                        return false;
                    }
                }
            }
            Kind::Select(columns) => {
                match incoming {
                    Carry::FileSet(fileset) => {
                        carry = finish_fileset_stage(fileset.select(columns), is_last, stdout_file, capture.as_mut().map(|s| &mut **s));
                        continue;
                    }
                    Carry::Table(table) => {
                        carry = finish_table_stage(table.select(columns), is_last, stdout_file, capture.as_mut().map(|s| &mut **s));
                        continue;
                    }
                    Carry::None => {
                        err_println!(
                            "ion-win: select: no table piped in (pipe through 'from-json' first)"
                        );
                        unregister_spawned!();
                        return false;
                    }
                    _ => {
                        err_println!(
                            "ion-win: select: expected a table (pipe through 'from-json' first)"
                        );
                        unregister_spawned!();
                        return false;
                    }
                }
            }
            Kind::Where(where_args) => {
                let incoming = match incoming {
                    Carry::Table(t) => EitherStructured::Table(t),
                    Carry::FileSet(f) => EitherStructured::FileSet(f),
                    Carry::None => {
                        err_println!(
                            "ion-win: where: no table piped in (pipe through 'from-json' first)"
                        );
                        unregister_spawned!();
                        return false;
                    }
                    _ => {
                        err_println!(
                            "ion-win: where: expected a table (pipe through 'from-json' first)"
                        );
                        unregister_spawned!();
                        return false;
                    }
                };
                let [column, op, value] = &where_args[..] else {
                    err_println!(
                        "ion-win: where: usage: where COLUMN OP VALUE (e.g. 'where pid -gt 1000')"
                    );
                    unregister_spawned!();
                    return false;
                };
                if !SUPPORTED_WHERE_OPS.contains(&op.as_str()) {
                    err_println!(
                        "ion-win: where: unsupported operator '{op}' (expected one of {})",
                        SUPPORTED_WHERE_OPS.join(" ")
                    );
                    unregister_spawned!();
                    return false;
                }
                carry = match incoming {
                    EitherStructured::Table(table) => finish_table_stage(table.filter(column, op, value), is_last, stdout_file, capture.as_mut().map(|s| &mut **s)),
                    EitherStructured::FileSet(fileset) => finish_fileset_stage(fileset.filter(column, op, value), is_last, stdout_file, capture.as_mut().map(|s| &mut **s)),
                };
            }
            Kind::DateColumn(args) => {
                let table = match incoming {
                    Carry::Table(table) => table,
                    Carry::None => {
                        err_println!(
                            "ion-win: date-column: no table piped in (pipe through 'from-json'/'from-csv' first)"
                        );
                        unregister_spawned!();
                        return false;
                    }
                    _ => {
                        err_println!(
                            "ion-win: date-column: expected a table (pipe through 'from-json'/'from-csv' first)"
                        );
                        unregister_spawned!();
                        return false;
                    }
                };
                let table = match crate::temporal_column::transform(table, args) {
                    Ok(table) => table,
                    Err(error) => {
                        err_println!("ion-win: {error}");
                        unregister_spawned!();
                        return false;
                    }
                };
                carry = finish_table_stage(
                    table,
                    is_last,
                    stdout_file,
                    capture.as_mut().map(|slot| &mut **slot),
                );
            }
            Kind::Stat(stat_args) => {
                let (arg_files, hash_algo) = match crate::stat::parse_args(stat_args) {
                    Ok(v) => v,
                    Err(e) => {
                        err_println!("ion-win: {e}");
                        unregister_spawned!();
                        return false;
                    }
                };
                // Explicit file arguments always win, matching `cat`'s
                // convention; only fall back to piped-in paths (one per
                // line, e.g. from a future `find`) when none were given.
                let incoming_fileset = if arg_files.is_empty() {
                    match &incoming {
                        Carry::FileSet(fileset) => Some(fileset.clone()),
                        _ => None,
                    }
                } else {
                    None
                };
                let files = if !arg_files.is_empty() {
                    drop(incoming);
                    arg_files
                } else {
                    match incoming {
                        Carry::Bytes(bytes) => String::from_utf8_lossy(&bytes)
                            .lines()
                            .map(str::trim)
                            .filter(|line| !line.is_empty())
                            .map(str::to_string)
                            .collect(),
                        _ => Vec::new(),
                    }
                };
                let fileset = if let Some(fileset) = incoming_fileset {
                    if hash_algo.is_none() {
                        fileset
                    } else {
                        let paths = fileset
                            .files
                            .iter()
                            .map(|record| record.path.to_string_lossy().into_owned())
                            .collect::<Vec<_>>();
                        crate::stat::build_fileset(&paths, hash_algo.as_deref()).await
                    }
                } else {
                    crate::stat::build_fileset(&files, hash_algo.as_deref()).await
                };
                carry = finish_fileset_stage(
                    fileset,
                    is_last,
                    stdout_file,
                    capture.as_mut().map(|s| &mut **s),
                );
            }
            Kind::ToJson => {
                let table = match incoming {
                    Carry::Table(t) => t,
                    Carry::FileSet(f) => f.to_table(),
                    Carry::None => {
                        err_println!(
                            "ion-win: to-json: no table piped in (pipe through 'from-json' first)"
                        );
                        unregister_spawned!();
                        return false;
                    }
                    _ => {
                        err_println!(
                            "ion-win: to-json: expected a table (pipe through 'from-json'/'select' first)"
                        );
                        unregister_spawned!();
                        return false;
                    }
                };
                let mut text = table.to_json();
                text.push('\n');
                if let Some(mut f) = stdout_file {
                    let _ = f.write_all(text.as_bytes());
                } else if is_last {
                    print!("{text}");
                } else {
                    carry = Carry::Bytes(text.into_bytes());
                }
            }
            Kind::ToCsv => {
                let table = match incoming {
                    Carry::Table(t) => t,
                    Carry::FileSet(f) => f.to_table(),
                    Carry::None => {
                        err_println!(
                            "ion-win: to-csv: no table piped in (pipe through 'from-json'/'from-csv' first)"
                        );
                        unregister_spawned!();
                        return false;
                    }
                    _ => {
                        err_println!(
                            "ion-win: to-csv: expected a table (pipe through 'from-json'/'select' first)"
                        );
                        unregister_spawned!();
                        return false;
                    }
                };
                // Unlike ToJson: to_csv() already ends every row (including
                // the last) with its own newline, so no extra one is added.
                let text = table.to_csv();
                if let Some(mut f) = stdout_file {
                    let _ = f.write_all(text.as_bytes());
                } else if is_last {
                    print!("{text}");
                } else {
                    carry = Carry::Bytes(text.into_bytes());
                }
            }
            Kind::Unsupported(_) => unreachable!("checked above"),
        }
    }

    if pipeline.background || pipeline.disown {
        let job_count = children.len();
        // `&` (background) keeps tracking the job for `jobs`/`wait`/
        // `disown`; `&!` (disown) never tracks it at all — matching real
        // shell semantics of "disowned" meaning the shell doesn't manage
        // its lifecycle from the moment it's spawned.
        if pipeline.background {
            let Some(id) = pipeline_execution else {
                err_println!("ion-win: background pipeline has no execution");
                return false;
            };
            if let Err(error) = execution::register_background_children(id, children, command_texts)
            {
                execution::fail_pipeline_execution(id, error.to_string());
                err_println!("ion-win: could not retain background pipeline: {error}");
                return false;
            }
        }
        println!("ion-win: [bg] started {job_count} process(es)");
        return true;
    }

    let ok = match pipeline_execution {
        Some(id) => execution::wait_foreground_pipeline(id, children).unwrap_or(false),
        None => unreachable!("background pipelines returned before foreground wait"),
    };
    for handle in merge_threads {
        let _ = handle.join();
    }
    ok
}

fn pipeline_display(pipeline: &Pipeline) -> String {
    let mut display = String::new();
    for (index, stage) in pipeline.stages.iter().enumerate() {
        if index > 0 {
            let separator = match pipeline.pipes.get(index - 1) {
                Some(PipeKind::Stdout) => " | ",
                Some(PipeKind::Stderr) => " ^| ",
                Some(PipeKind::Combined) => " &| ",
                None => " | ",
            };
            display.push_str(separator);
        }
        display.push_str(
            &stage
                .tokens
                .iter()
                .map(|token| token.text.as_str())
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    display
}

fn classify_stages(pipeline: &Pipeline, interp: &Interpreter) -> Vec<Kind> {
    pipeline
        .stages
        .iter()
        .map(|stage| {
            if stage.tokens.is_empty() {
                return Kind::Unsupported(String::new());
            }
            let args = interp.expand_all(&stage.tokens);
            let cmd = args[0].clone();
            if cmd == "echo" {
                let (rest, no_newline) = crate::interp::split_echo_no_newline_flag(&args[1..]);
                Kind::Echo(rest.to_vec(), no_newline)
            } else if cmd == "cat" {
                Kind::Cat(args[1..].to_vec())
            } else if cmd == "find" {
                Kind::Find(args[1..].to_vec())
            } else if matches!(cmd.as_str(), "files" | "folders" | "dirs") {
                Kind::ListEntries(cmd, args[1..].to_vec())
            } else if cmd == "copy" || cmd == "cp" {
                Kind::Copy(args[1..].to_vec())
            } else if cmd == "move" || cmd == "mv" {
                Kind::Move(args[1..].to_vec())
            } else if cmd == "compress" {
                Kind::Compress(args[1..].to_vec())
            } else if cmd == "delete" {
                Kind::Delete(args[1..].to_vec())
            } else if cmd == "from-json" {
                Kind::FromJson
            } else if cmd == "to-json" {
                Kind::ToJson
            } else if cmd == "from-csv" {
                Kind::FromCsv
            } else if cmd == "to-csv" {
                Kind::ToCsv
            } else if cmd == "select" {
                Kind::Select(args[1..].to_vec())
            } else if cmd == "where" || cmd == "filter" {
                Kind::Where(args[1..].to_vec())
            } else if cmd == "date-column" {
                Kind::DateColumn(args[1..].to_vec())
            } else if cmd == "stat" {
                Kind::Stat(args[1..].to_vec())
            } else if let Some(fileset) = interp.get_fileset(&cmd) {
                Kind::FileSetSource(fileset.clone())
            } else if let Some(table) = interp.get_table(&cmd) {
                Kind::TableSource(table.clone())
            } else if interp.get_function(&cmd).is_some()
                || matches!(
                    cmd.as_str(),
                    "pvar"
                        | "dmark"
                        | "test"
                        | "matches"
                        | "let"
                        | "drop"
                        | "fn"
                        | "read"
                        | "source"
                        | "exit"
                        | "quit"
                        | "help"
                        | "end"
                )
            {
                Kind::Unsupported(cmd)
            } else {
                Kind::External(args)
            }
        })
        .collect()
}

/// Shared terminal-vs-pass-through logic for `from-json`/`select`: if
/// nothing structured-aware follows (this is the last stage, or output is
/// redirected to a file), the table is the final answer — printed/written
/// as pretty JSON. Otherwise it's handed forward as `Carry::Table` so the
/// next stage (`select`, `to-json`, or an external process, which
/// implicitly textifies it — see `Carry::Table`'s arm in the `External`
/// incoming-carry match) can consume it.
fn finish_table_stage(
    table: Table,
    is_last: bool,
    stdout_file: Option<std::fs::File>,
    capture: Option<&mut Option<CapturedStructured>>,
) -> Carry {
    if let Some(mut f) = stdout_file {
        let mut text = table.to_json();
        text.push('\n');
        let _ = f.write_all(text.as_bytes());
        Carry::None
    } else if is_last {
        match capture {
            // `let NAME = ...` (ARCHITECTURE.md §18): store the table
            // instead of printing it.
            Some(slot) => *slot = Some(CapturedStructured::Table(table)),
            None => println!("{}", table.to_json()),
        }
        Carry::None
    } else {
        Carry::Table(table)
    }
}

fn finish_fileset_stage(
    fileset: FileSet,
    is_last: bool,
    stdout_file: Option<std::fs::File>,
    capture: Option<&mut Option<CapturedStructured>>,
) -> Carry {
    if let Some(mut file) = stdout_file {
        let mut text = fileset.to_table().to_json();
        text.push('\n');
        let _ = file.write_all(text.as_bytes());
        Carry::None
    } else if is_last {
        match capture {
            Some(slot) => *slot = Some(CapturedStructured::FileSet(fileset)),
            None => println!("{}", fileset.to_table().to_json()),
        }
        Carry::None
    } else {
        Carry::FileSet(fileset)
    }
}

/// Resolves the file each stream should redirect to, if any: the *last*
/// redirect matching that stream (or `Combined`, which applies to both)
/// wins. If stdout and stderr both resolve to the same `Combined` redirect,
/// the file is opened once and the handle cloned so both streams share it
/// correctly instead of each independently truncating it.
fn resolve_stage_stdio(
    redirects: &[Redirect],
) -> Result<(Option<std::fs::File>, Option<std::fs::File>), String> {
    let mut stdout_pick: Option<&Redirect> = None;
    let mut stderr_pick: Option<&Redirect> = None;

    for r in redirects {
        match r.stream {
            Stream::Stdout => stdout_pick = Some(r),
            Stream::Stderr => stderr_pick = Some(r),
            Stream::Combined => {
                stdout_pick = Some(r);
                stderr_pick = Some(r);
            }
        }
    }

    let open = |r: &Redirect| -> Result<std::fs::File, String> {
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .append(r.append)
            .truncate(!r.append)
            .open(&r.path)
            .map_err(|e| format!("{}: {e}", r.path))
    };

    match (stdout_pick, stderr_pick) {
        (Some(so), Some(se)) if std::ptr::eq(so, se) => {
            let f = open(so)?;
            let f2 = f.try_clone().map_err(|e| e.to_string())?;
            Ok((Some(f), Some(f2)))
        }
        (so, se) => Ok((so.map(open).transpose()?, se.map(open).transpose()?)),
    }
}
