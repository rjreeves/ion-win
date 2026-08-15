//! Embedded state persistence via `redb`.
//!
//! The main shell thread never touches the database directly. It sends
//! `StateCommand`s down an mpsc channel to a dedicated background worker,
//! which owns the `redb::Database` and performs the actual reads/writes.
//! This keeps `redb`'s single-writer lock from ever stalling the prompt.

use redb::{Database, ReadableTable, TableDefinition};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::sync::{mpsc, oneshot};

/// Persistent user variables: maps variable name -> evaluated string value.
pub const PERSISTENT_VARS: TableDefinition<&str, &str> = TableDefinition::new("shell_variables");

/// Fast-travel directory bookmarks: maps short alias -> absolute path.
pub const DIR_BOOKMARKS: TableDefinition<&str, &str> = TableDefinition::new("dir_bookmarks");

/// Named reusable task definitions, stored independently from executions.
pub const TASKS: TableDefinition<&str, &str> = TableDefinition::new("tasks");
pub const SCHEDULES: TableDefinition<&str, &str> = TableDefinition::new("schedules");

#[derive(Debug)]
pub enum StateCommand {
    SetVar {
        key: String,
        value: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    GetVar {
        key: String,
        reply: oneshot::Sender<Option<String>>,
    },
    ListVars {
        reply: oneshot::Sender<Vec<(String, String)>>,
    },
    DeleteVar {
        key: String,
        reply: oneshot::Sender<Result<(), String>>,
    },

    AddBookmark {
        name: String,
        path: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    GetBookmark {
        name: String,
        reply: oneshot::Sender<Option<String>>,
    },
    ListBookmarks {
        reply: oneshot::Sender<Vec<(String, String)>>,
    },
    PutTask {
        task: crate::task::TaskDefinition,
        replace: bool,
        reply: oneshot::Sender<Result<(), String>>,
    },
    GetTask {
        name: String,
        reply: oneshot::Sender<Result<Option<crate::task::TaskDefinition>, String>>,
    },
    ListTasks {
        reply: oneshot::Sender<Result<Vec<crate::task::TaskDefinition>, String>>,
    },
    DeleteTask {
        name: String,
        reply: oneshot::Sender<Result<bool, String>>,
    },
    PutSchedule {
        schedule: crate::schedule::ScheduleDefinition,
        replace: bool,
        reply: oneshot::Sender<Result<(), String>>,
    },
    GetSchedule {
        name: String,
        reply: oneshot::Sender<Result<Option<crate::schedule::ScheduleDefinition>, String>>,
    },
    ListSchedules {
        reply: oneshot::Sender<Result<Vec<crate::schedule::ScheduleDefinition>, String>>,
    },
    DeleteSchedule {
        name: String,
        reply: oneshot::Sender<Result<bool, String>>,
    },
}

/// Handle held by the main shell thread. Cloneable, cheap, non-blocking.
#[derive(Clone)]
pub struct StateHandle {
    tx: mpsc::Sender<StateCommand>,
}

impl StateHandle {
    pub async fn set_var(
        &self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        self.send(StateCommand::SetVar {
            key: key.into(),
            value: value.into(),
            reply,
        })
        .await;
        rx.await.map_err(|_| "state worker dropped".to_string())?
    }

    pub async fn get_var(&self, key: impl Into<String>) -> Option<String> {
        let (reply, rx) = oneshot::channel();
        self.send(StateCommand::GetVar {
            key: key.into(),
            reply,
        })
        .await;
        rx.await.ok().flatten()
    }

    pub async fn list_vars(&self) -> Vec<(String, String)> {
        let (reply, rx) = oneshot::channel();
        self.send(StateCommand::ListVars { reply }).await;
        rx.await.unwrap_or_default()
    }

    pub async fn delete_var(&self, key: impl Into<String>) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        self.send(StateCommand::DeleteVar {
            key: key.into(),
            reply,
        })
        .await;
        rx.await.map_err(|_| "state worker dropped".to_string())?
    }

    pub async fn add_bookmark(
        &self,
        name: impl Into<String>,
        path: impl Into<String>,
    ) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        self.send(StateCommand::AddBookmark {
            name: name.into(),
            path: path.into(),
            reply,
        })
        .await;
        rx.await.map_err(|_| "state worker dropped".to_string())?
    }

