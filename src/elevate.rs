//! Windows-native elevated process launch through the Shell `runas` verb.
//! This launches one program rather than elevating an in-process builtin or
//! attempting to transport a pipeline across the UAC process boundary.

use std::path::PathBuf;

const USAGE: &str = "usage: elevate [--wait] [--cwd DIRECTORY] PROGRAM [ARGS...]";

#[derive(Debug, Eq, PartialEq)]
struct Options {
    wait: bool,
    cwd: Option<PathBuf>,
    program: String,
    arguments: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    Launched,
    Exited(u32),
}

impl Outcome {
    pub fn success(self) -> bool {
        matches!(self, Self::Launched | Self::Exited(0))
    }
}

#[cfg(windows)]
pub fn is_elevated() -> Result<bool, String> {
    use windows_sys::Win32::Foundation::PSID;
    use windows_sys::Win32::Security::{
        AllocateAndInitializeSid, CheckTokenMembership, FreeSid, SECURITY_NT_AUTHORITY,
    };
    use windows_sys::Win32::System::SystemServices::{
        DOMAIN_ALIAS_RID_ADMINS, SECURITY_BUILTIN_DOMAIN_RID,
    };

    let mut administrators_sid: PSID = std::ptr::null_mut();
    let allocated = unsafe {
        AllocateAndInitializeSid(
            &SECURITY_NT_AUTHORITY,
            2,
            SECURITY_BUILTIN_DOMAIN_RID as u32,
            DOMAIN_ALIAS_RID_ADMINS as u32,
            0,
            0,
            0,
            0,
            0,
            0,
            &mut administrators_sid,
        )
    };
    if allocated == 0 {
        return Err(format!(
            "could not create the Administrators group SID: {}",
            std::io::Error::last_os_error()
        ));
    }

    let mut member = 0;
    let checked = unsafe { CheckTokenMembership(0, administrators_sid, &mut member) };
    let error = if checked == 0 {
        Some(std::io::Error::last_os_error())
    } else {
        None
    };
    unsafe {
        FreeSid(administrators_sid);
    }
    match error {
        Some(error) => Err(format!(
            "could not inspect the current process token: {error}"
        )),
        None => Ok(member != 0),
    }
}

#[cfg(not(windows))]
pub fn is_elevated() -> Result<bool, String> {
    Err("elevation status is only available on Windows".to_string())
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut wait = false;
    let mut cwd = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--wait" => {
                wait = true;
                index += 1;
            }
            "--cwd" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("--cwd requires a directory\n{USAGE}"))?;
                cwd = Some(PathBuf::from(value));
                index += 2;
            }
            "--" => {
                index += 1;
                break;
            }
            "--help" | "-h" => return Err(USAGE.to_string()),
            option if option.starts_with('-') => {
                return Err(format!("unknown option '{option}'\n{USAGE}"));
            }
            _ => break,
        }
    }
    let program = args
        .get(index)
        .cloned()
        .ok_or_else(|| format!("missing program\n{USAGE}"))?;
    Ok(Options {
        wait,
        cwd,
        program,
        arguments: args[index + 1..].to_vec(),
    })
}

