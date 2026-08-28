use crate::compress::ArchivePlan;
use crate::fileset::{FileIdentity, FileKind, FileRecord};
use crate::table::Table;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static OPERATION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannedOperation { Archive(ArchivePlan), Copy(FileTransferPlan), Move(FileTransferPlan), Delete(DeletePlan) }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletePlan { pub items: Vec<FileRecord>, pub options: crate::delete::DeleteOptions }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTransferPlan {
    pub items: Vec<FileTransferPlanItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTransferPlanItem {
    pub source: FileRecord,
    pub destination: PathBuf,
    pub destination_before: Option<FileRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationPlan { pub id: String, pub operation: PlannedOperation, pub force: bool }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalOutput { pub role: String, pub record: FileRecord, pub original_path: Option<PathBuf> }

/// Durable evidence of what an operation actually created. Undo trusts these
/// post-write identities, never just the paths from the original plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationJournal {
    pub id: String,
    pub plan_id: String,
    pub operation: String,
    pub started_at: u64,
    pub finished_at: u64,
    pub outputs: Vec<JournalOutput>,
    /// Exact temporary paths created by a transactional operation. These
    /// intents are persisted before creation so crash recovery never guesses.
    pub staging_paths: Vec<PathBuf>,
    pub undo_safe: bool,
    pub status: String,
    pub error: Option<String>,
    pub undone_at: Option<u64>,
}

fn now_seconds() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |value| value.as_secs()) }

fn unique_id(prefix: &str) -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |value| value.as_nanos());
    let sequence = OPERATION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{nanos:x}-{sequence:x}")
}

impl OperationPlan {
    pub fn archive(plan: ArchivePlan, force: bool) -> Self { Self { id: unique_id("plan"), operation: PlannedOperation::Archive(plan), force } }

    pub fn copy(fileset: &crate::fileset::FileSet, destination: &str, force: bool) -> Result<Self, String> {
        Ok(Self { id: unique_id("plan"), operation: PlannedOperation::Copy(plan_transfer(fileset, destination, crate::copy::resource_target, "copy")?), force })
    }

    pub fn move_files(fileset: &crate::fileset::FileSet, destination: &str, force: bool) -> Result<Self, String> {
        Ok(Self { id: unique_id("plan"), operation: PlannedOperation::Move(plan_transfer(fileset, destination, crate::fs_ops::resource_target, "move")?), force })
    }

    pub fn delete(fileset: &crate::fileset::FileSet, options: crate::delete::DeleteOptions) -> Result<Self, String> {
        if options.permanent { return Err("delete: --plan does not support permanent deletion".to_string()); }
        for item in &fileset.files { crate::delete::validate_planned(&item.path, options)?; }
        Ok(Self { id: unique_id("plan"), operation: PlannedOperation::Delete(DeletePlan { items: fileset.files.clone(), options }), force: false })
    }

    pub fn to_table(&self) -> Table {
        let mut table = match &self.operation {
            PlannedOperation::Archive(plan) => plan.to_table(),
            PlannedOperation::Copy(plan) => transfer_table(plan, "copy"),
            PlannedOperation::Move(plan) => transfer_table(plan, "move"),
            PlannedOperation::Delete(plan) => Table { rows: plan.items.iter().map(|item| vec![("operation".to_string(), "delete".to_string()), ("path".to_string(), item.path.to_string_lossy().into_owned()), ("kind".to_string(), kind_name(&item.kind).to_string()), ("recurse".to_string(), plan.options.recurse.to_string()), ("mode".to_string(), "recycle".to_string()), ("undo_safe".to_string(), "false".to_string())]).collect() },
        };
        for row in &mut table.rows { row.insert(0, ("plan_id".to_string(), self.id.clone())); }
        table
    }

