//! Executes a parsed `pipeline::Pipeline` against real OS processes.
//!
//! Supported: `|`/`^|`/`&|` piping between external commands, `>`/`>>`/
//! `^>`/`&>` redirection to files, `echo` as a producer (writing into the
//! next stage's stdin or a redirect target), and `&`/`&!` (spawn without
//! waiting — no `jobs`/`bg`/`fg` job-control tracking yet).
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
use crate::pipeline::{PipeKind, Pipeline, Redirect, Stream};
use crate::state::StateHandle;
use std::io::{self, Write};
use std::process::{Child, ChildStderr, ChildStdout, Stdio};
use std::sync::{Arc, Mutex};

enum Kind {
    Echo(Vec<String>),
    External(Vec<String>),
    /// A builtin/function name that isn't yet supported as a pipeline
    /// stage. Empty string means the stage had no command at all (e.g. a
    /// stray `|`).
    Unsupported(String),
}

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
pub async fn run(pipeline: &Pipeline, interp: &mut Interpreter, _state: &StateHandle) -> bool {
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
            Kind::Echo(args) => {
                let text = args.join(" ") + "\n";
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

                carry = match next_pipe_kind {
                    None => Carry::None,
                    Some(PipeKind::Stdout) => child
                        .stdout
                        .take()
                        .map(Stdio::from)
                        .map(Carry::Stdio)
                        .unwrap_or(Carry::None),
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

                children.push(child);
            }
            Kind::Unsupported(_) => unreachable!("checked above"),
        }
    }

    if pipeline.background || pipeline.disown {
        println!("ion-win: [bg] started {} process(es)", children.len());
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
                Kind::Echo(args[1..].to_vec())
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