    pub async fn get_bookmark(&self, name: impl Into<String>) -> Option<String> {
        let (reply, rx) = oneshot::channel();
        self.send(StateCommand::GetBookmark {
            name: name.into(),
            reply,
        })
        .await;
        rx.await.ok().flatten()
    }

    pub async fn list_bookmarks(&self) -> Vec<(String, String)> {
        let (reply, rx) = oneshot::channel();
        self.send(StateCommand::ListBookmarks { reply }).await;
        rx.await.unwrap_or_default()
    }

    pub async fn put_task(
        &self,
        task: crate::task::TaskDefinition,
        replace: bool,
    ) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        self.send(StateCommand::PutTask {
            task,
            replace,
            reply,
        })
        .await;
        rx.await.map_err(|_| "state worker dropped".to_string())?
    }

    pub async fn get_task(
        &self,
        name: impl Into<String>,
    ) -> Result<Option<crate::task::TaskDefinition>, String> {
        let (reply, rx) = oneshot::channel();
        self.send(StateCommand::GetTask {
            name: name.into(),
            reply,
        })
        .await;
        rx.await.map_err(|_| "state worker dropped".to_string())?
    }

    pub async fn list_tasks(&self) -> Result<Vec<crate::task::TaskDefinition>, String> {
        let (reply, rx) = oneshot::channel();
        self.send(StateCommand::ListTasks { reply }).await;
        rx.await.map_err(|_| "state worker dropped".to_string())?
    }

    pub async fn delete_task(&self, name: impl Into<String>) -> Result<bool, String> {
        let (reply, rx) = oneshot::channel();
        self.send(StateCommand::DeleteTask {
            name: name.into(),
            reply,
        })
        .await;
        rx.await.map_err(|_| "state worker dropped".to_string())?
    }

    pub async fn put_schedule(
        &self,
        schedule: crate::schedule::ScheduleDefinition,
        replace: bool,
    ) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        self.send(StateCommand::PutSchedule {
            schedule,
            replace,
            reply,
        })
        .await;
        rx.await.map_err(|_| "state worker dropped".to_string())?
    }

    pub async fn get_schedule(
        &self,
        name: impl Into<String>,
    ) -> Result<Option<crate::schedule::ScheduleDefinition>, String> {
        let (reply, rx) = oneshot::channel();
        self.send(StateCommand::GetSchedule {
            name: name.into(),
            reply,
        })
        .await;
        rx.await.map_err(|_| "state worker dropped".to_string())?
    }

    pub async fn list_schedules(&self) -> Result<Vec<crate::schedule::ScheduleDefinition>, String> {
        let (reply, rx) = oneshot::channel();
        self.send(StateCommand::ListSchedules { reply }).await;
        rx.await.map_err(|_| "state worker dropped".to_string())?
    }

    pub async fn delete_schedule(&self, name: impl Into<String>) -> Result<bool, String> {
        let (reply, rx) = oneshot::channel();
        self.send(StateCommand::DeleteSchedule {
            name: name.into(),
            reply,
        })
        .await;
        rx.await.map_err(|_| "state worker dropped".to_string())?
    }

    async fn send(&self, cmd: StateCommand) {
        // Bounded channel backpressure is fine here: the worker drains fast
        // and this is the only place allowed to block briefly on contention.
        let _ = self.tx.send(cmd).await;
    }
}

/// Spawns the background database worker and returns a handle to talk to it.
pub fn spawn(db_path: PathBuf) -> Result<StateHandle, redb::Error> {
    let db = Database::create(&db_path)?;
    {
        // Ensure tables exist before any reads happen.
        let write_txn = db.begin_write()?;
        write_txn.open_table(PERSISTENT_VARS)?;
        write_txn.open_table(DIR_BOOKMARKS)?;
        write_txn.open_table(TASKS)?;
        write_txn.open_table(SCHEDULES)?;
        write_txn.commit()?;
    }

    let (tx, mut rx) = mpsc::channel::<StateCommand>(256);

    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            handle_command(&db, cmd);
        }
    });

    Ok(StateHandle { tx })
}