    pub async fn apply(&self, state: &crate::state::StateHandle) -> Result<OperationJournal, String> {
        let started_at = now_seconds();
        let operation = match &self.operation { PlannedOperation::Archive(_) => "compress", PlannedOperation::Copy(_) => "copy", PlannedOperation::Move(_) => "move", PlannedOperation::Delete(_) => "delete" };
        let initially_undo_safe = match &self.operation {
            PlannedOperation::Archive(plan) => plan.items.iter().all(|item| !item.archive.exists() && item.backup.as_ref().is_none_or(|path| !path.exists())),
            PlannedOperation::Copy(plan) | PlannedOperation::Move(plan) => plan.items.iter().all(|item| item.destination_before.is_none()),
            PlannedOperation::Delete(_) => false,
        };
        let mut journal = OperationJournal { id: unique_id("operation"), plan_id: self.id.clone(), operation: operation.to_string(), started_at, finished_at: 0, outputs: Vec::new(), staging_paths: Vec::new(), undo_safe: initially_undo_safe, status: "in_progress".to_string(), error: None, undone_at: None };
        state.put_journal(journal.clone()).await?;
        let result: Result<(), String> = match &self.operation {
            PlannedOperation::Archive(plan) => {
                apply_archive_transaction(plan, self.force, &mut journal, state).await
            },
            PlannedOperation::Copy(plan) => apply_transfer(plan, self.force, false, &mut journal, state).await,
            PlannedOperation::Move(plan) => apply_transfer(plan, self.force, true, &mut journal, state).await,
            PlannedOperation::Delete(plan) => apply_delete(plan, &mut journal, state).await,
        };
        match result {
            Ok(()) => {
                journal.status = "applied".to_string();
                journal.finished_at = now_seconds();
                state.put_journal(journal.clone()).await?;
                Ok(journal)
            }
            Err(error) => {
                if journal.status == "in_progress" { journal.status = "failed".to_string(); }
                journal.error = Some(error.clone());
                journal.finished_at = now_seconds();
                let _ = state.put_journal(journal.clone()).await;
                Err(format!("{error} (journal {})", journal.id))
            }
        }
    }
}

async fn apply_archive_transaction(
    plan: &ArchivePlan,
    force: bool,
    journal: &mut OperationJournal,
    state: &crate::state::StateHandle,
) -> Result<(), String> {
    crate::compress::validate_archive_plan(plan)?;
    for item in &plan.items {
        validate_archive_destination(&item.archive, force)?;
        if let Some(backup) = &item.backup { validate_archive_destination(backup, force)?; }
    }

    for item in &plan.items {
        let archive_staging = match staging_path(&item.archive, &journal.id, "archive") {
            Ok(path) => path,
            Err(error) => {
                rollback_and_checkpoint(journal, state).await;
                return Err(error);
            }
        };
        if let Err(error) = checkpoint_staging(&archive_staging, journal, state).await {
            rollback_and_checkpoint(journal, state).await;
            return Err(error);
        }
        let build_result = crate::compress::build_planned_archive(item, &archive_staging).await;
        if let Err(error) = build_result {
            let _ = std::fs::remove_file(&archive_staging);
            rollback_and_checkpoint(journal, state).await;
            return Err(error);
        }
        if let Err(error) = publish_staged(&archive_staging, &item.archive, force) {
            let _ = std::fs::remove_file(&archive_staging);
            rollback_and_checkpoint(journal, state).await;
            return Err(error);
        }
        journal.staging_paths.retain(|path| path != &archive_staging);
        if let Err(error) = checkpoint_output("archive", item.archive.clone(), journal, state).await {
            rollback_and_checkpoint(journal, state).await;
            return Err(error);
        }

        if let Some(backup) = &item.backup {
            let backup_staging = match staging_path(backup, &journal.id, "backup") {
                Ok(path) => path,
                Err(error) => {
                    rollback_and_checkpoint(journal, state).await;
                    return Err(error);
                }
            };
            if let Err(error) = checkpoint_staging(&backup_staging, journal, state).await {
                rollback_and_checkpoint(journal, state).await;
                return Err(error);
            }
            if let Some(parent) = backup_staging.parent() {
                if let Err(error) = std::fs::create_dir_all(parent) {
                    rollback_and_checkpoint(journal, state).await;
                    return Err(format!("compress: could not create {}: {error}", parent.display()));
                }
            }
            if let Err(error) = std::fs::copy(&item.archive, &backup_staging) {
                let _ = std::fs::remove_file(&backup_staging);
                rollback_and_checkpoint(journal, state).await;
                return Err(format!("compress: could not stage backup {}: {error}", backup.display()));
            }
            if let Err(error) = publish_staged(&backup_staging, backup, force) {
                let _ = std::fs::remove_file(&backup_staging);
                rollback_and_checkpoint(journal, state).await;
                return Err(error);
            }
            journal.staging_paths.retain(|path| path != &backup_staging);
            if let Err(error) = checkpoint_output("backup", backup.clone(), journal, state).await {
                rollback_and_checkpoint(journal, state).await;
                return Err(error);
            }
        }
    }
    Ok(())
}

fn validate_archive_destination(path: &std::path::Path, force: bool) -> Result<(), String> {
    if path.exists() && !force {
        return Err(format!("compress: {}: destination already exists (use --force to overwrite)", path.display()));
    }
    Ok(())
}

