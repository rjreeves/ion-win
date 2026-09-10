//! PostgreSQL convenience workflow: wait for a server, then open an
//! interactive `psql` session on the same connection.

use crate::execution::{ExecutionMode, ExecutionSpec, ExecutionTarget};
use std::process::Stdio;
use std::time::Duration;

const DEFAULT_HOST: &str = "localhost";
const DEFAULT_PORT: &str = "5432";
const DEFAULT_DATABASE: &str = "postgres";
const DEFAULT_USER: &str = "postgres";
const RETRY_DELAY: Duration = Duration::from_secs(1);

#[derive(Debug, Eq, PartialEq)]
struct Options {
    host: String,
    port: String,
    database: String,
    user: String,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            host: DEFAULT_HOST.into(),
            port: DEFAULT_PORT.into(),
            database: DEFAULT_DATABASE.into(),
            user: DEFAULT_USER.into(),
        }
    }
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut index = 0;
    while index < args.len() {
        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("{} requires a value", args[index]))?;
        match args[index].as_str() {
            "--host" | "-h" => options.host = value.clone(),
            "--port" | "-p" => options.port = value.clone(),
            "--database" | "-d" => options.database = value.clone(),
            "--user" | "-U" => options.user = value.clone(),
            unknown => return Err(format!("unknown option '{unknown}'")),
        }
        index += 2;
    }
    Ok(options)
}

/// Waits indefinitely while PostgreSQL is starting, then runs `psql` with
/// inherited terminal handles. Consequently this call returns only when the
/// user exits psql (or interrupts the wait).
pub fn connect(args: &[String]) -> bool {
    let options = match parse_options(args) {
        Ok(options) => options,
        Err(error) => {
            crate::err_println!("ion-win: pg-connect: {error}");
            crate::err_println!(
                "usage: pg-connect [--host HOST] [--port PORT] [--database DATABASE] [--user USER]"
            );
            return false;
        }
    };

    let Some(pg_isready) = crate::command_resolver::resolve("pg_isready") else {
        crate::err_println!("ion-win: pg-connect: pg_isready was not found on PATH");
        return false;
    };
    let Some(psql) = crate::command_resolver::resolve("psql") else {
        crate::err_println!("ion-win: pg-connect: psql was not found on PATH");
        return false;
    };

    let probe_args = [
        "-q",
        "-h",
        options.host.as_str(),
        "-p",
        options.port.as_str(),
        "-U",
        options.user.as_str(),
        "-d",
        options.database.as_str(),
    ];
    println!(
        "Waiting for PostgreSQL at {}:{} ...",
        options.host, options.port
    );
    loop {
        if crate::jobctl::take_interrupt() {
            println!("^C");
            return false;
        }
        match crate::jobctl::new_command(&pg_isready)
            .args(probe_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            Ok(status) if status.success() => break,
            Ok(_) => std::thread::sleep(RETRY_DELAY),
            Err(error) => {
                crate::err_println!("ion-win: pg-connect: readiness check failed: {error}");
                return false;
            }
        }
    }

    println!("PostgreSQL is ready; opening psql as {}.", options.user);
    let psql_args = vec![
        "-h".into(),
        options.host,
        "-p".into(),
        options.port,
        "-U".into(),
        options.user,
        "-d".into(),
        options.database,
    ];
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            crate::err_println!("ion-win: pg-connect: could not read current directory: {error}");
            return false;
        }
    };
    let spec = ExecutionSpec::new(
        ExecutionTarget::External {
            program: psql,
            args: psql_args,
        },
        cwd,
        ExecutionMode::Foreground,
    );

    match crate::execution::run_foreground_spec(spec) {
        Ok(status) => status.success(),
        Err(error) => {
            crate::err_println!("ion-win: pg-connect: failed to open psql: {error}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }

    #[test]
    fn defaults_target_local_postgres() {
        assert_eq!(parse_options(&[]).unwrap(), Options::default());
    }

    #[test]
    fn connection_target_can_be_overridden() {
        assert_eq!(
            parse_options(&strings(&[
                "--host",
                "db",
                "-p",
                "5544",
                "-d",
                "application"
            ]))
            .unwrap(),
            Options {
                host: "db".into(),
                port: "5544".into(),
                database: "application".into(),
                user: DEFAULT_USER.into(),
            }
        );
    }

    #[test]
    fn connection_user_can_be_overridden() {
        assert_eq!(
            parse_options(&strings(&["--user", "application_user"]))
                .unwrap()
                .user,
            "application_user"
        );
    }

    #[test]
    fn malformed_options_are_rejected() {
        assert!(parse_options(&strings(&["--host"])).is_err());
        assert!(parse_options(&strings(&["--bogus", "value"])).is_err());
    }
}
