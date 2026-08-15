//! Persistent task definitions, separate from live execution state.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::execution::{ExecutionLimits, ExecutionMode, ExecutionSpec, ExecutionTarget};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskDefinition {
    name: String,
    command: String,
    args: Vec<String>,
    cwd: PathBuf,
    timeout_ms: Option<u64>,
    memory_bytes: Option<u64>,
    max_processes: Option<u32>,
}

impl TaskDefinition {
    pub fn new(
        name: impl Into<String>,
        command: impl Into<String>,
        args: Vec<String>,
        cwd: PathBuf,
    ) -> Result<Self, String> {
        let name = name.into();
        if name.is_empty()
            || !name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
        {
            return Err(
                "task name must contain only ASCII letters, digits, '-', '_', or '.'".into(),
            );
        }
        let command = command.into();
        if command.is_empty() {
            return Err("task command cannot be empty".into());
        }
        if !cwd.is_absolute() {
            return Err("task working directory must be absolute".into());
        }
        Ok(Self {
            name,
            command,
            args,
            cwd,
            timeout_ms: None,
            memory_bytes: None,
            max_processes: None,
        })
    }

    pub fn with_policy(
        mut self,
        timeout: Option<Duration>,
        memory_bytes: Option<u64>,
        max_processes: Option<u32>,
    ) -> Result<Self, String> {
        if timeout.is_some_and(|value| value.is_zero()) {
            return Err("task timeout must be greater than zero".into());
        }
        if memory_bytes == Some(0) {
            return Err("task memory limit must be greater than zero".into());
        }
        if max_processes == Some(0) {
            return Err("task process limit must be greater than zero".into());
        }
        self.timeout_ms = timeout
            .map(|value| {
                u64::try_from(value.as_millis())
                    .map_err(|_| "task timeout is too large".to_string())
            })
            .transpose()?;
        self.memory_bytes = memory_bytes;
        self.max_processes = max_processes;
        Ok(self)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn timeout(&self) -> Option<Duration> {
        self.timeout_ms.map(Duration::from_millis)
    }

    pub fn memory_bytes(&self) -> Option<u64> {
        self.memory_bytes
    }

    pub fn max_processes(&self) -> Option<u32> {
        self.max_processes
    }

    pub fn execution_spec(&self) -> Result<ExecutionSpec, String> {
        let program = crate::command_resolver::resolve_in(&self.command, &self.cwd)
            .ok_or_else(|| format!("command not found: {}", self.command))?;
        let mut spec = ExecutionSpec::new(
            ExecutionTarget::External {
                program,
                args: self.args.clone(),
            },
            self.cwd.clone(),
            ExecutionMode::Foreground,
        )
        .with_limits(ExecutionLimits {
            memory_bytes: self.memory_bytes,
            process_count: self.max_processes,
        });
        if let Some(timeout) = self.timeout() {
            spec = spec.with_timeout(timeout);
        }
        Ok(spec)
    }

    pub fn display_command(&self) -> String {
        std::iter::once(self.command.clone())
            .chain(self.args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn to_json(&self) -> String {
        serde_json::json!({
            "version": 2,
            "name": self.name,
            "command": self.command,
            "args": self.args,
            "cwd": self.cwd.to_string_lossy(),
            "timeout_ms": self.timeout_ms,
            "memory_bytes": self.memory_bytes,
            "max_processes": self.max_processes,
        })
        .to_string()
    }

    pub fn from_json(text: &str) -> Result<Self, String> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|error| format!("invalid task data: {error}"))?;
        let version = value.get("version").and_then(|value| value.as_u64());
        if !matches!(version, Some(1 | 2)) {
            return Err("unsupported task data version".into());
        }
        let name = value
            .get("name")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "task data is missing name".to_string())?;
        let command = value
            .get("command")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "task data is missing command".to_string())?;
        let args = value
            .get("args")
            .and_then(|value| value.as_array())
            .ok_or_else(|| "task data is missing args".to_string())?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "task argument is not a string".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let cwd = value
            .get("cwd")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "task data is missing cwd".to_string())?;
        let task = Self::new(name, command, args, PathBuf::from(cwd))?;
        if version == Some(1) {
            return Ok(task);
        }
        let optional_u64 = |name: &str| -> Result<Option<u64>, String> {
            match value.get(name) {
                None | Some(serde_json::Value::Null) => Ok(None),
                Some(value) => value
                    .as_u64()
                    .map(Some)
                    .ok_or_else(|| format!("task data field '{name}' is not an unsigned integer")),
            }
        };
        let timeout_ms = optional_u64("timeout_ms")?;
        let memory_bytes = optional_u64("memory_bytes")?;
        let max_processes = optional_u64("max_processes")?
            .map(|value| {
                u32::try_from(value).map_err(|_| "task process limit is too large".to_string())
            })
            .transpose()?;
        task.with_policy(
            timeout_ms.map(Duration::from_millis),
            memory_bytes,
            max_processes,
        )
    }
}

