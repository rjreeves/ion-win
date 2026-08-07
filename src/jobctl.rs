//! Ctrl+C interrupt plumbing (ion-manual page 65, "Signal Handling":
//! "SIGINT (Ctrl+C): Interrupt the running program with a signal to
//! terminate").
//!
//! Two independent halves, since Ctrl+C needs to reach two different
//! kinds of "currently running thing":
//!
//! 1. **A foreground external process** (or several, for a pipeline) —
//!    reached by forwarding a real console control event to it. See
//!    `register_foreground`/`interrupt`.
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
use std::sync::{Mutex, OnceLock};

static FOREGROUND_PIDS: OnceLock<Mutex<Vec<u32>>> = OnceLock::new();
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

fn registry() -> &'static Mutex<Vec<u32>> {
    FOREGROUND_PIDS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Marks a spawned child's PID as part of the current foreground job, so
/// Ctrl+C will forward to it. Don't call this for backgrounded (`&`) or
/// disowned (`&!`) processes — only the foreground job should be
/// interruptible.
pub fn register_foreground(pid: u32) {
    if let Ok(mut pids) = registry().lock() {
        pids.push(pid);
    }
}

/// Unregisters a child once it's been waited on (or will never be waited
/// on, e.g. it failed to spawn a sibling stage).
pub fn unregister_foreground(pid: u32) {
    if let Ok(mut pids) = registry().lock() {
        pids.retain(|&p| p != pid);
    }
}

/// Called from the Ctrl+C handler thread: forwards a console interrupt to
/// every registered foreground PID and sets the cooperative flag for
/// pure-Ion loops to notice. Never panics — this runs on a signal-handling
/// thread where a panic would be especially unwelcome.
pub fn request_interrupt() {
    INTERRUPTED.store(true, Ordering::SeqCst);

    let Ok(pids) = registry().lock() else { return };
    for &pid in pids.iter() {
        forward_ctrl_c(pid);
    }
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
/// foreground job (see `register_foreground`).
pub fn new_command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP);
    }
    command
}

/// Registers a freshly-spawned child as the foreground job, waits for it,
/// then unregisters it — shared by every single-process external-command
/// spawn site. Pipeline stages (potentially several children at once)
/// register/unregister themselves individually instead; see
/// `pipeline_exec.rs`.
pub fn wait_foreground(mut child: std::process::Child) -> std::io::Result<std::process::ExitStatus> {
    let pid = child.id();
    register_foreground(pid);
    let result = child.wait();
    unregister_foreground(pid);
    result
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

    // These tests share the module's global statics, so they're combined
    // into one #[test] rather than run in parallel — otherwise one test's
    // register/unregister could interleave with another's flag check.
    #[test]
    fn registry_and_interrupt_flag_behave() {
        // Start clean regardless of test execution order.
        take_interrupt();
        while registry().lock().unwrap().pop().is_some() {}

        assert!(!take_interrupt(), "flag should start clear");

        register_foreground(4242);
        register_foreground(4243);
        assert_eq!(*registry().lock().unwrap(), vec![4242, 4243]);

        unregister_foreground(4242);
        assert_eq!(*registry().lock().unwrap(), vec![4243]);

        request_interrupt(); // sets the flag; forwarding to a fake PID is a harmless no-op
        assert!(interrupt_requested(), "non-consuming check should see the flag");
        assert!(interrupt_requested(), "non-consuming check must leave the flag set");
        assert!(take_interrupt(), "request_interrupt should set the flag");
        assert!(!take_interrupt(), "flag should be consumed by the first take");

        unregister_foreground(4243);
        assert!(registry().lock().unwrap().is_empty());
    }
}
