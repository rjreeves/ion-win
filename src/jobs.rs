//! Background job tracking for `jobs`/`wait`/`disown` (ion-manual pages
//! 68-83). Distinct from `jobctl.rs`'s foreground-process Ctrl+C plumbing:
//! a job enters this registry only when it's running in the background via
//! plain `&` (never `&!` — a disowned job is, by definition, never tracked
//! at all, matching real shell semantics), and leaves either when
//! `jobs`/`wait` notices it has exited, or when `disown` explicitly stops
//! tracking it.
//!
//! Only `jobs`, `wait`, and `disown` are implemented here — not `fg`/`bg`.
//! Their real value ("resume a job I stopped with Ctrl+Z") has no clean
//! Windows equivalent: there's no POSIX-style SIGSTOP/SIGCONT, and
//! ion-win doesn't implement job-stopping at all, so a "faithful" `fg`/`bg`
//! would either fake it with something fragile or just not really do what
//! the name implies. Deliberately skipped rather than shipped
//! half-faithful — see `ARCHITECTURE.md`.

use std::process::Child;
use std::sync::{Mutex, OnceLock};

struct Job {
    pid: u32,
    command: String,
    child: Child,
}

static JOBS: OnceLock<Mutex<Vec<Job>>> = OnceLock::new();

fn registry() -> &'static Mutex<Vec<Job>> {
    JOBS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Registers a newly-spawned background job. Only ever called for plain
/// `&` (never `&!`/disown) — see `pipeline_exec.rs`.
pub fn register(pid: u32, command: String, child: Child) {
    if let Ok(mut jobs) = registry().lock() {
        jobs.push(Job { pid, command, child });
    }
}

/// `jobs` (ion-manual page 75): lists every still-running background job
/// as `(pid, command)`, pruning any that have already exited (checked via
/// `try_wait`, which never blocks).
pub fn list() -> Vec<(u32, String)> {
    let Ok(mut jobs) = registry().lock() else {
        return Vec::new();
    };
    jobs.retain_mut(|j| matches!(j.child.try_wait(), Ok(None)));
    jobs.iter().map(|j| (j.pid, j.command.clone())).collect()
}

/// `wait` (ion-manual page 83): blocks until every currently-tracked
/// background job has finished; the registry is empty afterward.
pub fn wait_all() {
    let Ok(mut jobs) = registry().lock() else {
        return;
    };
    for mut job in jobs.drain(..) {
        let _ = job.child.wait();
    }
}

/// `disown [PID...]` (ion-manual page 69): stops tracking the given jobs
/// — or every job, if `pids` is empty (bare `disown`, `disown -a`, and
/// `disown -r` are all treated this way; see `shell.rs`'s `handle_disown`)
/// — without waiting for them. They keep running; the shell just no
/// longer knows about them. Returns how many jobs were actually disowned.
pub fn disown(pids: &[u32]) -> usize {
    let Ok(mut jobs) = registry().lock() else {
        return 0;
    };
    let before = jobs.len();
    if pids.is_empty() {
        jobs.clear();
    } else {
        jobs.retain(|j| !pids.contains(&j.pid));
    }
    before - jobs.len()
}
