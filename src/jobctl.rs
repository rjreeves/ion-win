//! Ctrl+C interrupt plumbing (ion-manual page 65, "Signal Handling":
//! "SIGINT (Ctrl+C): Interrupt the running program with a signal to
//! terminate").
//!
//! Two independent halves, since Ctrl+C needs to reach two different
//! kinds of "currently running thing":
//!
//! 1. **A foreground external process** (or several, for a pipeline) —
//!    reached by forwarding a real console control event to it. See
//!    PID targets are read from active foreground executions in
//!    `ExecutionManager` and signalled here.
//! 2. **A pure-Ion loop with no external process at all** (`while true;
//!    end` with only builtins/expansions inside) — there's no OS-level
//!    signal target for that; the running Rust code has to periodically
//!    check a shared flag itself. See `interrupted`/`clear_interrupt`.
//!
//! Both are driven from the single `ctrlc::set_handler` callback installed
//! in `main.rs`, which runs on its own dedicated thread (per the `ctrlc`
//! crate), concurrently with whatever the main thread is doing — that's
//! what makes it possible to react to Ctrl+C while the main thread is
//! blocked in a tight loop or a `Child::wait()`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
static INTERRUPTED: AtomicBool = AtomicBool::new(false);
const CANCELLATION_GRACE: Duration = Duration::from_millis(750);

/// Called from the Ctrl+C handler thread: forwards a console interrupt to
/// every active foreground execution PID and sets the cooperative flag for
/// pure-Ion loops to notice. Never panics — this runs on a signal-handling
/// thread where a panic would be especially unwelcome.
pub fn request_interrupt() {
    INTERRUPTED.store(true, Ordering::SeqCst);

    let cancellation = crate::execution::request_foreground_cancellation();
    for pid in cancellation.process_ids {
        forward_ctrl_c(pid);
    }
    if !cancellation.execution_ids.is_empty() {
        std::thread::spawn(move || {
            std::thread::sleep(CANCELLATION_GRACE);
            crate::execution::escalate_cancellation(&cancellation.execution_ids);
        });
    }
}

/// Cancels one execution selected by the user-facing `exec cancel` command.
/// This uses the same cooperative Ctrl+Break-first and Job Object escalation
/// policy as an interactive Ctrl+C, without setting the pure-Ion loop flag.
pub fn cancel_execution(id: crate::execution::ExecutionId) -> Result<(), String> {
    let cancellation = crate::execution::request_cancellation(id)?;
    for pid in cancellation.process_ids {
        forward_ctrl_c(pid);
    }
    if !cancellation.execution_ids.is_empty() {
        std::thread::spawn(move || {
            std::thread::sleep(CANCELLATION_GRACE);
            crate::execution::escalate_cancellation(&cancellation.execution_ids);
        });
    }
    Ok(())
}

/// Checks (and clears) the cooperative interrupt flag. `while`/`for` loop
/// bodies in `shell.rs` poll this once per iteration; a `true` result
/// means "stop looping and unwind back to the prompt," analogous to how a
/// real shell aborts the current command line on Ctrl+C.
pub fn take_interrupt() -> bool {
    INTERRUPTED.swap(false, Ordering::SeqCst)
}

/// Observes the cooperative interrupt flag without consuming it. Long-running
/// in-process builtins use this from worker threads so every worker can notice
/// the same Ctrl+C request; the coordinating task consumes it once cleanup is
/// complete with `take_interrupt`.
pub fn interrupt_requested() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// Builds a `Command` for `program`, isolated into its own Windows process
/// group. By default, Windows delivers a console Ctrl+C event to *every*
/// process sharing the console — including this shell itself and every
/// child it has ever spawned, all at once, with no way to target just one.
/// Putting each child in its own process group opts it out of that blanket
/// delivery, so `request_interrupt` can instead explicitly forward the
/// event only to whichever PID(s) are currently registered as the
/// foreground execution (tracked by `ExecutionManager`).
pub fn new_command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP);
    }
    command
}

/// Waits for a foreground child. PID discovery for Ctrl+C now comes from
/// `ExecutionManager`; this remains the process-wait backend entry point.
pub fn wait_foreground(
    mut child: std::process::Child,
) -> std::io::Result<std::process::ExitStatus> {
    child.wait()
}

#[cfg(windows)]
fn forward_ctrl_c(pid: u32) {
    use windows_sys::Win32::System::Console::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};
    // Windows only lets GenerateConsoleCtrlEvent target a *specific* process
    // group with CTRL_BREAK_EVENT — CTRL_C_EVENT can only ever be broadcast
    // to every process on the console (dwProcessGroupId must be 0 for it).
    // Sending CTRL_C_EVENT with our child's PID here is silently a no-op.
    // CTRL_BREAK_EVENT is the documented workaround: nearly every console
    // program without its own SetConsoleCtrlHandler (ping, timeout, etc.)
    // terminates on it exactly like CTRL_C_EVENT, and since the child was
    // spawned with CREATE_NEW_PROCESS_GROUP (see `new_command`), its PID
    // doubles as its process group ID, so only it (not the shell) is hit.
    //
    // SAFETY: just posts a console control event to the given process
    // group ID; passing a PID that has already exited (a benign race — the
    // child may finish between registration and this call) is a
    // documented, harmless failure mode (returns an error we don't need to
    // check here).
    unsafe {
        GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid);
    }
}

#[cfg(not(windows))]
fn forward_ctrl_c(_pid: u32) {
    // No-op off Windows (this project targets Windows; anything else is
    // just local dev convenience).
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupt_flag_behaves() {
        take_interrupt();
        assert!(!take_interrupt(), "flag should start clear");
        request_interrupt();
        assert!(
            interrupt_requested(),
            "non-consuming check should see the flag"
        );
        assert!(
            interrupt_requested(),
            "non-consuming check must leave the flag set"
        );
        assert!(take_interrupt(), "request_interrupt should set the flag");
        assert!(
            !take_interrupt(),
            "flag should be consumed by the first take"
        );
    }
}