/// Spawns a non-persistent state worker. This keeps Ion usable when the
/// persistent redb file is already locked by another Ion process.
pub fn spawn_memory() -> StateHandle {
    let (tx, mut rx) = mpsc::channel::<StateCommand>(256);

    tokio::spawn(async move {
        let mut vars = HashMap::<String, String>::new();
        let mut bookmarks = HashMap::<String, String>::new();
        let mut tasks = HashMap::<String, crate::task::TaskDefinition>::new();
        let mut schedules = HashMap::<String, crate::schedule::ScheduleDefinition>::new();

        while let Some(cmd) = rx.recv().await {
            handle_memory_command(&mut vars, &mut bookmarks, &mut tasks, &mut schedules, cmd);
        }
    });

    StateHandle { tx }
}

fn handle_command(db: &Database, cmd: StateCommand) {
    match cmd {
        StateCommand::SetVar { key, value, reply } => {
            let result = (|| -> Result<(), redb::Error> {
                let write_txn = db.begin_write()?;
                {
                    let mut table = write_txn.open_table(PERSISTENT_VARS)?;
                    table.insert(key.as_str(), value.as_str())?;
                }
                write_txn.commit()?;
                Ok(())
            })();
            let _ = reply.send(result.map_err(|e| e.to_string()));
        }
        StateCommand::GetVar { key, reply } => {
            let value = (|| -> Result<Option<String>, redb::Error> {
                let read_txn = db.begin_read()?;
                let table = read_txn.open_table(PERSISTENT_VARS)?;
                Ok(table.get(key.as_str())?.map(|v| v.value().to_string()))
            })()
            .unwrap_or(None);
            let _ = reply.send(value);
        }
        StateCommand::ListVars { reply } => {
            let values = (|| -> Result<Vec<(String, String)>, redb::Error> {
                let read_txn = db.begin_read()?;
                let table = read_txn.open_table(PERSISTENT_VARS)?;
                let mut out = Vec::new();
                for entry in table.iter()? {
                    let (k, v) = entry?;
                    out.push((k.value().to_string(), v.value().to_string()));
                }
                Ok(out)
            })()
            .unwrap_or_default();
            let _ = reply.send(values);
        }
        StateCommand::DeleteVar { key, reply } => {
            let result = (|| -> Result<(), redb::Error> {
                let write_txn = db.begin_write()?;
                {
                    let mut table = write_txn.open_table(PERSISTENT_VARS)?;
                    table.remove(key.as_str())?;
                }
                write_txn.commit()?;
                Ok(())
            })();
            let _ = reply.send(result.map_err(|e| e.to_string()));
        }
        StateCommand::AddBookmark { name, path, reply } => {
            let result = (|| -> Result<(), redb::Error> {
                let write_txn = db.begin_write()?;
                {
                    let mut table = write_txn.open_table(DIR_BOOKMARKS)?;
                    table.insert(name.as_str(), path.as_str())?;
                }
                write_txn.commit()?;
                Ok(())
            })();
            let _ = reply.send(result.map_err(|e| e.to_string()));
        }
        StateCommand::GetBookmark { name, reply } => {
            let value = (|| -> Result<Option<String>, redb::Error> {
                let read_txn = db.begin_read()?;
                let table = read_txn.open_table(DIR_BOOKMARKS)?;
                Ok(table.get(name.as_str())?.map(|v| v.value().to_string()))
            })()
            .unwrap_or(None);
            let _ = reply.send(value);
        }
        StateCommand::ListBookmarks { reply } => {
            let values = (|| -> Result<Vec<(String, String)>, redb::Error> {
                let read_txn = db.begin_read()?;
                let table = read_txn.open_table(DIR_BOOKMARKS)?;
                let mut out = Vec::new();
                for entry in table.iter()? {
                    let (k, v) = entry?;
                    out.push((k.value().to_string(), v.value().to_string()));
                }
                Ok(out)
            })()
            .unwrap_or_default();
            let _ = reply.send(values);
        }
        StateCommand::PutTask {
            task,
            replace,
            reply,
        } => {
            let result = (|| -> Result<(), String> {
                let write_txn = db.begin_write().map_err(|error| error.to_string())?;
                if replace {
                    let schedules = write_txn
                        .open_table(SCHEDULES)
                        .map_err(|error| error.to_string())?;
                    for entry in schedules.iter().map_err(|error| error.to_string())? {
                        let (_, value) = entry.map_err(|error| error.to_string())?;
                        let schedule =
                            crate::schedule::ScheduleDefinition::from_json(value.value())?;
                        if schedule.task_name() == task.name() {
                            return Err(format!(
                                "task '{}' is referenced by schedule '{}'; recreate the schedule after replacing the task",
                                task.name(),
                                schedule.name()
                            ));
                        }
                    }
                }
                {
                    let mut table = write_txn
                        .open_table(TASKS)
                        .map_err(|error| error.to_string())?;
                    if !replace
                        && table
                            .get(task.name())
                            .map_err(|error| error.to_string())?
                            .is_some()
                    {
                        return Err(format!("task '{}' already exists", task.name()));
                    }
                    let encoded = task.to_json();
                    table
                        .insert(task.name(), encoded.as_str())
                        .map_err(|error| error.to_string())?;
                }
                write_txn.commit().map_err(|error| error.to_string())?;
                Ok(())
            })();
            let _ = reply.send(result);
        }
        StateCommand::GetTask { name, reply } => {
            let result = (|| -> Result<Option<crate::task::TaskDefinition>, String> {
                let read_txn = db.begin_read().map_err(|error| error.to_string())?;
                let table = read_txn
                    .open_table(TASKS)
                    .map_err(|error| error.to_string())?;
                table
                    .get(name.as_str())
                    .map_err(|error| error.to_string())?
                    .map(|value| crate::task::TaskDefinition::from_json(value.value()))
                    .transpose()
            })();
            let _ = reply.send(result);
        }
        StateCommand::ListTasks { reply } => {
            let result = (|| -> Result<Vec<crate::task::TaskDefinition>, String> {
                let read_txn = db.begin_read().map_err(|error| error.to_string())?;
                let table = read_txn
                    .open_table(TASKS)
                    .map_err(|error| error.to_string())?;
                let mut tasks = Vec::new();
                for entry in table.iter().map_err(|error| error.to_string())? {
                    let (_, value) = entry.map_err(|error| error.to_string())?;
                    tasks.push(crate::task::TaskDefinition::from_json(value.value())?);
                }
                tasks.sort_by(|left, right| left.name().cmp(right.name()));
                Ok(tasks)
            })();
            let _ = reply.send(result);
        }
        StateCommand::DeleteTask { name, reply } => {
            let result = (|| -> Result<bool, String> {
                let write_txn = db.begin_write().map_err(|error| error.to_string())?;
                {
                    let schedules = write_txn
                        .open_table(SCHEDULES)
                        .map_err(|error| error.to_string())?;
                    for entry in schedules.iter().map_err(|error| error.to_string())? {
                        let (_, value) = entry.map_err(|error| error.to_string())?;
                        let schedule =
                            crate::schedule::ScheduleDefinition::from_json(value.value())?;
                        if schedule.task_name() == name {
                            return Err(format!(
                                "task '{name}' is referenced by schedule '{}'",
                                schedule.name()
                            ));
                        }
                    }
                }
                let removed = {
                    let mut table = write_txn
                        .open_table(TASKS)
                        .map_err(|error| error.to_string())?;
                    let removed = table
                        .remove(name.as_str())
                        .map_err(|error| error.to_string())?
                        .is_some();
                    removed
                };
                write_txn.commit().map_err(|error| error.to_string())?;
                Ok(removed)
            })();
            let _ = reply.send(result);
        }
        StateCommand::PutSchedule {
            schedule,
            replace,
            reply,
        } => {
            let result = (|| -> Result<(), String> {
                let write_txn = db.begin_write().map_err(|error| error.to_string())?;
                {
                    let tasks = write_txn
                        .open_table(TASKS)
                        .map_err(|error| error.to_string())?;
                    if tasks
                        .get(schedule.task_name())
                        .map_err(|error| error.to_string())?
                        .is_none()
                    {
                        return Err(format!(
                            "schedule references missing task '{}'",
                            schedule.task_name()
                        ));
                    }
                }
                {
                    let mut table = write_txn
                        .open_table(SCHEDULES)
                        .map_err(|error| error.to_string())?;
                    if !replace
                        && table
                            .get(schedule.name())
                            .map_err(|error| error.to_string())?
                            .is_some()
                    {
                        return Err(format!("schedule '{}' already exists", schedule.name()));
                    }
                    let encoded = schedule.to_json();
                    table
                        .insert(schedule.name(), encoded.as_str())
                        .map_err(|error| error.to_string())?;
                }
                write_txn.commit().map_err(|error| error.to_string())?;
                Ok(())
            })();
            let _ = reply.send(result);
        }
        StateCommand::GetSchedule { name, reply } => {
            let result = (|| -> Result<Option<crate::schedule::ScheduleDefinition>, String> {
                let read_txn = db.begin_read().map_err(|error| error.to_string())?;
                let table = read_txn
                    .open_table(SCHEDULES)
                    .map_err(|error| error.to_string())?;
                table
                    .get(name.as_str())
                    .map_err(|error| error.to_string())?
                    .map(|value| crate::schedule::ScheduleDefinition::from_json(value.value()))
                    .transpose()
            })();
            let _ = reply.send(result);
        }
        StateCommand::ListSchedules { reply } => {
            let result = (|| -> Result<Vec<crate::schedule::ScheduleDefinition>, String> {
                let read_txn = db.begin_read().map_err(|error| error.to_string())?;
                let table = read_txn
                    .open_table(SCHEDULES)
                    .map_err(|error| error.to_string())?;
                let mut schedules = Vec::new();
                for entry in table.iter().map_err(|error| error.to_string())? {
                    let (_, value) = entry.map_err(|error| error.to_string())?;
                    schedules.push(crate::schedule::ScheduleDefinition::from_json(
                        value.value(),
                    )?);
                }
                schedules.sort_by(|left, right| left.name().cmp(right.name()));
                Ok(schedules)
            })();
            let _ = reply.send(result);
        }
        StateCommand::DeleteSchedule { name, reply } => {
            let result = (|| -> Result<bool, String> {
                let write_txn = db.begin_write().map_err(|error| error.to_string())?;
                let removed = {
                    let mut table = write_txn
                        .open_table(SCHEDULES)
                        .map_err(|error| error.to_string())?;
                    let removed = table
                        .remove(name.as_str())
                        .map_err(|error| error.to_string())?
                        .is_some();
                    removed
                };
                write_txn.commit().map_err(|error| error.to_string())?;
                Ok(removed)
            })();
            let _ = reply.send(result);
        }
    }
}

