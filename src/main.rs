mod arith;
mod builtin_names;
mod builtins;
mod clipboard;
mod colorout;
mod command_resolver;
mod compress;
mod copy;
mod delete;
mod editor;
mod execution;
mod fs_builtins;
mod fs_ops;
mod functions;
mod history;
mod interp;
mod jobctl;
#[cfg(windows)]
mod job_object;
mod keyboard_input;
mod methods;
mod pipeline;
mod pipeline_exec;
mod procexpand;
mod ranges;
mod shell;
mod state;
mod stat;
mod table;
mod temporal;
mod temporal_column;
mod types;

use std::path::PathBuf;

#[tokio::main]
async fn main() {
    // SIGINT (Ctrl+C) should interrupt whatever's currently running — a
    // foreground external process/pipeline, or a pure-Ion loop with no
    // external process at all — without killing the shell itself. See
    // jobctl.rs for how both halves work; this handler runs on its own
    // dedicated thread (per the `ctrlc` crate) so it can react even while
    // the main thread is blocked in a tight loop or a `Child::wait()`.
    ctrlc::set_handler(jobctl::request_interrupt).expect("failed to install Ctrl+C handler");

    let cli_args: Vec<String> = std::env::args().collect();
    adjust_interactive_startup_dir(&cli_args);

    let db_path = state_db_path();
    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let state_handle = match state::spawn(db_path) {
        Ok(handle) => handle,
        Err(redb::Error::DatabaseAlreadyOpen) => {
            eprintln!(
                "ion-win: state database is already open; this window will use temporary pvar/dmark state"
            );
            state::spawn_memory()
        }
        Err(e) => panic!("failed to open ion-win state database: {e}"),
    };

    // `ion-win.exe script.ion arg1 arg2` runs the script non-interactively,
    // exposing argv[1..] to it as the `@args` array (args[0] = the script's
    // own path, matching ion-manual's "Script Executions"). With no
    // arguments, fall back to the interactive/piped REPL as before.
    if cli_args.len() > 1 {
        let script_args = cli_args[1..].to_vec();
        let exit_code = shell::run_script(&cli_args[1], script_args, state_handle).await;
        std::process::exit(exit_code);
    }

    shell::run(state_handle).await;
}

/// Windows sometimes starts a double-clicked console program in a generic
/// Windows directory instead of a useful working folder. Preserve any real
/// inherited directory, but recover from that default for interactive shells.
fn adjust_interactive_startup_dir(cli_args: &[String]) {
    if cli_args.len() > 1 {
        return;
    }

    #[cfg(windows)]
    {
        if !fresh_console_process() || !current_dir_is_windows_default() {
            return;
        }

        if let Ok(exe) = std::env::current_exe() {
            if let Some(parent) = exe.parent() {
                let _ = std::env::set_current_dir(parent);
            }
        }
    }
}

#[cfg(windows)]
fn fresh_console_process() -> bool {
    use windows_sys::Win32::System::Console::GetConsoleProcessList;

    let mut processes = [0u32; 8];
    let count = unsafe { GetConsoleProcessList(processes.as_mut_ptr(), processes.len() as u32) };
    count <= 1
}

#[cfg(windows)]
fn current_dir_is_windows_default() -> bool {
    let Ok(current) = std::env::current_dir().and_then(|p| p.canonicalize()) else {
        return false;
    };
    let Ok(windir) = std::env::var("WINDIR") else {
        return false;
    };
    let Ok(windir) = PathBuf::from(windir).canonicalize() else {
        return false;
    };

    current == windir || current == windir.join("System32") || current == windir.join("SysWOW64")
}

/// Resolves the path to the persistent state database, preferring
/// `%APPDATA%\ion-win\state.redb` on Windows and falling back to the
/// current directory everywhere else (e.g. during local dev on non-Windows).
fn state_db_path() -> PathBuf {
    if let Ok(appdata) = std::env::var("APPDATA") {
        return PathBuf::from(appdata).join("ion-win").join("state.redb");
    }
    PathBuf::from("ion-win-state.redb")
}