fn staging_path(path: &std::path::Path, journal_id: &str, role: &str) -> Result<PathBuf, String> {
    let parent = path.parent().filter(|value| !value.as_os_str().is_empty()).unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(parent).map_err(|error| format!("compress: could not create {}: {error}", parent.display()))?;
    let name = path.file_name().ok_or_else(|| format!("compress: invalid destination: {}", path.display()))?.to_string_lossy();
    Ok(parent.join(format!(".{name}.{journal_id}.{role}.tmp")))
}

fn publish_staged(staging: &std::path::Path, destination: &std::path::Path, force: bool) -> Result<(), String> {
    if destination.exists() {
        if !force { return Err(format!("compress: destination appeared during apply: {}", destination.display())); }
        std::fs::remove_file(destination).map_err(|error| format!("compress: could not replace {}: {error}", destination.display()))?;
    }
    std::fs::rename(staging, destination).map_err(|error| format!("compress: could not publish {}: {error}", destination.display()))
}

async fn checkpoint_output(role: &str, path: PathBuf, journal: &mut OperationJournal, state: &crate::state::StateHandle) -> Result<(), String> {
    journal.outputs.push(journal_output(role, path)?);
    state.put_journal(journal.clone()).await
}

async fn checkpoint_staging(path: &std::path::Path, journal: &mut OperationJournal, state: &crate::state::StateHandle) -> Result<(), String> {
    journal.staging_paths.push(path.to_path_buf());
    if let Err(error) = state.put_journal(journal.clone()).await {
        journal.staging_paths.retain(|candidate| candidate != path);
        return Err(format!("apply: could not persist staging intent: {error}"));
    }
    Ok(())
}

async fn rollback_and_checkpoint(journal: &mut OperationJournal, state: &crate::state::StateHandle) {
    transactional_rollback(journal);
    let _ = state.put_journal(journal.clone()).await;
}

async fn apply_delete(plan: &DeletePlan, journal: &mut OperationJournal, state: &crate::state::StateHandle) -> Result<(), String> {
    for item in &plan.items {
        validate_record(item, "apply: source drift")?;
        crate::delete::validate_planned(&item.path, plan.options)?;
    }
    for item in &plan.items {
        let path = item.path.clone();
        let options = plan.options;
        let result = tokio::task::spawn_blocking(move || crate::delete::delete_planned(&path, options)).await.map_err(|error| format!("apply: delete worker failed: {error}"))?;
        if let Err(error) = result {
            journal.status = if journal.outputs.is_empty() { "failed" } else { "partially_applied" }.to_string();
            let _ = state.put_journal(journal.clone()).await;
            return Err(format!("apply: delete: {error}"));
        }
        journal.outputs.push(JournalOutput { role: "recycled".to_string(), record: item.clone(), original_path: Some(item.path.clone()) });
        state.put_journal(journal.clone()).await?;
    }
    Ok(())
}

fn journal_output(role: &str, path: PathBuf) -> Result<JournalOutput, String> {
    let record = FileRecord::from_path(path.clone(), None).map_err(|error| format!("apply: could not journal {}: {error}", path.display()))?;
    Ok(JournalOutput { role: role.to_string(), record, original_path: None })
}

fn plan_transfer(fileset: &crate::fileset::FileSet, destination: &str, target: fn(&std::path::Path, &std::path::Path) -> PathBuf, operation: &str) -> Result<FileTransferPlan, String> {
    let destination = std::path::Path::new(destination);
    let mut seen = std::collections::HashSet::new();
    let mut items = Vec::with_capacity(fileset.files.len());
    for source in &fileset.files {
        if source.kind != FileKind::File { return Err(format!("{operation}: --plan currently accepts file records only: {}", source.path.display())); }
        let output = target(destination, &source.path);
        if source.path == output { return Err(format!("{operation}: source and destination are the same: {}", source.path.display())); }
        if !seen.insert(output.to_string_lossy().to_lowercase()) { return Err(format!("{operation}: multiple sources target the same destination: {}", output.display())); }
        let destination_before = if output.exists() { Some(FileRecord::from_path(output.clone(), None).map_err(|error| format!("{operation}: cannot inspect {}: {error}", output.display()))?) } else { None };
        items.push(FileTransferPlanItem { source: source.clone(), destination: output, destination_before });
    }
    Ok(FileTransferPlan { items })
}

fn transfer_table(plan: &FileTransferPlan, operation: &str) -> Table {
    Table { rows: plan.items.iter().map(|item| vec![
        ("operation".to_string(), operation.to_string()),
        ("source".to_string(), item.source.path.to_string_lossy().into_owned()),
        ("destination".to_string(), item.destination.to_string_lossy().into_owned()),
        ("destination_exists".to_string(), item.destination_before.is_some().to_string()),
    ]).collect() }
}