fn handle_memory_command(
    vars: &mut HashMap<String, String>,
    bookmarks: &mut HashMap<String, String>,
    tasks: &mut HashMap<String, crate::task::TaskDefinition>,
    schedules: &mut HashMap<String, crate::schedule::ScheduleDefinition>,
    cmd: StateCommand,
) {
    match cmd {
        StateCommand::SetVar { key, value, reply } => {
            vars.insert(key, value);
            let _ = reply.send(Ok(()));
        }
        StateCommand::GetVar { key, reply } => {
            let _ = reply.send(vars.get(&key).cloned());
        }
        StateCommand::ListVars { reply } => {
            let mut values: Vec<_> = vars.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            values.sort_by(|a, b| a.0.cmp(&b.0));
            let _ = reply.send(values);
        }
        StateCommand::DeleteVar { key, reply } => {
            vars.remove(&key);
            let _ = reply.send(Ok(()));
        }
        StateCommand::AddBookmark { name, path, reply } => {
            bookmarks.insert(name, path);
            let _ = reply.send(Ok(()));
        }
        StateCommand::GetBookmark { name, reply } => {
            let _ = reply.send(bookmarks.get(&name).cloned());
        }
        StateCommand::ListBookmarks { reply } => {
            let mut values: Vec<_> = bookmarks
                .iter()
                .map(|(name, path)| (name.clone(), path.clone()))
                .collect();
            values.sort_by(|a, b| a.0.cmp(&b.0));
            let _ = reply.send(values);
        }
        StateCommand::PutTask {
            task,
            replace,
            reply,
        } => {
            if replace {
                if let Some(schedule) = schedules
                    .values()
                    .find(|schedule| schedule.task_name() == task.name())
                {
                    let _ = reply.send(Err(format!(
                        "task '{}' is referenced by schedule '{}'; recreate the schedule after replacing the task",
                        task.name(),
                        schedule.name()
                    )));
                    return;
                }
            }
            if !replace && tasks.contains_key(task.name()) {
                let _ = reply.send(Err(format!("task '{}' already exists", task.name())));
            } else {
                tasks.insert(task.name().to_string(), task);
                let _ = reply.send(Ok(()));
            }
        }
        StateCommand::GetTask { name, reply } => {
            let _ = reply.send(Ok(tasks.get(&name).cloned()));
        }
        StateCommand::ListTasks { reply } => {
            let mut values: Vec<_> = tasks.values().cloned().collect();
            values.sort_by(|left, right| left.name().cmp(right.name()));
            let _ = reply.send(Ok(values));
        }
        StateCommand::DeleteTask { name, reply } => {
            if let Some(schedule) = schedules
                .values()
                .find(|schedule| schedule.task_name() == name)
            {
                let _ = reply.send(Err(format!(
                    "task '{name}' is referenced by schedule '{}'",
                    schedule.name()
                )));
            } else {
                let _ = reply.send(Ok(tasks.remove(&name).is_some()));
            }
        }
        StateCommand::PutSchedule {
            schedule,
            replace,
            reply,
        } => {
            if !tasks.contains_key(schedule.task_name()) {
                let _ = reply.send(Err(format!(
                    "schedule references missing task '{}'",
                    schedule.task_name()
                )));
            } else if !replace && schedules.contains_key(schedule.name()) {
                let _ = reply.send(Err(format!(
                    "schedule '{}' already exists",
                    schedule.name()
                )));
            } else {
                schedules.insert(schedule.name().to_string(), schedule);
                let _ = reply.send(Ok(()));
            }
        }
        StateCommand::GetSchedule { name, reply } => {
            let _ = reply.send(Ok(schedules.get(&name).cloned()));
        }
        StateCommand::ListSchedules { reply } => {
            let mut values: Vec<_> = schedules.values().cloned().collect();
            values.sort_by(|left, right| left.name().cmp(right.name()));
            let _ = reply.send(Ok(values));
        }
        StateCommand::DeleteSchedule { name, reply } => {
            let _ = reply.send(Ok(schedules.remove(&name).is_some()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(name: &str) -> crate::task::TaskDefinition {
        crate::task::TaskDefinition::new(
            name,
            "cmd",
            vec!["/c".into(), "echo task".into()],
            PathBuf::from(r"C:\work"),
        )
        .unwrap()
    }

    fn schedule(name: &str) -> crate::schedule::ScheduleDefinition {
        crate::schedule::ScheduleDefinition::new(
            name,
            "alpha",
            crate::schedule::ScheduleTrigger::AtLogon,
            true,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn memory_task_registry_create_list_replace_and_delete() {
        let state = spawn_memory();
        state.put_task(task("beta"), false).await.unwrap();
        state.put_task(task("alpha"), false).await.unwrap();
        assert!(state.put_task(task("alpha"), false).await.is_err());
        state.put_task(task("alpha"), true).await.unwrap();

        let listed = state.list_tasks().await.unwrap();
        assert_eq!(
            listed.iter().map(|task| task.name()).collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );
        assert_eq!(
            state.get_task("alpha").await.unwrap().unwrap().name(),
            "alpha"
        );
        state.put_schedule(schedule("daily"), false).await.unwrap();
        assert!(state.put_schedule(schedule("daily"), false).await.is_err());
        assert_eq!(
            state
                .get_schedule("daily")
                .await
                .unwrap()
                .unwrap()
                .task_name(),
            "alpha"
        );
        assert_eq!(state.list_schedules().await.unwrap().len(), 1);
        assert!(state.put_task(task("alpha"), true).await.is_err());
        assert!(state.delete_task("alpha").await.is_err());
        assert!(state.delete_schedule("daily").await.unwrap());
        assert!(state.delete_task("alpha").await.unwrap());
        assert!(!state.delete_task("alpha").await.unwrap());
    }
}
