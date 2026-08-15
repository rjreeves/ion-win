//! Persistent task definitions, separate from live execution state.

use std::path::{Path, PathBuf};

use crate::execution::{ExecutionMode, ExecutionSpec, ExecutionTarget};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskDefinition {
    name: String,
    command: String,
    args: Vec<String>,
    cwd: PathBuf,
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
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn execution_spec(&self) -> Result<ExecutionSpec, String> {
        let program = crate::command_resolver::resolve_in(&self.command, &self.cwd)
            .ok_or_else(|| format!("command not found: {}", self.command))?;
        Ok(ExecutionSpec::new(
            ExecutionTarget::External {
                program,
                args: self.args.clone(),
            },
            self.cwd.clone(),
            ExecutionMode::Foreground,
        ))
    }

    pub fn display_command(&self) -> String {
        std::iter::once(self.command.clone())
            .chain(self.args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn to_json(&self) -> String {
        serde_json::json!({
            "version": 1,
            "name": self.name,
            "command": self.command,
            "args": self.args,
            "cwd": self.cwd.to_string_lossy(),
        })
        .to_string()
    }

    pub fn from_json(text: &str) -> Result<Self, String> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|error| format!("invalid task data: {error}"))?;
        let version = value.get("version").and_then(|value| value.as_u64());
        if version != Some(1) {
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
        Self::new(name, command, args, PathBuf::from(cwd))
    }
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
        .unwrap();
        assert_eq!(TaskDefinition::from_json(&task.to_json()).unwrap(), task);
        assert_eq!(task.display_command(), "robocopy source backup");
    }

    #[test]
    fn task_names_are_safe_registry_keys() {
        assert!(TaskDefinition::new("bad name", "cmd", vec![], PathBuf::from(r"C:\work")).is_err());
    }
}
