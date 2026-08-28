use crate::compress::ArchivePlan;
use crate::fileset::{FileIdentity, FileKind, FileRecord};
use crate::table::Table;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static OPERATION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannedOperation { Archive(ArchivePlan) }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationPlan { pub id: String, pub operation: PlannedOperation, pub force: bool }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalOutput { pub role: String, pub record: FileRecord }

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
    pub undo_safe: bool,
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

    pub fn to_table(&self) -> Table {
        let mut table = match &self.operation { PlannedOperation::Archive(plan) => plan.to_table() };
        for row in &mut table.rows { row.insert(0, ("plan_id".to_string(), self.id.clone())); }
        table
    }

    pub async fn apply(&self) -> Result<OperationJournal, String> {
        let started_at = now_seconds();
        let outputs = match &self.operation {
            PlannedOperation::Archive(plan) => {
                let undo_safe = plan.items.iter().all(|item| !item.archive.exists() && item.backup.as_ref().is_none_or(|path| !path.exists()));
                crate::compress::apply_archive_plan(plan, self.force).await?;
                let mut outputs = Vec::new();
                for item in &plan.items {
                    outputs.push(journal_output("archive", item.archive.clone())?);
                    if let Some(backup) = &item.backup { outputs.push(journal_output("backup", backup.clone())?); }
                }
                (outputs, undo_safe)
            }
        };
        Ok(OperationJournal { id: unique_id("operation"), plan_id: self.id.clone(), operation: "compress".to_string(), started_at, finished_at: now_seconds(), outputs: outputs.0, undo_safe: outputs.1, undone_at: None })
    }
}

fn journal_output(role: &str, path: PathBuf) -> Result<JournalOutput, String> {
    let record = FileRecord::from_path(path.clone(), None).map_err(|error| format!("apply: could not journal {}: {error}", path.display()))?;
    Ok(JournalOutput { role: role.to_string(), record })
}

impl OperationJournal {
    pub fn to_table(&self) -> Table {
        Table { rows: self.outputs.iter().map(|output| vec![
            ("operation_id".to_string(), self.id.clone()),
            ("plan_id".to_string(), self.plan_id.clone()),
            ("operation".to_string(), self.operation.clone()),
            ("status".to_string(), if self.undone_at.is_some() { "undone" } else { "applied" }.to_string()),
            ("role".to_string(), output.role.clone()),
            ("path".to_string(), output.record.path.to_string_lossy().into_owned()),
            ("size".to_string(), output.record.size.to_string()),
            ("undo_safe".to_string(), self.undo_safe.to_string()),
            ("started_at".to_string(), self.started_at.to_string()),
            ("finished_at".to_string(), self.finished_at.to_string()),
        ]).collect() }
    }

    /// Validates every output before deleting the first one. A modified,
    /// replaced, or missing output makes the entire undo fail closed.
    pub fn undo(&self) -> Result<Self, String> {
        if self.undone_at.is_some() { return Err(format!("undo: operation {} was already undone", self.id)); }
        if !self.undo_safe { return Err(format!("undo: operation {} replaced existing output and cannot be safely undone", self.id)); }
        for output in &self.outputs {
            let current = FileRecord::from_path(output.record.path.clone(), None).map_err(|error| format!("undo: output drift: {} is unavailable: {error}", output.record.path.display()))?;
            if current.identity != output.record.identity || current.kind != output.record.kind || current.size != output.record.size || current.modified != output.record.modified {
                return Err(format!("undo: output drift: {} changed after apply", output.record.path.display()));
            }
        }
        for output in &self.outputs { std::fs::remove_file(&output.record.path).map_err(|error| format!("undo: could not remove {}: {error}", output.record.path.display()))?; }
        let mut undone = self.clone();
        undone.undone_at = Some(now_seconds());
        Ok(undone)
    }