async fn apply_transfer(plan: &FileTransferPlan, force: bool, moving: bool, journal: &mut OperationJournal, state: &crate::state::StateHandle) -> Result<(), String> {
    for item in &plan.items {
        validate_record(&item.source, "apply: source drift")?;
        match (&item.destination_before, item.destination.exists()) {
            (None, true) => return Err(format!("apply: destination drift: {} appeared after planning", item.destination.display())),
            (Some(_), false) => return Err(format!("apply: destination drift: {} disappeared after planning", item.destination.display())),
            (Some(expected), true) => validate_record(expected, "apply: destination drift")?,
            (None, false) => {}
        }
        if item.destination_before.is_some() && !force { return Err(format!("apply: {} exists (use --force when planning to replace it)", item.destination.display())); }
    }
    for item in &plan.items {
        let source = item.source.path.clone();
        let destination = item.destination.clone();
        let result = if moving {
            tokio::task::spawn_blocking(move || crate::fs_ops::move_one(&source, &destination, force)).await
        } else {
            tokio::task::spawn_blocking(move || crate::copy::copy_one(&source, &destination, force)).await
        }.map_err(|error| format!("apply: worker failed: {error}"))?;
        if let Err(error) = result {
            transactional_rollback(journal);
            let _ = state.put_journal(journal.clone()).await;
            return Err(format!("apply: {error}"));
        }
        let record = FileRecord::from_path(item.destination.clone(), None).map_err(|error| format!("apply: could not journal {}: {error}", item.destination.display()))?;
        journal.outputs.push(JournalOutput { role: if moving { "moved" } else { "copy" }.to_string(), record, original_path: moving.then(|| item.source.path.clone()) });
        state.put_journal(journal.clone()).await?;
    }
    Ok(())
}

fn transactional_rollback(journal: &mut OperationJournal) {
    if !journal.undo_safe { journal.status = "partially_applied".to_string(); return; }
    let mut complete = true;
    for output in journal.outputs.iter().rev() {
        let result = if let Some(original) = &output.original_path { crate::fs_ops::move_one(&output.record.path, original, false) } else { std::fs::remove_file(&output.record.path).map_err(|error| error.to_string()) };
        if result.is_err() { complete = false; }
    }
    journal.status = if complete { "rolled_back" } else { "partially_applied" }.to_string();
}

fn validate_record(expected: &FileRecord, label: &str) -> Result<(), String> {
    let current = FileRecord::from_path(expected.path.clone(), None).map_err(|error| format!("{label}: {} is unavailable: {error}", expected.path.display()))?;
    if current.identity != expected.identity || current.kind != expected.kind || current.size != expected.size || current.modified != expected.modified {
        return Err(format!("{label}: {} changed", expected.path.display()));
    }
    Ok(())
}

impl OperationJournal {
    pub fn needs_recovery(&self) -> bool {
        matches!(self.status.as_str(), "in_progress" | "partially_applied" | "failed")
    }

    pub fn recover_rollback(&self) -> Result<Self, String> {
        if !self.needs_recovery() {
            return Err(format!("recover: operation {} has status '{}' and does not need recovery", self.id, self.status));
        }
        if !self.undo_safe { return Err(format!("recover: operation {} is not safely reversible", self.id)); }
        for output in &self.outputs {
            validate_record(&output.record, "recover: output drift")?;
            if let Some(original) = &output.original_path {
                if original.exists() { return Err(format!("recover: original path is no longer free: {}", original.display())); }
            }
        }
        for path in &self.staging_paths {
            if path.exists() {
                std::fs::remove_file(path).map_err(|error| format!("recover: could not remove staged file {}: {error}", path.display()))?;
            }
        }
        let mut recovered = self.rollback().map_err(|error| error.replacen("rollback:", "recover:", 1))?;
        recovered.staging_paths.clear();
        Ok(recovered)
    }

    pub fn to_table(&self) -> Table {
        Table { rows: self.outputs.iter().map(|output| vec![
            ("operation_id".to_string(), self.id.clone()),
            ("plan_id".to_string(), self.plan_id.clone()),
            ("operation".to_string(), self.operation.clone()),
            ("status".to_string(), self.status.clone()),
            ("role".to_string(), output.role.clone()),
            ("path".to_string(), output.record.path.to_string_lossy().into_owned()),
            ("original_path".to_string(), output.original_path.as_ref().map(|path| path.to_string_lossy().into_owned()).unwrap_or_default()),
            ("size".to_string(), output.record.size.to_string()),
            ("undo_safe".to_string(), self.undo_safe.to_string()),
            ("started_at".to_string(), self.started_at.to_string()),
            ("finished_at".to_string(), self.finished_at.to_string()),
        ]).collect() }
    }

