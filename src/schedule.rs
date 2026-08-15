//! Platform-neutral persistent schedule definitions.
//!
//! The Windows Task Scheduler adapter is intentionally a later layer; these
//! immutable values describe user intent without storing OS registration or
//! live execution state.

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScheduleTrigger {
    Once { at: String },
    Daily { at: String, timezone: String },
    AtLogon,
    AtStartup,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScheduleDefinition {
    name: String,
    task_name: String,
    trigger: ScheduleTrigger,
    enabled: bool,
}

impl ScheduleDefinition {
    pub fn new(
        name: impl Into<String>,
        task_name: impl Into<String>,
        trigger: ScheduleTrigger,
        enabled: bool,
    ) -> Result<Self, String> {
        let name = name.into();
        validate_name("schedule", &name)?;
        let task_name = task_name.into();
        validate_name("task", &task_name)?;
        validate_trigger(&trigger)?;
        Ok(Self {
            name,
            task_name,
            trigger,
            enabled,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn task_name(&self) -> &str {
        &self.task_name
    }

    pub fn trigger(&self) -> &ScheduleTrigger {
        &self.trigger
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn with_enabled(&self, enabled: bool) -> Self {
        Self {
            name: self.name.clone(),
            task_name: self.task_name.clone(),
            trigger: self.trigger.clone(),
            enabled,
        }
    }

    pub fn display_trigger(&self) -> String {
        match &self.trigger {
            ScheduleTrigger::Once { at } => format!("once {at}"),
            ScheduleTrigger::Daily { at, timezone } => format!("daily {at} {timezone}"),
            ScheduleTrigger::AtLogon => "at-logon".into(),
            ScheduleTrigger::AtStartup => "at-startup".into(),
        }
    }

    pub fn to_json(&self) -> String {
        let trigger = match &self.trigger {
            ScheduleTrigger::Once { at } => serde_json::json!({ "kind": "once", "at": at }),
            ScheduleTrigger::Daily { at, timezone } => {
                serde_json::json!({ "kind": "daily", "at": at, "timezone": timezone })
            }
            ScheduleTrigger::AtLogon => serde_json::json!({ "kind": "at_logon" }),
            ScheduleTrigger::AtStartup => serde_json::json!({ "kind": "at_startup" }),
        };
        serde_json::json!({
            "version": 1,
            "name": self.name,
            "task_name": self.task_name,
            "trigger": trigger,
            "enabled": self.enabled,
        })
        .to_string()
    }

    pub fn from_json(text: &str) -> Result<Self, String> {
        let value: serde_json::Value = serde_json::from_str(text)
            .map_err(|error| format!("invalid schedule data: {error}"))?;
        if value.get("version").and_then(|value| value.as_u64()) != Some(1) {
            return Err("unsupported schedule data version".into());
        }
        let name = required_string(&value, "name")?;
        let task_name = required_string(&value, "task_name")?;
        let trigger_value = value
            .get("trigger")
            .ok_or_else(|| "schedule data is missing trigger".to_string())?;
        let trigger = match required_string(trigger_value, "kind")?.as_str() {
            "once" => ScheduleTrigger::Once {
                at: required_string(trigger_value, "at")?,
            },
            "daily" => ScheduleTrigger::Daily {
                at: required_string(trigger_value, "at")?,
                timezone: required_string(trigger_value, "timezone")?,
            },
            "at_logon" => ScheduleTrigger::AtLogon,
            "at_startup" => ScheduleTrigger::AtStartup,
            other => return Err(format!("unknown schedule trigger kind: {other}")),
        };
        let enabled = value
            .get("enabled")
            .and_then(|value| value.as_bool())
            .ok_or_else(|| "schedule data is missing enabled".to_string())?;
        Self::new(name, task_name, trigger, enabled)
    }
}

fn required_string(value: &serde_json::Value, field: &str) -> Result<String, String> {
    value
        .get(field)
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("schedule data is missing {field}"))
}

fn validate_name(kind: &str, name: &str) -> Result<(), String> {
    if name.is_empty()
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        Err(format!(
            "{kind} name must contain only ASCII letters, digits, '-', '_', or '.'"
        ))
    } else {
        Ok(())
    }
}

fn validate_trigger(trigger: &ScheduleTrigger) -> Result<(), String> {
    match trigger {
        ScheduleTrigger::Once { at } => chrono::DateTime::parse_from_rfc3339(at)
            .map(|_| ())
            .map_err(|_| "once trigger must be an RFC 3339 timestamp with an offset".into()),
        ScheduleTrigger::Daily { at, timezone } => {
            chrono::NaiveTime::parse_from_str(at, "%H:%M")
                .or_else(|_| chrono::NaiveTime::parse_from_str(at, "%H:%M:%S"))
                .map_err(|_| "daily trigger time must be HH:MM or HH:MM:SS".to_string())?;
            timezone
                .parse::<chrono_tz::Tz>()
                .map(|_| ())
                .map_err(|_| format!("unknown schedule timezone: {timezone}"))
        }
        ScheduleTrigger::AtLogon | ScheduleTrigger::AtStartup => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_platform_neutral_schedule_intent() {
        let schedule = ScheduleDefinition::new(
            "nightly",
            "backup",
            ScheduleTrigger::Daily {
                at: "23:30".into(),
                timezone: "Australia/Sydney".into(),
            },
            true,
        )
        .unwrap();
        assert_eq!(schedule.name(), "nightly");
        assert_eq!(schedule.task_name(), "backup");
        assert!(schedule.enabled());
        assert!(matches!(schedule.trigger(), ScheduleTrigger::Daily { .. }));
        assert_eq!(
            ScheduleDefinition::from_json(&schedule.to_json()).unwrap(),
            schedule
        );
    }

    #[test]
    fn rejects_ambiguous_or_invalid_calendar_values() {
        assert!(ScheduleDefinition::new(
            "nightly",
            "backup",
            ScheduleTrigger::Once {
                at: "tomorrow".into()
            },
            true,
        )
        .is_err());
        assert!(ScheduleDefinition::new(
            "nightly",
            "backup",
            ScheduleTrigger::Daily {
                at: "25:00".into(),
                timezone: "Local".into(),
            },
            true,
        )
        .is_err());
    }
}
