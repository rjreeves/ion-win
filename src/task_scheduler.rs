//! Windows Task Scheduler adapter for persistent schedule definitions.

use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{Datelike, LocalResult, NaiveTime, TimeZone};

use crate::schedule::{ScheduleDefinition, ScheduleTrigger};
use crate::task::TaskDefinition;

const TASK_PREFIX: &str = r"\ion-win-";

pub fn register(schedule: &ScheduleDefinition, task: &TaskDefinition) -> Result<(), String> {
    #[cfg(not(windows))]
    return Err("Windows Task Scheduler is only available on Windows".into());

    #[cfg(windows)]
    {
        let snapshot = snapshot_path(schedule.name())?;
        let previous_snapshot = std::fs::read(&snapshot).ok();
        if let Some(parent) = snapshot.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("could not create scheduler state directory: {error}"))?;
        }
        std::fs::write(&snapshot, task.to_json())
            .map_err(|error| format!("could not write task snapshot: {error}"))?;

        let executable = std::env::current_exe()
            .map_err(|error| format!("could not locate ion-win executable: {error}"))?;
        let xml = registration_xml(schedule, task, &executable, &snapshot)?;
        let xml_path = snapshot.with_extension("xml.tmp");
        write_utf16(&xml_path, &xml)
            .map_err(|error| format!("could not write scheduler registration: {error}"))?;
        let result = run_schtasks(&[
            "/Create",
            "/TN",
            &registered_name(schedule.name()),
            "/XML",
            &xml_path.to_string_lossy(),
            "/F",
        ]);
        let _ = std::fs::remove_file(&xml_path);
        if result.is_err() {
            if let Some(previous) = previous_snapshot {
                let _ = std::fs::write(&snapshot, previous);
            } else {
                let _ = std::fs::remove_file(&snapshot);
            }
        }
        result
    }
}

pub fn delete(name: &str) -> Result<(), String> {
    run_schtasks(&["/Delete", "/TN", &registered_name(name), "/F"])?;
    forget_snapshot(name);
    Ok(())
}

pub fn forget_snapshot(name: &str) {
    if let Ok(snapshot) = snapshot_path(name) {
        let _ = std::fs::remove_file(snapshot);
    }
}

pub fn set_enabled(name: &str, enabled: bool) -> Result<(), String> {
    run_schtasks(&[
        "/Change",
        "/TN",
        &registered_name(name),
        if enabled { "/ENABLE" } else { "/DISABLE" },
    ])
}

pub fn run(name: &str) -> Result<(), String> {
    run_schtasks(&["/Run", "/TN", &registered_name(name)])
}

fn run_schtasks(args: &[&str]) -> Result<(), String> {
    #[cfg(not(windows))]
    return Err("Windows Task Scheduler is only available on Windows".into());

    #[cfg(windows)]
    {
        let output = Command::new("schtasks.exe")
            .args(args)
            .output()
            .map_err(|error| format!("could not start schtasks.exe: {error}"))?;
        if output.status.success() {
            Ok(())
        } else {
            let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let fallback = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Err(if message.is_empty() {
                fallback
            } else {
                message
            })
        }
    }
}

fn registered_name(name: &str) -> String {
    format!("{TASK_PREFIX}{name}")
}

fn snapshot_path(name: &str) -> Result<PathBuf, String> {
    let root = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| "could not determine scheduler state directory".to_string())?;
    Ok(root
        .join("ion-win")
        .join("scheduled")
        .join(format!("{name}.task.json")))
}

fn registration_xml(
    schedule: &ScheduleDefinition,
    task: &TaskDefinition,
    executable: &Path,
    snapshot: &Path,
) -> Result<String, String> {
    let trigger = trigger_xml(schedule.trigger())?;
    let enabled = if schedule.enabled() { "true" } else { "false" };
    let arguments = format!("--run-scheduled-task \"{}\"", snapshot.to_string_lossy());
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>ion-win schedule {description}</Description></RegistrationInfo>
  <Triggers>{trigger}</Triggers>
  <Principals><Principal id="Author"><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><AllowHardTerminate>true</AllowHardTerminate><StartWhenAvailable>true</StartWhenAvailable><Enabled>{enabled}</Enabled><ExecutionTimeLimit>PT0S</ExecutionTimeLimit></Settings>
  <Actions Context="Author"><Exec><Command>{command}</Command><Arguments>{arguments}</Arguments><WorkingDirectory>{cwd}</WorkingDirectory></Exec></Actions>
</Task>"#,
        description = xml_escape(schedule.name()),
        trigger = trigger,
        enabled = enabled,
        command = xml_escape(&executable.to_string_lossy()),
        arguments = xml_escape(&arguments),
        cwd = xml_escape(&task.cwd().to_string_lossy()),
    ))
}

