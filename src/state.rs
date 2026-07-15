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

        while let Some(cmd) = rx.recv().await {
            handle_memory_command(&mut vars, &mut bookmarks, cmd);
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
    }
}

fn handle_memory_command(
    vars: &mut HashMap<String, String>,
    bookmarks: &mut HashMap<String, String>,
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
    }
}