pub fn parse_timeout(value: &str) -> Result<Duration, String> {
    let (number, multiplier) = if let Some(value) = value.strip_suffix("ms") {
        (value, 1u64)
    } else if let Some(value) = value.strip_suffix('s') {
        (value, 1_000)
    } else if let Some(value) = value.strip_suffix('m') {
        (value, 60_000)
    } else if let Some(value) = value.strip_suffix('h') {
        (value, 3_600_000)
    } else {
        (value, 1_000)
    };
    let number = number
        .parse::<u64>()
        .map_err(|_| format!("invalid timeout '{value}'"))?;
    let millis = number
        .checked_mul(multiplier)
        .ok_or_else(|| "timeout is too large".to_string())?;
    if millis == 0 {
        return Err("timeout must be greater than zero".into());
    }
    Ok(Duration::from_millis(millis))
}

pub fn parse_memory(value: &str) -> Result<u64, String> {
    let lower = value.to_ascii_lowercase();
    let (number, multiplier) = [
        ("gib", 1024u64.pow(3)),
        ("mib", 1024u64.pow(2)),
        ("kib", 1024u64),
        ("gb", 1_000_000_000),
        ("mb", 1_000_000),
        ("kb", 1_000),
        ("b", 1),
    ]
    .into_iter()
    .find_map(|(suffix, multiplier)| {
        lower.strip_suffix(suffix).map(|number| (number, multiplier))
    })
    .unwrap_or((lower.as_str(), 1));
    let bytes = number
        .parse::<u64>()
        .map_err(|_| format!("invalid memory limit '{value}'"))?
        .checked_mul(multiplier)
        .ok_or_else(|| "memory limit is too large".to_string())?;
    if bytes == 0 {
        return Err("memory limit must be greater than zero".into());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_round_trips_without_runtime_state() {
        let task = TaskDefinition::new(
            "nightly.backup",
            "robocopy",
            vec!["source".into(), "backup".into()],
            PathBuf::from(r"C:\work"),
        )
        .unwrap()
        .with_policy(
            Some(Duration::from_secs(30)),
            Some(256 * 1024 * 1024),
            Some(4),
        )
        .unwrap();
        assert_eq!(TaskDefinition::from_json(&task.to_json()).unwrap(), task);
        assert_eq!(task.display_command(), "robocopy source backup");
        assert_eq!(task.timeout(), Some(Duration::from_secs(30)));
        assert_eq!(task.memory_bytes(), Some(256 * 1024 * 1024));
        assert_eq!(task.max_processes(), Some(4));
    }

    #[test]
    fn task_names_are_safe_registry_keys() {
        assert!(TaskDefinition::new("bad name", "cmd", vec![], PathBuf::from(r"C:\work")).is_err());
    }

    #[test]
    fn version_one_tasks_load_without_policies() {
        let encoded = r#"{"version":1,"name":"old","command":"cmd.exe","args":[],"cwd":"C:\\work"}"#;
        let task = TaskDefinition::from_json(encoded).unwrap();
        assert_eq!(task.timeout(), None);
        assert_eq!(task.memory_bytes(), None);
        assert_eq!(task.max_processes(), None);
    }

    #[test]
    fn policy_value_parsers_accept_units_and_reject_zero() {
        assert_eq!(parse_timeout("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_timeout("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_memory("64MiB").unwrap(), 64 * 1024 * 1024);
        assert_eq!(parse_memory("2GB").unwrap(), 2_000_000_000);
        assert!(parse_timeout("0s").is_err());
        assert!(parse_memory("0").is_err());
    }
}