    pub fn to_json(&self) -> String {
        let outputs = self.outputs.iter().map(|output| serde_json::json!({
            "role": output.role, "path": output.record.path.to_string_lossy(),
            "volume_id": output.record.identity.volume_id,
            "file_id": output.record.identity.file_id.map(hex_file_id),
            "kind": kind_name(&output.record.kind), "size": output.record.size,
            "modified": output.record.modified
        })).collect::<Vec<_>>();
        serde_json::json!({"version":1,"id":self.id,"plan_id":self.plan_id,"operation":self.operation,"started_at":self.started_at,"finished_at":self.finished_at,"undo_safe":self.undo_safe,"undone_at":self.undone_at,"outputs":outputs}).to_string()
    }

    pub fn from_json(text: &str) -> Result<Self, String> {
        let value: serde_json::Value = serde_json::from_str(text).map_err(|error| format!("journal: invalid persisted JSON: {error}"))?;
        let required_string = |name: &str| value.get(name).and_then(|v| v.as_str()).map(str::to_string).ok_or_else(|| format!("journal: missing '{name}'"));
        let required_number = |name: &str| value.get(name).and_then(|v| v.as_u64()).ok_or_else(|| format!("journal: missing '{name}'"));
        let mut outputs = Vec::new();
        for output in value.get("outputs").and_then(|v| v.as_array()).ok_or_else(|| "journal: missing 'outputs'".to_string())? {
            let file_id = output.get("file_id").and_then(|v| v.as_str()).map(parse_file_id).transpose()?;
            outputs.push(JournalOutput { role: output.get("role").and_then(|v| v.as_str()).unwrap_or("").to_string(), record: FileRecord {
                path: PathBuf::from(output.get("path").and_then(|v| v.as_str()).ok_or_else(|| "journal: output missing path".to_string())?),
                identity: FileIdentity { volume_id: output.get("volume_id").and_then(|v| v.as_u64()), file_id },
                kind: parse_kind(output.get("kind").and_then(|v| v.as_str()).unwrap_or("other")),
                size: output.get("size").and_then(|v| v.as_u64()).unwrap_or(0), modified: output.get("modified").and_then(|v| v.as_u64()), sha256: None,
            } });
        }
        Ok(Self { id: required_string("id")?, plan_id: required_string("plan_id")?, operation: required_string("operation")?, started_at: required_number("started_at")?, finished_at: required_number("finished_at")?, outputs, undo_safe: value.get("undo_safe").and_then(|v| v.as_bool()).unwrap_or(false), undone_at: value.get("undone_at").and_then(|v| v.as_u64()) })
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
        let journal = OperationJournal { id: "operation-1".into(), plan_id: "plan-1".into(), operation: "compress".into(), started_at: 1, finished_at: 2, outputs: vec![JournalOutput { role: "archive".into(), record: FileRecord { path: "a.zip".into(), identity: FileIdentity { volume_id: Some(7), file_id: Some([3; 16]) }, kind: FileKind::File, size: 10, modified: Some(9), sha256: None } }], undo_safe: true, undone_at: None };
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
        let journal = OperationJournal { id: "operation-undo".into(), plan_id: "plan".into(), operation: "compress".into(), started_at: 1, finished_at: 2, outputs: vec![JournalOutput { role: "archive".into(), record }], undo_safe: true, undone_at: None };
        let undone = journal.undo().unwrap();
        assert!(!path.exists());
        assert!(undone.undone_at.is_some());
    }

    #[test]
    fn undo_validates_all_outputs_before_deleting_any() {
        let (first_path, first) = temp_output("atomic-a.zip", "first");
        let (second_path, second) = temp_output("atomic-b.zip", "second");
        let journal = OperationJournal { id: "operation-drift".into(), plan_id: "plan".into(), operation: "compress".into(), started_at: 1, finished_at: 2, outputs: vec![JournalOutput { role: "archive".into(), record: first }, JournalOutput { role: "backup".into(), record: second }], undo_safe: true, undone_at: None };
        std::fs::write(&second_path, "changed-size").unwrap();
        let error = journal.undo().unwrap_err();
        assert!(error.contains("output drift"), "{error}");
        assert!(first_path.exists(), "no output may be removed when validation fails");
        assert!(second_path.exists());
        let _ = std::fs::remove_file(first_path);
        let _ = std::fs::remove_file(second_path);
    }
}