/// Applies the quoting convention understood by CommandLineToArgvW-compatible
/// programs. Backslashes preceding a quote, and trailing backslashes inside a
/// quoted argument, must be doubled to preserve the original argument.
fn quote_argument(argument: &str) -> String {
    if !argument.is_empty()
        && !argument
            .chars()
            .any(|character| character.is_whitespace() || character == '"')
    {
        return argument.to_string();
    }

    let mut quoted = String::from("\"");
    let mut backslashes = 0usize;
    for character in argument.chars() {
        if character == '\\' {
            backslashes += 1;
        } else if character == '"' {
            quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
            quoted.push('"');
            backslashes = 0;
        } else {
            quoted.push_str(&"\\".repeat(backslashes));
            backslashes = 0;
            quoted.push(character);
        }
    }
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

fn parameter_string(arguments: &[String]) -> String {
    arguments
        .iter()
        .map(|argument| quote_argument(argument))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(windows)]
pub fn run(args: &[String]) -> Result<Outcome, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, WAIT_FAILED};
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    fn wide(value: &std::ffi::OsStr) -> Vec<u16> {
        value.encode_wide().chain(std::iter::once(0)).collect()
    }

    let options = parse_options(args)?;
    if let Some(directory) = &options.cwd {
        if !directory.is_dir() {
            return Err(format!(
                "working directory does not exist: {}",
                directory.display()
            ));
        }
    }

    let program = wide(std::ffi::OsStr::new(&options.program));
    let parameters = wide(std::ffi::OsStr::new(&parameter_string(&options.arguments)));
    let directory = options.cwd.as_ref().map(|path| wide(path.as_os_str()));
    let verb = wide(std::ffi::OsStr::new("runas"));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(program.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        lpDirectory: directory
            .as_ref()
            .map_or(PCWSTR::null(), |value| PCWSTR(value.as_ptr())),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };

    if let Err(error) = unsafe { ShellExecuteExW(&mut info) } {
        if error.code().0 as u32 == 1223 {
            return Err("UAC consent was cancelled".to_string());
        }
        return Err(format!("could not start '{}': {error}", options.program));
    }
    if info.hProcess.is_invalid() {
        return Err(format!(
            "Windows did not return a process handle for '{}'",
            options.program
        ));
    }

    if !options.wait {
        let _ = unsafe { CloseHandle(info.hProcess) };
        return Ok(Outcome::Launched);
    }

    if unsafe { WaitForSingleObject(info.hProcess, INFINITE) } == WAIT_FAILED {
        let _ = unsafe { CloseHandle(info.hProcess) };
        return Err(format!("waiting for '{}' failed", options.program));
    }
    let mut exit_code = 0u32;
    let result = unsafe { GetExitCodeProcess(info.hProcess, &mut exit_code) };
    let _ = unsafe { CloseHandle(info.hProcess) };
    result.map_err(|error| format!("could not read '{}' exit code: {error}", options.program))?;
    Ok(Outcome::Exited(exit_code))
}

#[cfg(not(windows))]
pub fn run(args: &[String]) -> Result<Outcome, String> {
    let _ = parse_options(args)?;
    Err("elevation is only available on Windows".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn parses_options_only_before_the_program() {
        assert_eq!(
            parse_options(&strings(&[
                "--wait", "--cwd", "C:\\Work", "tool.exe", "--quiet"
            ]))
            .unwrap(),
            Options {
                wait: true,
                cwd: Some(PathBuf::from("C:\\Work")),
                program: "tool.exe".to_string(),
                arguments: strings(&["--quiet"]),
            }
        );
    }

    #[test]
    fn option_terminator_allows_a_dash_prefixed_program() {
        assert_eq!(
            parse_options(&strings(&["--", "-tool"])).unwrap().program,
            "-tool"
        );
    }

    #[test]
    fn rejects_missing_values_and_program() {
        assert!(parse_options(&strings(&["--cwd"])).is_err());
        assert!(parse_options(&strings(&["--wait"])).is_err());
        assert!(parse_options(&strings(&["--unknown", "tool.exe"])).is_err());
    }

    #[test]
    fn quotes_windows_arguments_without_changing_boundaries() {
        assert_eq!(quote_argument("plain"), "plain");
        assert_eq!(quote_argument(""), "\"\"");
        assert_eq!(quote_argument("two words"), "\"two words\"");
        assert_eq!(quote_argument("say\"hello"), "\"say\\\"hello\"");
        assert_eq!(
            quote_argument("C:\\path with space\\"),
            "\"C:\\path with space\\\\\""
        );
    }

    #[test]
    fn wait_outcome_reflects_the_child_exit_code() {
        assert!(Outcome::Launched.success());
        assert!(Outcome::Exited(0).success());
        assert!(!Outcome::Exited(5).success());
    }

    #[cfg(windows)]
    #[test]
    fn current_token_elevation_can_be_inspected_without_prompting() {
        assert!(is_elevated().is_ok());
    }
}