    /// Validates every output before deleting the first one. A modified,
    /// replaced, or missing output makes the entire undo fail closed.
    pub fn undo(&self) -> Result<Self, String> {
        if self.status != "applied" { return Err(format!("undo: operation {} has status '{}'", self.id, self.status)); }
        if self.undone_at.is_some() { return Err(format!("undo: operation {} was already undone", self.id)); }
        if !self.undo_safe { return Err(format!("undo: operation {} replaced existing output and cannot be safely undone", self.id)); }
        for output in &self.outputs {
            let current = FileRecord::from_path(output.record.path.clone(), None).map_err(|error| format!("undo: output drift: {} is unavailable: {error}", output.record.path.display()))?;
            if current.identity != output.record.identity || current.kind != output.record.kind || current.size != output.record.size || current.modified != output.record.modified {
                return Err(format!("undo: output drift: {} changed after apply", output.record.path.display()));
            }
            if let Some(original) = &output.original_path {
                if original.exists() { return Err(format!("undo: original path is no longer free: {}", original.display())); }
            }
        }
        for output in &self.outputs {
            if let Some(original) = &output.original_path {
                crate::fs_ops::move_one(&output.record.path, original, false).map_err(|error| format!("undo: could not restore {}: {error}", original.display()))?;
            } else {
                std::fs::remove_file(&output.record.path).map_err(|error| format!("undo: could not remove {}: {error}", output.record.path.display()))?;
            }
        }
        let mut undone = self.clone();
        undone.undone_at = Some(now_seconds());
        undone.status = "undone".to_string();
        Ok(undone)
    }

    pub fn rollback(&self) -> Result<Self, String> {
        if !matches!(self.status.as_str(), "in_progress" | "partially_applied" | "failed") {
            return Err(format!("rollback: operation {} has status '{}'", self.id, self.status));
        }
        if !self.undo_safe { return Err(format!("rollback: operation {} is not safely reversible", self.id)); }
        for output in &self.outputs {
            validate_record(&output.record, "rollback: output drift")?;
            if let Some(original) = &output.original_path {
                if original.exists() { return Err(format!("rollback: original path is no longer free: {}", original.display())); }
            }
        }
        let mut rolled_back = self.clone();
        transactional_rollback(&mut rolled_back);
        if rolled_back.status != "rolled_back" { return Err(format!("rollback: operation {} could not be fully rolled back", self.id)); }
        rolled_back.finished_at = now_seconds();
        Ok(rolled_back)
    }

    pub fn to_json(&self) -> String {
        let outputs = self.outputs.iter().map(|output| serde_json::json!({
            "role": output.role, "path": output.record.path.to_string_lossy(),
            "volume_id": output.record.identity.volume_id,
            "file_id": output.record.identity.file_id.map(hex_file_id),
            "kind": kind_name(&output.record.kind), "size": output.record.size,
            "modified": output.record.modified,
            "original_path": output.original_path.as_ref().map(|path| path.to_string_lossy())
        })).collect::<Vec<_>>();
        serde_json::json!({"version":3,"id":self.id,"plan_id":self.plan_id,"operation":self.operation,"started_at":self.started_at,"finished_at":self.finished_at,"undo_safe":self.undo_safe,"status":self.status,"error":self.error,"undone_at":self.undone_at,"staging_paths":self.staging_paths.iter().map(|path| path.to_string_lossy()).collect::<Vec<_>>(),"outputs":outputs}).to_string()
    }

    pub fn from_json(text: &str) -> Result<Self, String> {
        let value: serde_json::Value = serde_json::from_str(text).map_err(|error| format!("journal: invalid persisted JSON: {error}"))?;
        let required_string = |name: &str| value.get(name).and_then(|v| v.as_str()).map(str::to_string).ok_or_else(|| format!("journal: missing '{name}'"));
        let required_number = |name: &str| value.get(name).and_then(|v| v.as_u64()).ok_or_else(|| format!("journal: missing '{name}'"));
        let mut outputs = Vec::new();
        for output in value.get("outputs").and_then(|v| v.as_array()).ok_or_else(|| "journal: missing 'outputs'".to_string())? {
            let file_id = output.get("file_id").and_then(|v| v.as_str()).map(parse_file_id).transpose()?;
            outputs.push(JournalOutput { role: output.get("role").and_then(|v| v.as_str()).unwrap_or("").to_string(), original_path: output.get("original_path").and_then(|v| v.as_str()).map(PathBuf::from), record: FileRecord {
                path: PathBuf::from(output.get("path").and_then(|v| v.as_str()).ok_or_else(|| "journal: output missing path".to_string())?),
                identity: FileIdentity { volume_id: output.get("volume_id").and_then(|v| v.as_u64()), file_id },
                kind: parse_kind(output.get("kind").and_then(|v| v.as_str()).unwrap_or("other")),
                size: output.get("size").and_then(|v| v.as_u64()).unwrap_or(0), modified: output.get("modified").and_then(|v| v.as_u64()), sha256: None,
            } });
        }
        let undone_at = value.get("undone_at").and_then(|v| v.as_u64());
        let status = value.get("status").and_then(|v| v.as_str()).map(str::to_string).unwrap_or_else(|| if undone_at.is_some() { "undone" } else { "applied" }.to_string());
        let staging_paths = value.get("staging_paths").and_then(|v| v.as_array()).map(|values| values.iter().filter_map(|value| value.as_str().map(PathBuf::from)).collect()).unwrap_or_default();
        Ok(Self { id: required_string("id")?, plan_id: required_string("plan_id")?, operation: required_string("operation")?, started_at: required_number("started_at")?, finished_at: required_number("finished_at")?, outputs, staging_paths, undo_safe: value.get("undo_safe").and_then(|v| v.as_bool()).unwrap_or(false), status, error: value.get("error").and_then(|v| v.as_str()).map(str::to_string), undone_at })
    }
}

