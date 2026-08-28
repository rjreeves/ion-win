use crate::compress::ArchivePlan;
use crate::fileset::{FileIdentity, FileKind, FileRecord};
use crate::table::Table;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static OPERATION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CrashPoint {
    InitialJournal,
    StagingIntent,
    ZipStaged,
    Published,
    OutputCheckpoint,
    ArchiveBeforeBackup,
    BeforeApplied,
    BeforeTransferMutation,
    TransferMutated,
    TransferCheckpoint,
    BetweenTransferRecords,
}

fn injected_crash(selected: Option<CrashPoint>, point: CrashPoint) -> Result<(), String> {
    if selected == Some(point) { Err(format!("__ion_test_crash__:{point:?}")) } else { Ok(()) }
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryStep { pub operation: String, pub source: FileRecord, pub destination: PathBuf }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveRecoveryEntry { pub archive_name: String, pub source: FileRecord }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveRecoveryStep { pub root: PathBuf, pub archive: PathBuf, pub backup: Option<PathBuf>, pub entries: Vec<ArchiveRecoveryEntry> }

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
    pub remaining_steps: Vec<RecoveryStep>,
    pub remaining_archives: Vec<ArchiveRecoveryStep>,
    pub resume_supported: bool,
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
        self.apply_internal(state, None).await
    }

    async fn apply_internal(&self, state: &crate::state::StateHandle, crash: Option<CrashPoint>) -> Result<OperationJournal, String> {
        let started_at = now_seconds();
        let operation = match &self.operation { PlannedOperation::Archive(_) => "compress", PlannedOperation::Copy(_) => "copy", PlannedOperation::Move(_) => "move", PlannedOperation::Delete(_) => "delete" };
        let initially_undo_safe = match &self.operation {
            PlannedOperation::Archive(plan) => plan.items.iter().all(|item| !item.archive.exists() && item.backup.as_ref().is_none_or(|path| !path.exists())),
            PlannedOperation::Copy(plan) | PlannedOperation::Move(plan) => plan.items.iter().all(|item| item.destination_before.is_none()),
            PlannedOperation::Delete(_) => false,
        };
        let remaining_steps = match &self.operation {
            PlannedOperation::Copy(plan) => plan.items.iter().map(|item| RecoveryStep { operation: "copy".to_string(), source: item.source.clone(), destination: item.destination.clone() }).collect(),
            PlannedOperation::Move(plan) => plan.items.iter().map(|item| RecoveryStep { operation: "move".to_string(), source: item.source.clone(), destination: item.destination.clone() }).collect(),
            _ => Vec::new(),
        };
        let remaining_archives = match &self.operation {
            PlannedOperation::Archive(plan) => plan.items.iter().map(|item| ArchiveRecoveryStep { root: item.root.clone(), archive: item.archive.clone(), backup: item.backup.clone(), entries: item.entries.iter().map(|entry| ArchiveRecoveryEntry { archive_name: entry.archive_name.clone(), source: entry.source.clone() }).collect() }).collect(),
            _ => Vec::new(),
        };
        let resume_supported = matches!(self.operation, PlannedOperation::Copy(_) | PlannedOperation::Move(_) | PlannedOperation::Archive(_));
        let mut journal = OperationJournal { id: unique_id("operation"), plan_id: self.id.clone(), operation: operation.to_string(), started_at, finished_at: 0, outputs: Vec::new(), staging_paths: Vec::new(), remaining_steps, remaining_archives, resume_supported, undo_safe: initially_undo_safe, status: "in_progress".to_string(), error: None, undone_at: None };
        state.put_journal(journal.clone()).await?;
        injected_crash(crash, CrashPoint::InitialJournal)?;
        let result: Result<(), String> = match &self.operation {
            PlannedOperation::Archive(plan) => {
                apply_archive_transaction(plan, self.force, &mut journal, state, crash).await
            },
            PlannedOperation::Copy(plan) => apply_transfer(plan, self.force, false, &mut journal, state, crash).await,
            PlannedOperation::Move(plan) => apply_transfer(plan, self.force, true, &mut journal, state, crash).await,
            PlannedOperation::Delete(plan) => apply_delete(plan, &mut journal, state).await,
        };
        match result {
            Ok(()) => {
                injected_crash(crash, CrashPoint::BeforeApplied)?;
                journal.status = "applied".to_string();
                journal.finished_at = now_seconds();
                state.put_journal(journal.clone()).await?;
                Ok(journal)
            }
            Err(error) => {
                if error.starts_with("__ion_test_crash__:") { return Err(error); }
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
    crash: Option<CrashPoint>,
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
        injected_crash(crash, CrashPoint::StagingIntent)?;
        let build_result = crate::compress::build_planned_archive(item, &archive_staging).await;
        if let Err(error) = build_result {
            let _ = std::fs::remove_file(&archive_staging);
            rollback_and_checkpoint(journal, state).await;
            return Err(error);
        }
        injected_crash(crash, CrashPoint::ZipStaged)?;
        let mut pending_record = FileRecord::from_path(archive_staging.clone(), None)
            .map_err(|error| format!("compress: could not identify staged archive: {error}"))?;
        pending_record.path = item.archive.clone();
        journal.outputs.push(JournalOutput { role: "pending_archive".to_string(), record: pending_record, original_path: None });
        if let Err(error) = state.put_journal(journal.clone()).await {
            journal.outputs.retain(|output| output.role != "pending_archive");
            let _ = std::fs::remove_file(&archive_staging);
            rollback_and_checkpoint(journal, state).await;
            return Err(format!("apply: could not persist publication intent: {error}"));
        }
        if let Err(error) = publish_staged(&archive_staging, &item.archive, force) {
            journal.outputs.retain(|output| output.role != "pending_archive");
            journal.staging_paths.retain(|path| path != &archive_staging);
            let _ = std::fs::remove_file(&archive_staging);
            rollback_and_checkpoint(journal, state).await;
            return Err(error);
        }
        injected_crash(crash, CrashPoint::Published)?;
        journal.outputs.retain(|output| output.role != "pending_archive");
        journal.staging_paths.retain(|path| path != &archive_staging);
        if let Err(error) = checkpoint_output("archive", item.archive.clone(), journal, state).await {
            rollback_and_checkpoint(journal, state).await;
            return Err(error);
        }
        injected_crash(crash, CrashPoint::OutputCheckpoint)?;

        if let Some(backup) = &item.backup {
            injected_crash(crash, CrashPoint::ArchiveBeforeBackup)?;
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
            let mut pending_record = FileRecord::from_path(backup_staging.clone(), None)
                .map_err(|error| format!("compress: could not identify staged backup: {error}"))?;
            pending_record.path = backup.clone();
            journal.outputs.push(JournalOutput { role: "pending_backup".to_string(), record: pending_record, original_path: None });
            if let Err(error) = state.put_journal(journal.clone()).await {
                journal.outputs.retain(|output| output.role != "pending_backup");
                let _ = std::fs::remove_file(&backup_staging);
                rollback_and_checkpoint(journal, state).await;
                return Err(format!("apply: could not persist publication intent: {error}"));
            }
            if let Err(error) = publish_staged(&backup_staging, backup, force) {
                journal.outputs.retain(|output| output.role != "pending_backup");
                journal.staging_paths.retain(|path| path != &backup_staging);
                let _ = std::fs::remove_file(&backup_staging);
                rollback_and_checkpoint(journal, state).await;
                return Err(error);
            }
            journal.outputs.retain(|output| output.role != "pending_backup");
            journal.staging_paths.retain(|path| path != &backup_staging);
            if let Err(error) = checkpoint_output("backup", backup.clone(), journal, state).await {
                rollback_and_checkpoint(journal, state).await;
                return Err(error);
            }
        }
        journal.remaining_archives.retain(|remaining| remaining.archive != item.archive);
        state.put_journal(journal.clone()).await?;
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

async fn apply_transfer(plan: &FileTransferPlan, force: bool, moving: bool, journal: &mut OperationJournal, state: &crate::state::StateHandle, crash: Option<CrashPoint>) -> Result<(), String> {
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
    for (index, item) in plan.items.iter().enumerate() {
        injected_crash(crash, CrashPoint::BeforeTransferMutation)?;
        let source = item.source.path.clone();
        let destination = item.destination.clone();
        if moving {
            let mut pending = item.source.clone();
            pending.path = destination.clone();
            journal.outputs.push(JournalOutput { role: "pending_move".to_string(), record: pending, original_path: Some(source.clone()) });
            if let Err(error) = state.put_journal(journal.clone()).await {
                journal.outputs.retain(|output| output.role != "pending_move");
                rollback_and_checkpoint(journal, state).await;
                return Err(format!("apply: could not persist move intent: {error}"));
            }
            let result = tokio::task::spawn_blocking(move || crate::fs_ops::move_one(&source, &destination, force)).await
                .map_err(|error| format!("apply: worker failed: {error}"))?;
            if let Err(error) = result {
                journal.outputs.retain(|output| output.role != "pending_move");
                rollback_and_checkpoint(journal, state).await;
                return Err(format!("apply: {error}"));
            }
            injected_crash(crash, CrashPoint::TransferMutated)?;
            journal.outputs.retain(|output| output.role != "pending_move");
        } else {
            let staging = match staging_path(&destination, &journal.id, "copy") {
                Ok(path) => path,
                Err(error) => {
                    rollback_and_checkpoint(journal, state).await;
                    return Err(error);
                }
            };
            if let Err(error) = checkpoint_staging(&staging, journal, state).await {
                rollback_and_checkpoint(journal, state).await;
                return Err(error);
            }
            let staging_target = staging.clone();
            let result = tokio::task::spawn_blocking(move || crate::copy::copy_one(&source, &staging_target, false)).await
                .map_err(|error| format!("apply: worker failed: {error}"))?;
            if let Err(error) = result {
                let _ = std::fs::remove_file(&staging);
                journal.staging_paths.retain(|path| path != &staging);
                rollback_and_checkpoint(journal, state).await;
                return Err(format!("apply: {error}"));
            }
            let mut pending = FileRecord::from_path(staging.clone(), None).map_err(|error| format!("apply: could not identify staged copy: {error}"))?;
            pending.path = destination.clone();
            journal.outputs.push(JournalOutput { role: "pending_copy".to_string(), record: pending, original_path: None });
            if let Err(error) = state.put_journal(journal.clone()).await {
                journal.outputs.retain(|output| output.role != "pending_copy");
                journal.staging_paths.retain(|path| path != &staging);
                let _ = std::fs::remove_file(&staging);
                rollback_and_checkpoint(journal, state).await;
                return Err(format!("apply: could not persist copy publication intent: {error}"));
            }
            if let Err(error) = publish_staged(&staging, &destination, force) {
                journal.outputs.retain(|output| output.role != "pending_copy");
                journal.staging_paths.retain(|path| path != &staging);
                let _ = std::fs::remove_file(staging);
                rollback_and_checkpoint(journal, state).await;
                return Err(error);
            }
            injected_crash(crash, CrashPoint::TransferMutated)?;
            journal.outputs.retain(|output| output.role != "pending_copy");
            journal.staging_paths.retain(|path| path != &staging);
        }
        let record = FileRecord::from_path(item.destination.clone(), None).map_err(|error| format!("apply: could not journal {}: {error}", item.destination.display()))?;
        journal.outputs.push(JournalOutput { role: if moving { "moved" } else { "copy" }.to_string(), record, original_path: moving.then(|| item.source.path.clone()) });
        journal.remaining_steps.retain(|step| step.destination != item.destination);
        state.put_journal(journal.clone()).await?;
        injected_crash(crash, CrashPoint::TransferCheckpoint)?;
        if index + 1 < plan.items.len() { injected_crash(crash, CrashPoint::BetweenTransferRecords)?; }
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
        let mut recovery = self.clone();
        let mut unpublished = std::collections::HashSet::new();
        for (index, output) in self.outputs.iter().enumerate() {
            if output.role.starts_with("pending_") && !output.record.path.exists() {
                if output.role == "pending_move" {
                    if let Some(original) = &output.original_path {
                        let mut expected = output.record.clone();
                        expected.path = original.clone();
                        validate_record(&expected, "recover: move source drift")?;
                        unpublished.insert(index);
                        continue;
                    }
                }
                let mut matched = false;
                for staging in &self.staging_paths {
                    if staging.exists() {
                        let mut expected = output.record.clone();
                        expected.path = staging.clone();
                        if validate_record(&expected, "recover: staging drift").is_ok() {
                            matched = true;
                            unpublished.insert(index);
                            break;
                        }
                    }
                }
                if !matched { return Err(format!("recover: pending output is unaccounted for: {}", output.record.path.display())); }
                continue;
            }
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
        recovery.outputs = recovery.outputs.into_iter().enumerate().filter_map(|(index, output)| (!unpublished.contains(&index)).then_some(output)).collect();
        recovery.staging_paths.clear();
        let mut recovered = recovery.rollback().map_err(|error| error.replacen("rollback:", "recover:", 1))?;
        recovered.staging_paths.clear();
        Ok(recovered)
    }

    pub async fn recover_resume(&self, state: &crate::state::StateHandle) -> Result<Self, String> {
        if !self.needs_recovery() { return Err(format!("recover: operation {} has status '{}' and does not need recovery", self.id, self.status)); }
        if self.operation == "compress" { return self.recover_resume_archive(state).await; }
        if self.operation != "copy" { return Err(format!("recover: --resume currently supports copy and compress operations only, not {}", self.operation)); }
        if !self.resume_supported { return Err(format!("recover: operation {} predates persisted resume intent; use --rollback", self.id)); }
        if !self.undo_safe { return Err(format!("recover: operation {} replaced existing destinations and cannot be safely resumed", self.id)); }
        let mut resumed = self.clone();
        for output in resumed.outputs.iter().filter(|output| output.role == "copy") {
            validate_record(&output.record, "recover: completed copy drift")?;
        }
        let steps = resumed.remaining_steps.clone();
        for step in steps {
            if step.operation != "copy" { return Err(format!("recover: journal {} contains a non-copy remaining step", self.id)); }
            let pending_index = resumed.outputs.iter().position(|output| output.role == "pending_copy" && output.record.path == step.destination);
            if let Some(index) = pending_index {
                let pending = resumed.outputs[index].record.clone();
                let matching_staging = resumed.staging_paths.iter().find(|path| {
                    if !path.exists() { return false; }
                    let mut expected = pending.clone(); expected.path = (*path).clone();
                    validate_record(&expected, "recover: staged copy drift").is_ok()
                }).cloned();
                if matching_staging.is_none() && resumed.staging_paths.iter().any(|path| path.exists()) {
                    return Err(format!("recover: staged copy identity changed for {}", step.destination.display()));
                }
                if step.destination.exists() && matching_staging.is_some() {
                    return Err(format!("recover: both staged and published copies exist for {}", step.destination.display()));
                }
                if step.destination.exists() {
                    validate_record(&pending, "recover: published copy drift")?;
                    resumed.staging_paths.retain(|path| path.exists());
                } else if let Some(staging) = matching_staging {
                    publish_staged(&staging, &step.destination, false)?;
                    resumed.staging_paths.retain(|path| path != &staging);
                } else {
                    return Err(format!("recover: neither staged nor published copy exists for {}", step.destination.display()));
                }
                resumed.outputs.remove(index);
            } else {
                if step.destination.exists() { return Err(format!("recover: unaccounted destination exists: {}", step.destination.display())); }
                validate_record(&step.source, "recover: copy source drift")?;
                let staging = staging_path(&step.destination, &resumed.id, "copy")?;
                if !resumed.staging_paths.contains(&staging) { resumed.staging_paths.push(staging.clone()); }
                state.put_journal(resumed.clone()).await?;
                crate::copy::copy_one(&step.source.path, &staging, false).map_err(|error| format!("recover: {error}"))?;
                let mut pending = FileRecord::from_path(staging.clone(), None).map_err(|error| format!("recover: could not identify staged copy: {error}"))?;
                pending.path = step.destination.clone();
                resumed.outputs.push(JournalOutput { role: "pending_copy".to_string(), record: pending, original_path: None });
                state.put_journal(resumed.clone()).await?;
                publish_staged(&staging, &step.destination, false)?;
                resumed.outputs.retain(|output| !(output.role == "pending_copy" && output.record.path == step.destination));
                resumed.staging_paths.retain(|path| path != &staging);
            }
            let record = FileRecord::from_path(step.destination.clone(), None).map_err(|error| format!("recover: could not checkpoint resumed copy: {error}"))?;
            resumed.outputs.push(JournalOutput { role: "copy".to_string(), record, original_path: None });
            resumed.remaining_steps.retain(|remaining| remaining.destination != step.destination);
            state.put_journal(resumed.clone()).await?;
        }
        if !resumed.remaining_steps.is_empty() { return Err(format!("recover: operation {} still has unsupported remaining steps", self.id)); }
        if resumed.staging_paths.iter().any(|path| path.exists()) { return Err(format!("recover: operation {} still has staged files", self.id)); }
        resumed.staging_paths.clear();
        resumed.status = "applied".to_string();
        resumed.error = None;
        resumed.finished_at = now_seconds();
        state.put_journal(resumed.clone()).await?;
        Ok(resumed)
    }

    async fn recover_resume_archive(&self, state: &crate::state::StateHandle) -> Result<Self, String> {
        if !self.resume_supported { return Err(format!("recover: operation {} predates persisted archive intent; use --rollback", self.id)); }
        if !self.undo_safe { return Err(format!("recover: operation {} replaced existing destinations and cannot be safely resumed", self.id)); }
        let mut resumed = self.clone();
        for output in resumed.outputs.iter().filter(|output| matches!(output.role.as_str(), "archive" | "backup")) {
            validate_record(&output.record, "recover: completed archive output drift")?;
        }
        for step in resumed.remaining_archives.clone() {
            let archive_done = resumed.outputs.iter().any(|output| output.role == "archive" && output.record.path == step.archive);
            if !archive_done {
                if !resume_pending_publication(&mut resumed, "pending_archive", &step.archive)? {
                    for entry in &step.entries { validate_record(&entry.source, "recover: archive source drift")?; }
                    let staging = staging_path(&step.archive, &resumed.id, "archive")?;
                    if !resumed.staging_paths.contains(&staging) { resumed.staging_paths.push(staging.clone()); }
                    state.put_journal(resumed.clone()).await?;
                    let item = crate::compress::ArchivePlanItem { root: step.root.clone(), archive: step.archive.clone(), backup: step.backup.clone(), entries: step.entries.iter().map(|entry| crate::compress::ArchivePlanEntry { archive_name: entry.archive_name.clone(), source: entry.source.clone() }).collect() };
                    crate::compress::build_planned_archive(&item, &staging).await?;
                    let mut pending = FileRecord::from_path(staging.clone(), None).map_err(|error| format!("recover: could not identify staged archive: {error}"))?;
                    pending.path = step.archive.clone();
                    resumed.outputs.push(JournalOutput { role: "pending_archive".to_string(), record: pending, original_path: None });
                    state.put_journal(resumed.clone()).await?;
                    publish_staged(&staging, &step.archive, false)?;
                    resumed.outputs.retain(|output| !(output.role == "pending_archive" && output.record.path == step.archive));
                    resumed.staging_paths.retain(|path| path != &staging);
                }
                let record = FileRecord::from_path(step.archive.clone(), None).map_err(|error| format!("recover: could not checkpoint archive: {error}"))?;
                resumed.outputs.push(JournalOutput { role: "archive".to_string(), record, original_path: None });
                state.put_journal(resumed.clone()).await?;
            }

            if let Some(backup) = &step.backup {
                let backup_done = resumed.outputs.iter().any(|output| output.role == "backup" && output.record.path == *backup);
                if !backup_done {
                    if !resume_pending_publication(&mut resumed, "pending_backup", backup)? {
                        let staging = staging_path(backup, &resumed.id, "backup")?;
                        if !resumed.staging_paths.contains(&staging) { resumed.staging_paths.push(staging.clone()); }
                        state.put_journal(resumed.clone()).await?;
                        std::fs::copy(&step.archive, &staging).map_err(|error| format!("recover: could not stage archive backup: {error}"))?;
                        let mut pending = FileRecord::from_path(staging.clone(), None).map_err(|error| format!("recover: could not identify staged backup: {error}"))?;
                        pending.path = backup.clone();
                        resumed.outputs.push(JournalOutput { role: "pending_backup".to_string(), record: pending, original_path: None });
                        state.put_journal(resumed.clone()).await?;
                        publish_staged(&staging, backup, false)?;
                        resumed.outputs.retain(|output| !(output.role == "pending_backup" && output.record.path == *backup));
                        resumed.staging_paths.retain(|path| path != &staging);
                    }
                    let record = FileRecord::from_path(backup.clone(), None).map_err(|error| format!("recover: could not checkpoint archive backup: {error}"))?;
                    resumed.outputs.push(JournalOutput { role: "backup".to_string(), record, original_path: None });
                    state.put_journal(resumed.clone()).await?;
                }
            }
            resumed.remaining_archives.retain(|remaining| remaining.archive != step.archive);
            state.put_journal(resumed.clone()).await?;
        }
        if !resumed.remaining_archives.is_empty() { return Err(format!("recover: operation {} still has unsupported archive steps", self.id)); }
        if resumed.staging_paths.iter().any(|path| path.exists()) { return Err(format!("recover: operation {} still has staged files", self.id)); }
        resumed.staging_paths.clear();
        resumed.status = "applied".to_string(); resumed.error = None; resumed.finished_at = now_seconds();
        state.put_journal(resumed.clone()).await?;
        Ok(resumed)
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
        let remaining_steps = self.remaining_steps.iter().map(|step| serde_json::json!({
            "operation": step.operation, "destination": step.destination.to_string_lossy(),
            "source": { "path": step.source.path.to_string_lossy(), "volume_id": step.source.identity.volume_id,
                "file_id": step.source.identity.file_id.map(hex_file_id), "kind": kind_name(&step.source.kind),
                "size": step.source.size, "modified": step.source.modified }
        })).collect::<Vec<_>>();
        let remaining_archives = self.remaining_archives.iter().map(|step| serde_json::json!({
            "root": step.root.to_string_lossy(), "archive": step.archive.to_string_lossy(),
            "backup": step.backup.as_ref().map(|path| path.to_string_lossy()),
            "entries": step.entries.iter().map(|entry| serde_json::json!({"archive_name":entry.archive_name,
                "source":{"path":entry.source.path.to_string_lossy(),"volume_id":entry.source.identity.volume_id,
                "file_id":entry.source.identity.file_id.map(hex_file_id),"kind":kind_name(&entry.source.kind),
                "size":entry.source.size,"modified":entry.source.modified}})).collect::<Vec<_>>()
        })).collect::<Vec<_>>();
        serde_json::json!({"version":5,"id":self.id,"plan_id":self.plan_id,"operation":self.operation,"started_at":self.started_at,"finished_at":self.finished_at,"undo_safe":self.undo_safe,"resume_supported":self.resume_supported,"status":self.status,"error":self.error,"undone_at":self.undone_at,"staging_paths":self.staging_paths.iter().map(|path| path.to_string_lossy()).collect::<Vec<_>>(),"remaining_steps":remaining_steps,"remaining_archives":remaining_archives,"outputs":outputs}).to_string()
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
        let mut remaining_steps = Vec::new();
        for step in value.get("remaining_steps").and_then(|v| v.as_array()).into_iter().flatten() {
            let source = step.get("source").ok_or_else(|| "journal: recovery step missing source".to_string())?;
            remaining_steps.push(RecoveryStep {
                operation: step.get("operation").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                destination: PathBuf::from(step.get("destination").and_then(|v| v.as_str()).ok_or_else(|| "journal: recovery step missing destination".to_string())?),
                source: FileRecord { path: PathBuf::from(source.get("path").and_then(|v| v.as_str()).ok_or_else(|| "journal: recovery source missing path".to_string())?),
                    identity: FileIdentity { volume_id: source.get("volume_id").and_then(|v| v.as_u64()), file_id: source.get("file_id").and_then(|v| v.as_str()).map(parse_file_id).transpose()? },
                    kind: parse_kind(source.get("kind").and_then(|v| v.as_str()).unwrap_or("other")), size: source.get("size").and_then(|v| v.as_u64()).unwrap_or(0),
                    modified: source.get("modified").and_then(|v| v.as_u64()), sha256: None }
            });
        }
        let mut remaining_archives = Vec::new();
        for step in value.get("remaining_archives").and_then(|v| v.as_array()).into_iter().flatten() {
            let mut entries = Vec::new();
            for entry in step.get("entries").and_then(|v| v.as_array()).into_iter().flatten() {
                let source = entry.get("source").ok_or_else(|| "journal: archive recovery entry missing source".to_string())?;
                entries.push(ArchiveRecoveryEntry { archive_name: entry.get("archive_name").and_then(|v| v.as_str()).unwrap_or("").to_string(), source: file_record_from_json(source, "archive recovery source")? });
            }
            remaining_archives.push(ArchiveRecoveryStep { root: PathBuf::from(step.get("root").and_then(|v| v.as_str()).unwrap_or("")), archive: PathBuf::from(step.get("archive").and_then(|v| v.as_str()).ok_or_else(|| "journal: archive recovery step missing destination".to_string())?), backup: step.get("backup").and_then(|v| v.as_str()).map(PathBuf::from), entries });
        }
        Ok(Self { id: required_string("id")?, plan_id: required_string("plan_id")?, operation: required_string("operation")?, started_at: required_number("started_at")?, finished_at: required_number("finished_at")?, outputs, staging_paths, remaining_steps, remaining_archives, resume_supported: value.get("resume_supported").and_then(|v| v.as_bool()).unwrap_or(false), undo_safe: value.get("undo_safe").and_then(|v| v.as_bool()).unwrap_or(false), status, error: value.get("error").and_then(|v| v.as_str()).map(str::to_string), undone_at })
    }
}

fn resume_pending_publication(journal: &mut OperationJournal, role: &str, destination: &std::path::Path) -> Result<bool, String> {
    let Some(index) = journal.outputs.iter().position(|output| output.role == role && output.record.path == destination) else { return Ok(false); };
    let pending = journal.outputs[index].record.clone();
    let matching_staging = journal.staging_paths.iter().find(|path| {
        if !path.exists() { return false; }
        let mut expected = pending.clone(); expected.path = (*path).clone();
        validate_record(&expected, "recover: staged archive drift").is_ok()
    }).cloned();
    if matching_staging.is_none() && journal.staging_paths.iter().any(|path| path.exists()) {
        return Err(format!("recover: staged output identity changed for {}", destination.display()));
    }
    if destination.exists() && matching_staging.is_some() { return Err(format!("recover: both staged and published outputs exist for {}", destination.display())); }
    if destination.exists() {
        validate_record(&pending, "recover: published archive drift")?;
        journal.staging_paths.retain(|path| path.exists());
    } else if let Some(staging) = matching_staging {
        publish_staged(&staging, destination, false)?;
        journal.staging_paths.retain(|path| path != &staging);
    } else {
        return Err(format!("recover: neither staged nor published output exists for {}", destination.display()));
    }
    journal.outputs.remove(index);
    Ok(true)
}

fn hex_file_id(bytes: [u8; 16]) -> String { bytes.iter().map(|byte| format!("{byte:02x}")).collect() }
fn kind_name(kind: &FileKind) -> &'static str { match kind { FileKind::File => "file", FileKind::Directory => "directory", FileKind::Symlink => "symlink", FileKind::Other => "other" } }
fn parse_kind(value: &str) -> FileKind { match value { "file" => FileKind::File, "directory" => FileKind::Directory, "symlink" => FileKind::Symlink, _ => FileKind::Other } }
fn file_record_from_json(value: &serde_json::Value, label: &str) -> Result<FileRecord, String> {
    Ok(FileRecord { path: PathBuf::from(value.get("path").and_then(|v| v.as_str()).ok_or_else(|| format!("journal: {label} missing path"))?),
        identity: FileIdentity { volume_id: value.get("volume_id").and_then(|v| v.as_u64()), file_id: value.get("file_id").and_then(|v| v.as_str()).map(parse_file_id).transpose()? },
        kind: parse_kind(value.get("kind").and_then(|v| v.as_str()).unwrap_or("other")), size: value.get("size").and_then(|v| v.as_u64()).unwrap_or(0),
        modified: value.get("modified").and_then(|v| v.as_u64()), sha256: None })
}
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
        let journal = OperationJournal { id: "operation-1".into(), plan_id: "plan-1".into(), operation: "compress".into(), started_at: 1, finished_at: 2, outputs: vec![JournalOutput { role: "archive".into(), record: FileRecord { path: "a.zip".into(), identity: FileIdentity { volume_id: Some(7), file_id: Some([3; 16]) }, kind: FileKind::File, size: 10, modified: Some(9), sha256: None }, original_path: None }], staging_paths: Vec::new(), remaining_steps: Vec::new(), remaining_archives: Vec::new(), resume_supported: false, undo_safe: true, status: "applied".into(), error: None, undone_at: None };
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
        let journal = OperationJournal { id: "operation-undo".into(), plan_id: "plan".into(), operation: "compress".into(), started_at: 1, finished_at: 2, outputs: vec![JournalOutput { role: "archive".into(), record, original_path: None }], staging_paths: Vec::new(), remaining_steps: Vec::new(), remaining_archives: Vec::new(), resume_supported: false, undo_safe: true, status: "applied".into(), error: None, undone_at: None };
        let undone = journal.undo().unwrap();
        assert!(!path.exists());
        assert!(undone.undone_at.is_some());
    }

    #[test]
    fn undo_validates_all_outputs_before_deleting_any() {
        let (first_path, first) = temp_output("atomic-a.zip", "first");
        let (second_path, second) = temp_output("atomic-b.zip", "second");
        let journal = OperationJournal { id: "operation-drift".into(), plan_id: "plan".into(), operation: "compress".into(), started_at: 1, finished_at: 2, outputs: vec![JournalOutput { role: "archive".into(), record: first, original_path: None }, JournalOutput { role: "backup".into(), record: second, original_path: None }], staging_paths: Vec::new(), remaining_steps: Vec::new(), remaining_archives: Vec::new(), resume_supported: false, undo_safe: true, status: "applied".into(), error: None, undone_at: None };
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
        let mut journal = OperationJournal { id: "operation-transaction".into(), plan_id: "plan".into(), operation: "copy".into(), started_at: 1, finished_at: 0, outputs: vec![JournalOutput { role: "copy".into(), record, original_path: None }], staging_paths: Vec::new(), remaining_steps: Vec::new(), remaining_archives: Vec::new(), resume_supported: false, undo_safe: true, status: "in_progress".into(), error: None, undone_at: None };
        transactional_rollback(&mut journal);
        assert_eq!(journal.status, "rolled_back");
        assert!(!path.exists());
    }

    #[test]
    fn recovery_rolls_back_a_crash_checkpoint_after_identity_validation() {
        let (path, record) = temp_output("recover-copy.tmp", "checkpointed copy");
        let journal = OperationJournal { id: "operation-recover".into(), plan_id: "plan".into(), operation: "copy".into(), started_at: 1, finished_at: 0, outputs: vec![JournalOutput { role: "copy".into(), record, original_path: None }], staging_paths: Vec::new(), remaining_steps: Vec::new(), remaining_archives: Vec::new(), resume_supported: false, undo_safe: true, status: "in_progress".into(), error: None, undone_at: None };
        assert!(journal.needs_recovery());
        let recovered = journal.recover_rollback().unwrap();
        assert_eq!(recovered.status, "rolled_back");
        assert!(!path.exists());
        assert!(!recovered.needs_recovery());
    }

    #[test]
    fn recovery_fails_closed_when_a_checkpointed_output_drifted() {
        let (path, record) = temp_output("recover-drift.tmp", "before");
        let journal = OperationJournal { id: "operation-recover-drift".into(), plan_id: "plan".into(), operation: "compress".into(), started_at: 1, finished_at: 0, outputs: vec![JournalOutput { role: "archive".into(), record, original_path: None }], staging_paths: Vec::new(), remaining_steps: Vec::new(), remaining_archives: Vec::new(), resume_supported: false, undo_safe: true, status: "partially_applied".into(), error: None, undone_at: None };
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
        let journal = OperationJournal { id: "operation-stage".into(), plan_id: "plan".into(), operation: "compress".into(), started_at: 1, finished_at: 0, outputs: Vec::new(), staging_paths: vec![staged.clone()], remaining_steps: Vec::new(), remaining_archives: Vec::new(), resume_supported: false, undo_safe: true, status: "in_progress".into(), error: None, undone_at: None };

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

    #[tokio::test]
    async fn every_archive_crash_boundary_recovers_from_persisted_state_without_debris() {
        let points = [
            CrashPoint::InitialJournal,
            CrashPoint::StagingIntent,
            CrashPoint::ZipStaged,
            CrashPoint::Published,
            CrashPoint::OutputCheckpoint,
            CrashPoint::ArchiveBeforeBackup,
            CrashPoint::BeforeApplied,
        ];
        for point in points {
            let name = format!("crash-{point:?}");
            let (root, source, fileset) = transfer_fixture(&name);
            let fileset = fileset.with_roots(vec![source.parent().unwrap().to_path_buf()]);
            let archives = root.join("archives");
            let backups = root.join("backups");
            let archive_plan = crate::compress::plan_fileset_per_root(
                &fileset,
                &archives.to_string_lossy(),
                Some(&backups.to_string_lossy()),
            ).unwrap();
            let final_paths = vec![archive_plan.items[0].archive.clone(), archive_plan.items[0].backup.clone().unwrap()];
            let plan = OperationPlan::archive(archive_plan, false);
            let database = root.join("recovery-state.redb");
            let state = crate::state::spawn(database.clone()).unwrap();

            let error = plan.apply_internal(&state, Some(point)).await.unwrap_err();
            assert!(error.starts_with("__ion_test_crash__:"), "{point:?}: {error}");
            let persisted = state.list_journals().await.unwrap();
            assert_eq!(persisted.len(), 1, "{point:?}");
            assert_eq!(persisted[0].status, "in_progress", "{point:?}");
            let operation_id = persisted[0].id.clone();
            drop(state);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;

            let restarted = crate::state::spawn(database).unwrap();
            let journal = restarted.get_journal(operation_id).await.unwrap().unwrap();
            let recovered = journal.recover_rollback().unwrap_or_else(|error| panic!("{point:?}: {error}"));
            restarted.put_journal(recovered.clone()).await.unwrap();

            assert_eq!(recovered.status, "rolled_back", "{point:?}");
            assert!(recovered.staging_paths.is_empty(), "{point:?}");
            assert!(final_paths.iter().all(|path| !path.exists()), "{point:?}: a final output survived recovery");
            assert!(source.exists(), "{point:?}: recovery removed a source");
            for directory in [&archives, &backups] {
                if directory.exists() {
                    let unexplained = std::fs::read_dir(directory).unwrap().filter_map(Result::ok).collect::<Vec<_>>();
                    assert!(unexplained.is_empty(), "{point:?}: unexplained files remain in {}", directory.display());
                }
            }
            drop(restarted);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[tokio::test]
    async fn every_copy_and_move_crash_boundary_recovers_without_unexplained_files() {
        let points = [
            CrashPoint::InitialJournal,
            CrashPoint::BeforeTransferMutation,
            CrashPoint::TransferMutated,
            CrashPoint::TransferCheckpoint,
            CrashPoint::BetweenTransferRecords,
            CrashPoint::BeforeApplied,
        ];
        for moving in [false, true] {
            for point in points {
                let name = format!("{}-{point:?}", if moving { "move" } else { "copy" });
                let (root, first, mut fileset) = transfer_fixture(&name);
                let second = first.parent().unwrap().join("second.txt");
                std::fs::write(&second, "second typed transfer").unwrap();
                fileset.files.push(FileRecord::from_path(second.clone(), None).unwrap());
                let destination_root = root.join("destination");
                let plan = if moving {
                    OperationPlan::move_files(&fileset, &destination_root.to_string_lossy(), false).unwrap()
                } else {
                    OperationPlan::copy(&fileset, &destination_root.to_string_lossy(), false).unwrap()
                };
                let destinations = match &plan.operation {
                    PlannedOperation::Copy(value) | PlannedOperation::Move(value) => value.items.iter().map(|item| item.destination.clone()).collect::<Vec<_>>(),
                    _ => unreachable!(),
                };
                let database = root.join("recovery-state.redb");
                let state = crate::state::spawn(database.clone()).unwrap();

                let error = plan.apply_internal(&state, Some(point)).await.unwrap_err();
                assert!(error.starts_with("__ion_test_crash__:"), "{moving}/{point:?}: {error}");
                let journal = state.list_journals().await.unwrap().pop().unwrap();
                let operation_id = journal.id.clone();
                assert_eq!(journal.status, "in_progress", "{moving}/{point:?}");
                drop(state);
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;

                let restarted = crate::state::spawn(database).unwrap();
                let journal = restarted.get_journal(operation_id).await.unwrap().unwrap();
                let recovered = journal.recover_rollback().unwrap_or_else(|error| panic!("{moving}/{point:?}: {error}"));
                restarted.put_journal(recovered.clone()).await.unwrap();

                assert_eq!(recovered.status, "rolled_back", "{moving}/{point:?}");
                assert!(recovered.staging_paths.is_empty(), "{moving}/{point:?}");
                assert!(first.exists() && second.exists(), "{moving}/{point:?}: a source was not restored");
                assert!(destinations.iter().all(|path| !path.exists()), "{moving}/{point:?}: a destination survived recovery");
                let mut unexplained = Vec::new();
                let mut pending_dirs = vec![destination_root.clone()];
                while let Some(directory) = pending_dirs.pop() {
                    if !directory.exists() { continue; }
                    for entry in std::fs::read_dir(directory).unwrap().filter_map(Result::ok) {
                        if entry.path().is_dir() { pending_dirs.push(entry.path()); } else { unexplained.push(entry.path()); }
                    }
                }
                assert!(unexplained.is_empty(), "{moving}/{point:?}: unexplained destination files: {unexplained:?}");
                drop(restarted);
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                let _ = std::fs::remove_dir_all(root);
            }
        }
    }

    #[tokio::test]
    async fn copy_resume_completes_every_persisted_crash_state_idempotently() {
        let points = [
            CrashPoint::InitialJournal,
            CrashPoint::BeforeTransferMutation,
            CrashPoint::TransferMutated,
            CrashPoint::TransferCheckpoint,
            CrashPoint::BetweenTransferRecords,
            CrashPoint::BeforeApplied,
        ];
        for point in points {
            let (root, first, mut fileset) = transfer_fixture(&format!("resume-{point:?}"));
            let second = first.parent().unwrap().join("second.txt");
            std::fs::write(&second, "second resumable copy").unwrap();
            fileset.files.push(FileRecord::from_path(second.clone(), None).unwrap());
            let destination_root = root.join("destination");
            let plan = OperationPlan::copy(&fileset, &destination_root.to_string_lossy(), false).unwrap();
            let destinations = match &plan.operation { PlannedOperation::Copy(value) => value.items.iter().map(|item| item.destination.clone()).collect::<Vec<_>>(), _ => unreachable!() };
            let database = root.join("resume-state.redb");
            let state = crate::state::spawn(database.clone()).unwrap();
            let error = plan.apply_internal(&state, Some(point)).await.unwrap_err();
            assert!(error.starts_with("__ion_test_crash__:"), "{point:?}: {error}");
            let operation_id = state.list_journals().await.unwrap().pop().unwrap().id;
            drop(state);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;

            let restarted = crate::state::spawn(database).unwrap();
            let journal = restarted.get_journal(operation_id).await.unwrap().unwrap();
            let resumed = journal.recover_resume(&restarted).await.unwrap_or_else(|error| panic!("{point:?}: {error}"));

            assert_eq!(resumed.status, "applied", "{point:?}");
            assert!(resumed.remaining_steps.is_empty(), "{point:?}");
            assert!(resumed.staging_paths.is_empty(), "{point:?}");
            assert_eq!(resumed.outputs.iter().filter(|output| output.role == "copy").count(), 2, "{point:?}");
            assert!(first.exists() && second.exists(), "{point:?}");
            assert_eq!(std::fs::read_to_string(&destinations[0]).unwrap(), "typed transfer", "{point:?}");
            assert_eq!(std::fs::read_to_string(&destinations[1]).unwrap(), "second resumable copy", "{point:?}");
            drop(restarted);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[tokio::test]
    async fn copy_resume_refuses_source_drift_before_rebuilding() {
        let (root, source, fileset) = transfer_fixture("resume-source-drift");
        let destination_root = root.join("destination");
        let plan = OperationPlan::copy(&fileset, &destination_root.to_string_lossy(), false).unwrap();
        let state = crate::state::spawn_memory();
        plan.apply_internal(&state, Some(CrashPoint::InitialJournal)).await.unwrap_err();
        let journal = state.list_journals().await.unwrap().pop().unwrap();
        std::fs::write(&source, "source changed after crash and has another size").unwrap();

        let error = journal.recover_resume(&state).await.unwrap_err();

        assert!(error.contains("source drift"), "{error}");
        assert!(!destination_root.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn archive_resume_completes_every_persisted_crash_state() {
        let points = [CrashPoint::InitialJournal, CrashPoint::StagingIntent, CrashPoint::ZipStaged, CrashPoint::Published, CrashPoint::OutputCheckpoint, CrashPoint::ArchiveBeforeBackup, CrashPoint::BeforeApplied];
        for point in points {
            let (root, source, fileset) = transfer_fixture(&format!("archive-resume-{point:?}"));
            let fileset = fileset.with_roots(vec![source.parent().unwrap().to_path_buf()]);
            let archives = root.join("archives");
            let backups = root.join("backups");
            let archive_plan = crate::compress::plan_fileset_per_root(&fileset, &archives.to_string_lossy(), Some(&backups.to_string_lossy())).unwrap();
            let archive = archive_plan.items[0].archive.clone();
            let backup = archive_plan.items[0].backup.clone().unwrap();
            let plan = OperationPlan::archive(archive_plan, false);
            let database = root.join("archive-resume.redb");
            let state = crate::state::spawn(database.clone()).unwrap();
            let error = plan.apply_internal(&state, Some(point)).await.unwrap_err();
            assert!(error.starts_with("__ion_test_crash__:"), "{point:?}: {error}");
            let operation_id = state.list_journals().await.unwrap().pop().unwrap().id;
            drop(state);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;

            let restarted = crate::state::spawn(database).unwrap();
            let journal = restarted.get_journal(operation_id).await.unwrap().unwrap();
            let resumed = journal.recover_resume(&restarted).await.unwrap_or_else(|error| panic!("{point:?}: {error}"));

            assert_eq!(resumed.status, "applied", "{point:?}");
            assert!(resumed.remaining_archives.is_empty(), "{point:?}");
            assert!(resumed.staging_paths.is_empty(), "{point:?}");
            assert_eq!(resumed.outputs.iter().filter(|output| matches!(output.role.as_str(), "archive" | "backup")).count(), 2, "{point:?}");
            assert_eq!(std::fs::read(&archive).unwrap(), std::fs::read(&backup).unwrap(), "{point:?}");
            let file = std::fs::File::open(&archive).unwrap();
            let mut zip = zip::ZipArchive::new(file).unwrap();
            let mut contents = String::new();
            use std::io::Read as _;
            zip.by_index(0).unwrap().read_to_string(&mut contents).unwrap();
            assert_eq!(contents, "typed transfer", "{point:?}");
            drop(restarted);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[tokio::test]
    async fn archive_resume_refuses_source_drift_before_rebuild() {
        let (root, source, fileset) = transfer_fixture("archive-resume-drift");
        let fileset = fileset.with_roots(vec![source.parent().unwrap().to_path_buf()]);
        let archive_plan = crate::compress::plan_fileset_per_root(&fileset, &root.join("archives").to_string_lossy(), None).unwrap();
        let plan = OperationPlan::archive(archive_plan, false);
        let state = crate::state::spawn_memory();
        plan.apply_internal(&state, Some(CrashPoint::InitialJournal)).await.unwrap_err();
        let journal = state.list_journals().await.unwrap().pop().unwrap();
        std::fs::write(&source, "archive source drifted to a different size").unwrap();

        let error = journal.recover_resume(&state).await.unwrap_err();

        assert!(error.contains("source drift"), "{error}");
        assert!(!root.join("archives").join("source.zip").exists());
        let _ = std::fs::remove_dir_all(root);
    }
}
