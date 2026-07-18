//! Executes a parsed `pipeline::Pipeline` against real OS processes.
//!
//! Supported: `|`/`^|`/`&|` piping between external commands, `>`/`>>`/
//! `^>`/`&>` redirection to files, `echo` as a producer (writing into the
//! next stage's stdin or a redirect target), and `&`/`&!` (spawn without
//! waiting — `&` registers with `jobs.rs` for `jobs`/`wait`/`disown`;
//! `&!` never does, no `fg`/`bg` — see `jobs.rs`'s module doc for why).
//!
//! Not supported as a pipeline stage: any other builtin (`pvar`, `dmark`,
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
use crate::interp::Interpreter;
use crate::jobctl;
use crate::jobs;
use crate::pipeline::{PipeKind, Pipeline, Redirect, Stream};
use crate::state::StateHandle;
use crate::table::Table;
use std::io::{self, Read, Write};
use std::process::{Child, ChildStderr, ChildStdout, Stdio};
use std::sync::{Arc, Mutex};

enum Kind {
    /// Args plus whether the trailing newline is suppressed (`echo -n`).
    Echo(Vec<String>, bool),
    External(Vec<String>),
    /// Structured-data pipeline stages (`ARCHITECTURE.md` §17) — an
    /// in-process object bridge, since external processes only ever see
    /// JSON text, never a `Table` value directly.
    FromJson,
    ToJson,
    Select(Vec<String>),
    /// `where`/`filter COLUMN OP VALUE` — validated at execution time
    /// (arg count and operator), not here, matching how `FromJson`
    /// validates JSON-parseability during execution rather than
    /// classification.
    Where(Vec<String>),
    /// A bare reference to a `Table` variable (`ARCHITECTURE.md` §18),
    /// used as an independent pipeline source — like `Echo`, it ignores
    /// whatever fed it rather than trying to merge the two. Resolved once
    /// at classification time (the table is cloned in), not re-looked-up
    /// at execution time.
    TableSource(Table),
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
        Some(Kind::FromJson) | Some(Kind::Select(_)) | Some(Kind::ToJson) | Some(Kind::Where(_))
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
) -> (bool, Option<Table>) {
    let mut captured = None;
    let ok = run_impl(pipeline, interp, state, Some(&mut captured)).await;
    (ok, captured)
}

async fn run_impl(
    pipeline: &Pipeline,
    interp: &mut Interpreter,
    _state: &StateHandle,
    mut capture: Option<&mut Option<Table>>,
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

    let mut children: Vec<Child> = Vec::new();
    // Parallel to `children` (same index), for `jobs::register`'s display
    // text if this pipeline turns out to be backgrounded.
    let mut command_texts: Vec<String> = Vec::new();
    let mut merge_threads: Vec<std::thread::JoinHandle<()>> = Vec::new();
    let mut carry = Carry::None;

    // Unregisters whatever's been spawned so far — used on every early-exit
    // path below so a mid-pipeline failure doesn't leave stale entries in
    // the foreground registry.
    macro_rules! unregister_spawned {
        () => {
            if is_foreground {
                for child in &children {
                    jobctl::unregister_foreground(child.id());
                }
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
            Kind::External(args) => {
                let mut command = jobctl::new_command(&args[0]);
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
                if is_foreground {
                    jobctl::register_foreground(child.id());
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
            Kind::Select(columns) => {
                let table = match incoming {
                    Carry::Table(t) => t,
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
                };
                carry = finish_table_stage(
                    table.select(columns),
                    is_last,
                    stdout_file,
                    capture.as_mut().map(|s| &mut **s),
                );
            }
            Kind::Where(where_args) => {
                let table = match incoming {
                    Carry::Table(t) => t,
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
                carry = finish_table_stage(
                    table.filter(column, op, value),
                    is_last,
                    stdout_file,
                    capture.as_mut().map(|s| &mut **s),
                );
            }
            Kind::ToJson => {
                let table = match incoming {
                    Carry::Table(t) => t,
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
            for (child, command) in children.into_iter().zip(command_texts) {
                let pid = child.id();
                jobs::register(pid, command, child);
            }
        }
        println!("ion-win: [bg] started {job_count} process(es)");
        return true;
    }

    let mut ok = true;
    for mut child in children {
        let pid = child.id();
        ok = child.wait().map(|s| s.success()).unwrap_or(false);
        jobctl::unregister_foreground(pid);
    }
    for handle in merge_threads {
        let _ = handle.join();
    }
    ok
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
            } else if cmd == "from-json" {
                Kind::FromJson
            } else if cmd == "to-json" {
                Kind::ToJson
            } else if cmd == "select" {
                Kind::Select(args[1..].to_vec())
            } else if cmd == "where" || cmd == "filter" {
                Kind::Where(args[1..].to_vec())
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
    capture: Option<&mut Option<Table>>,
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
            Some(slot) => *slot = Some(table),
            None => println!("{}", table.to_json()),
        }
        Carry::None
    } else {
        Carry::Table(table)
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