fn hex_file_id(bytes: [u8; 16]) -> String { bytes.iter().map(|byte| format!("{byte:02x}")).collect() }
fn kind_name(kind: &FileKind) -> &'static str { match kind { FileKind::File => "file", FileKind::Directory => "directory", FileKind::Symlink => "symlink", FileKind::Other => "other" } }
fn parse_kind(value: &str) -> FileKind { match value { "file" => FileKind::File, "directory" => FileKind::Directory, "symlink" => FileKind::Symlink, _ => FileKind::Other } }
fn parse_file_id(value: &str) -> Result<[u8; 16], String> {
    if value.len() != 32 { return Err("journal: invalid file identity".to_string()); }
    let mut bytes = [0; 16];
    for (index, byte) in bytes.iter_mut().enumerate() { *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).map_err(|_| "journal: invalid file identity".to_string())?; }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_round_trips_persistent_json() {
        let journal = OperationJournal { id: "operation-1".into(), plan_id: "plan-1".into(), operation: "compress".into(), started_at: 1, finished_at: 2, outputs: vec![JournalOutput { role: "archive".into(), record: FileRecord { path: "a.zip".into(), identity: FileIdentity { volume_id: Some(7), file_id: Some([3; 16]) }, kind: FileKind::File, size: 10, modified: Some(9), sha256: None }, original_path: None }], staging_paths: Vec::new(), undo_safe: true, status: "applied".into(), error: None, undone_at: None };
        assert_eq!(OperationJournal::from_json(&journal.to_json()).unwrap(), journal);
    }

    fn temp_output(name: &str, contents: &str) -> (PathBuf, FileRecord) {
        let path = std::env::temp_dir().join(format!("ion-win-journal-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, contents).unwrap();
        let record = FileRecord::from_path(path.clone(), None).unwrap();
        (path, record)
    }

    #[test]
    fn undo_removes_unchanged_created_outputs() {
        let (path, record) = temp_output("undo.zip", "archive");
        let journal = OperationJournal { id: "operation-undo".into(), plan_id: "plan".into(), operation: "compress".into(), started_at: 1, finished_at: 2, outputs: vec![JournalOutput { role: "archive".into(), record, original_path: None }], staging_paths: Vec::new(), undo_safe: true, status: "applied".into(), error: None, undone_at: None };
        let undone = journal.undo().unwrap();
        assert!(!path.exists());
        assert!(undone.undone_at.is_some());
    }

    #[test]
    fn undo_validates_all_outputs_before_deleting_any() {
        let (first_path, first) = temp_output("atomic-a.zip", "first");
        let (second_path, second) = temp_output("atomic-b.zip", "second");
        let journal = OperationJournal { id: "operation-drift".into(), plan_id: "plan".into(), operation: "compress".into(), started_at: 1, finished_at: 2, outputs: vec![JournalOutput { role: "archive".into(), record: first, original_path: None }, JournalOutput { role: "backup".into(), record: second, original_path: None }], staging_paths: Vec::new(), undo_safe: true, status: "applied".into(), error: None, undone_at: None };
        std::fs::write(&second_path, "changed-size").unwrap();
        let error = journal.undo().unwrap_err();
        assert!(error.contains("output drift"), "{error}");
        assert!(first_path.exists(), "no output may be removed when validation fails");
        assert!(second_path.exists());
        let _ = std::fs::remove_file(first_path);
        let _ = std::fs::remove_file(second_path);
    }

    fn transfer_fixture(name: &str) -> (PathBuf, PathBuf, crate::fileset::FileSet) {
        let root = std::env::temp_dir().join(format!("ion-win-transfer-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let source = root.join("source").join("file.txt");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, "typed transfer").unwrap();
        let fileset = crate::fileset::FileSet::new(vec![FileRecord::from_path(source.clone(), None).unwrap()], "test");
        (root, source, fileset)
    }

    #[tokio::test]
    async fn copy_plan_applies_and_undo_removes_the_typed_destination() {
        let (root, source, fileset) = transfer_fixture("copy");
        let plan = OperationPlan::copy(&fileset, &root.join("copies").to_string_lossy(), false).unwrap();
        let destination = match &plan.operation { PlannedOperation::Copy(plan) => plan.items[0].destination.clone(), _ => unreachable!() };
        let journal = plan.apply(&crate::state::spawn_memory()).await.unwrap();
        assert_eq!(std::fs::read_to_string(&destination).unwrap(), "typed transfer");
        assert!(source.exists());
        assert_eq!(journal.operation, "copy");
        journal.undo().unwrap();
        assert!(!destination.exists());
        assert!(source.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn move_plan_undo_restores_the_original_path() {
        let (root, source, fileset) = transfer_fixture("move");
        let plan = OperationPlan::move_files(&fileset, &root.join("moved").to_string_lossy(), false).unwrap();
        let destination = match &plan.operation { PlannedOperation::Move(plan) => plan.items[0].destination.clone(), _ => unreachable!() };
        let journal = plan.apply(&crate::state::spawn_memory()).await.unwrap();
        assert!(!source.exists());
        assert!(destination.exists());
        assert_eq!(journal.outputs[0].original_path.as_ref(), Some(&source));
        journal.undo().unwrap();
        assert!(source.exists());
        assert!(!destination.exists());
        assert_eq!(std::fs::read_to_string(&source).unwrap(), "typed transfer");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn transfer_apply_rejects_source_drift_before_writes() {
        let (root, source, fileset) = transfer_fixture("drift");
        let plan = OperationPlan::copy(&fileset, &root.join("copies").to_string_lossy(), false).unwrap();
        let destination = match &plan.operation { PlannedOperation::Copy(plan) => plan.items[0].destination.clone(), _ => unreachable!() };
        std::fs::write(&source, "changed after planning and a different size").unwrap();
        let error = plan.apply(&crate::state::spawn_memory()).await.unwrap_err();
        assert!(error.contains("source drift"), "{error}");
        assert!(!destination.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn forced_transfer_that_replaces_a_destination_is_not_undoable() {
        let (root, _source, fileset) = transfer_fixture("force-undo");
        let destination_root = root.join("copies");
        let planned_destination = crate::copy::resource_target(&destination_root, &fileset.files[0].path);
        std::fs::create_dir_all(planned_destination.parent().unwrap()).unwrap();
        std::fs::write(&planned_destination, "existing destination").unwrap();
        let plan = OperationPlan::copy(&fileset, &destination_root.to_string_lossy(), true).unwrap();
        let journal = plan.apply(&crate::state::spawn_memory()).await.unwrap();
        assert!(!journal.undo_safe);
        assert!(journal.undo().unwrap_err().contains("cannot be safely undone"));
        assert!(planned_destination.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn transactional_rollback_removes_completed_safe_copy_outputs() {
        let (path, record) = temp_output("transaction-copy.tmp", "completed copy");
        let mut journal = OperationJournal { id: "operation-transaction".into(), plan_id: "plan".into(), operation: "copy".into(), started_at: 1, finished_at: 0, outputs: vec![JournalOutput { role: "copy".into(), record, original_path: None }], staging_paths: Vec::new(), undo_safe: true, status: "in_progress".into(), error: None, undone_at: None };
        transactional_rollback(&mut journal);
        assert_eq!(journal.status, "rolled_back");
        assert!(!path.exists());
    }

    #[test]
    fn recovery_rolls_back_a_crash_checkpoint_after_identity_validation() {
        let (path, record) = temp_output("recover-copy.tmp", "checkpointed copy");
        let journal = OperationJournal { id: "operation-recover".into(), plan_id: "plan".into(), operation: "copy".into(), started_at: 1, finished_at: 0, outputs: vec![JournalOutput { role: "copy".into(), record, original_path: None }], staging_paths: Vec::new(), undo_safe: true, status: "in_progress".into(), error: None, undone_at: None };
        assert!(journal.needs_recovery());
        let recovered = journal.recover_rollback().unwrap();
        assert_eq!(recovered.status, "rolled_back");
        assert!(!path.exists());
        assert!(!recovered.needs_recovery());
    }

    #[test]
    fn recovery_fails_closed_when_a_checkpointed_output_drifted() {
        let (path, record) = temp_output("recover-drift.tmp", "before");
        let journal = OperationJournal { id: "operation-recover-drift".into(), plan_id: "plan".into(), operation: "compress".into(), started_at: 1, finished_at: 0, outputs: vec![JournalOutput { role: "archive".into(), record, original_path: None }], staging_paths: Vec::new(), undo_safe: true, status: "partially_applied".into(), error: None, undone_at: None };
        std::fs::write(&path, "changed and now a different size").unwrap();
        let error = journal.recover_rollback().unwrap_err();
        assert!(error.contains("output drift"), "{error}");
        assert!(path.exists());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn recovery_removes_only_the_exact_persisted_staging_intent() {
        let staged = std::env::temp_dir().join(format!("ion-win-recover-stage-{}.tmp", std::process::id()));
        let unrelated = std::env::temp_dir().join(format!("ion-win-recover-unrelated-{}.tmp", std::process::id()));
        std::fs::write(&staged, "partial zip").unwrap();
        std::fs::write(&unrelated, "must survive").unwrap();
        let journal = OperationJournal { id: "operation-stage".into(), plan_id: "plan".into(), operation: "compress".into(), started_at: 1, finished_at: 0, outputs: Vec::new(), staging_paths: vec![staged.clone()], undo_safe: true, status: "in_progress".into(), error: None, undone_at: None };

        let recovered = journal.recover_rollback().unwrap();

        assert_eq!(recovered.status, "rolled_back");
        assert!(recovered.staging_paths.is_empty());
        assert!(!staged.exists());
        assert!(unrelated.exists());
        let _ = std::fs::remove_file(unrelated);
    }

    #[tokio::test]
    async fn delete_plan_source_drift_persists_a_failed_journal_before_writes() {
        let (root, source, fileset) = transfer_fixture("delete-drift");
        let plan = OperationPlan::delete(&fileset, crate::delete::DeleteOptions::default()).unwrap();
        std::fs::write(&source, "changed after delete planning").unwrap();
        let state = crate::state::spawn_memory();
        let error = plan.apply(&state).await.unwrap_err();
        assert!(error.contains("source drift"), "{error}");
        assert!(source.exists());
        let journals = state.list_journals().await.unwrap();
        assert_eq!(journals.len(), 1);
        assert_eq!(journals[0].status, "failed");
        assert!(journals[0].outputs.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn archive_plan_checkpoints_each_atomic_output_and_undo_removes_them() {
        let (root, source, fileset) = transfer_fixture("archive-transaction");
        let fileset = fileset.with_roots(vec![source.parent().unwrap().to_path_buf()]);
        let archives = root.join("archives");
        let backups = root.join("backups");
        let archive_plan = crate::compress::plan_fileset_per_root(
            &fileset,
            &archives.to_string_lossy(),
            Some(&backups.to_string_lossy()),
        ).unwrap();
        let plan = OperationPlan::archive(archive_plan, false);
        let state = crate::state::spawn_memory();

        let journal = plan.apply(&state).await.unwrap();

        assert_eq!(journal.status, "applied");
        assert_eq!(journal.outputs.iter().map(|output| output.role.as_str()).collect::<Vec<_>>(), vec!["archive", "backup"]);
        assert!(journal.outputs.iter().all(|output| output.record.path.exists()));
        let undone = journal.undo().unwrap();
        assert_eq!(undone.status, "undone");
        assert!(journal.outputs.iter().all(|output| !output.record.path.exists()));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn archive_backup_failure_rolls_back_an_already_published_archive() {
        let (root, source, fileset) = transfer_fixture("archive-rollback");
        let fileset = fileset.with_roots(vec![source.parent().unwrap().to_path_buf()]);
        let archives = root.join("archives");
        let backups = root.join("blocked-backups");
        let archive_plan = crate::compress::plan_fileset_per_root(
            &fileset,
            &archives.to_string_lossy(),
            Some(&backups.to_string_lossy()),
        ).unwrap();
        let archive_path = archive_plan.items[0].archive.clone();
        std::fs::write(&backups, "blocks creation of the backup directory").unwrap();
        let plan = OperationPlan::archive(archive_plan, false);
        let state = crate::state::spawn_memory();

        let error = plan.apply(&state).await.unwrap_err();

        assert!(error.contains("could not create"), "{error}");
        assert!(!archive_path.exists(), "published archive must be rolled back when backup staging fails");
        let journals = state.list_journals().await.unwrap();
        assert_eq!(journals[0].status, "rolled_back");
        assert_eq!(journals[0].outputs.len(), 1, "the completed archive remains as durable rollback evidence");
        let _ = std::fs::remove_dir_all(root);
    }
}