fn write_utf16(path: &Path, text: &str) -> std::io::Result<()> {
    let mut bytes = Vec::with_capacity(2 + text.len() * 2);
    bytes.extend_from_slice(&[0xFF, 0xFE]);
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(path, bytes)
}

fn trigger_xml(trigger: &ScheduleTrigger) -> Result<String, String> {
    match trigger {
        ScheduleTrigger::Once { at } => Ok(format!(
            "<TimeTrigger><StartBoundary>{}</StartBoundary><Enabled>true</Enabled></TimeTrigger>",
            xml_escape(at)
        )),
        ScheduleTrigger::Daily { at, timezone } => {
            let time = NaiveTime::parse_from_str(at, "%H:%M")
                .or_else(|_| NaiveTime::parse_from_str(at, "%H:%M:%S"))
                .map_err(|_| "invalid daily trigger time".to_string())?;
            let zone = timezone
                .parse::<chrono_tz::Tz>()
                .map_err(|_| format!("unknown schedule timezone: {timezone}"))?;
            let now = chrono::Utc::now().with_timezone(&zone);
            let mut date = now.date_naive();
            let mut local = match zone.from_local_datetime(&date.and_time(time)) {
                LocalResult::Single(value) => value,
                LocalResult::Ambiguous(first, _) => first,
                LocalResult::None => {
                    return Err("daily trigger falls in a timezone transition gap".into())
                }
            };
            if local <= now {
                date = date
                    .succ_opt()
                    .ok_or_else(|| "daily trigger date overflow".to_string())?;
                local = match zone.from_local_datetime(&date.and_time(time)) {
                    LocalResult::Single(value) => value,
                    LocalResult::Ambiguous(first, _) => first,
                    LocalResult::None => {
                        return Err("daily trigger falls in a timezone transition gap".into())
                    }
                };
            }
            let boundary = format!(
                "{:04}-{:02}-{:02}T{}{}",
                local.year(),
                local.month(),
                local.day(),
                local.format("%H:%M:%S"),
                local.format("%:z")
            );
            Ok(format!("<CalendarTrigger><StartBoundary>{boundary}</StartBoundary><Enabled>true</Enabled><ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay></CalendarTrigger>"))
        }
        ScheduleTrigger::AtLogon => {
            Ok("<LogonTrigger><Enabled>true</Enabled></LogonTrigger>".into())
        }
        ScheduleTrigger::AtStartup => {
            Ok("<BootTrigger><Enabled>true</Enabled></BootTrigger>".into())
        }
    }
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_escapes_action_and_uses_private_runner() {
        let schedule =
            ScheduleDefinition::new("daily", "backup", ScheduleTrigger::AtLogon, false).unwrap();
        let task = TaskDefinition::new("backup", "tool", vec![], PathBuf::from(r"C:\work & files"))
            .unwrap();
        let xml = registration_xml(
            &schedule,
            &task,
            Path::new(r"C:\Program Files\ion-win.exe"),
            Path::new(r"C:\state & data\daily.task.json"),
        )
        .unwrap();
        assert!(xml.contains("--run-scheduled-task &quot;C:\\state &amp; data"));
        assert!(xml.contains("<Enabled>false</Enabled>"));
        assert!(xml.contains("C:\\work &amp; files"));
    }

    #[test]
    fn scheduler_names_stay_in_ion_folder() {
        assert_eq!(registered_name("nightly"), r"\ion-win-nightly");
    }

    #[test]
    fn scheduler_xml_is_written_as_utf16le_with_bom() {
        let path =
            std::env::temp_dir().join(format!("ion-win-scheduler-xml-{}.tmp", std::process::id()));
        write_utf16(&path, "<Task>✓</Task>").unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..2], &[0xFF, 0xFE]);
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        assert_eq!(String::from_utf16(&units).unwrap(), "<Task>✓</Task>");
        std::fs::remove_file(path).unwrap();
    }
}
