use crate::table::{Row, Table};
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileIdentity {
    pub volume_id: Option<u64>,
    pub file_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRecord {
    pub path: PathBuf,
    pub identity: FileIdentity,
    pub kind: FileKind,
    pub size: u64,
    pub modified: Option<u64>,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSetProvenance {
    pub producer: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSet {
    pub files: Vec<FileRecord>,
    pub provenance: FileSetProvenance,
    visible_columns: Vec<String>,
}

impl FileSet {
    pub fn new(files: Vec<FileRecord>, producer: impl Into<String>) -> Self {
        let mut visible_columns = vec!["path", "size", "modified", "is_dir"]
            .into_iter().map(str::to_string).collect::<Vec<_>>();
        if files.iter().any(|file| file.sha256.is_some()) {
            visible_columns.push("sha256".to_string());
        }
        Self { files, provenance: FileSetProvenance { producer: producer.into() }, visible_columns }
    }

    pub fn to_table(&self) -> Table {
        Table { rows: self.files.iter().map(|file| {
            let all = file.to_row();
            all.into_iter().filter(|(name, _)| self.visible_columns.contains(name)).collect()
        }).collect() }
    }

    pub fn select(mut self, columns: &[String]) -> Self {
        self.visible_columns = columns.to_vec();
        self
    }

    pub fn filter(mut self, column: &str, op: &str, value: &str) -> Self {
        self.files.retain(|file| {
            !Table { rows: vec![file.to_row()] }.filter(column, op, value).rows.is_empty()
        });
        self
    }

    pub fn len(&self) -> usize { self.files.len() }
}

impl FileRecord {
    pub fn from_path(path: PathBuf, sha256: Option<String>) -> Result<Self, String> {
        let metadata = std::fs::metadata(&path).map_err(|e| e.to_string())?;
        let file_type = metadata.file_type();
        let kind = if file_type.is_symlink() { FileKind::Symlink }
            else if metadata.is_dir() { FileKind::Directory }
            else if metadata.is_file() { FileKind::File }
            else { FileKind::Other };
        let modified = metadata.modified().ok().and_then(|time| time.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs());
        Ok(Self { path, identity: identity(&metadata), kind, size: metadata.len(), modified, sha256 })
    }

    pub fn to_row(&self) -> Row {
        let mut row = vec![
            ("path".to_string(), self.path.to_string_lossy().into_owned()),
            ("size".to_string(), self.size.to_string()),
            ("modified".to_string(), self.modified.map(|v| v.to_string()).unwrap_or_default()),
            ("is_dir".to_string(), matches!(self.kind, FileKind::Directory).to_string()),
        ];
        if let Some(hash) = &self.sha256 { row.push(("sha256".to_string(), hash.clone())); }
        row
    }
}

#[cfg(windows)]
fn identity(_: &std::fs::Metadata) -> FileIdentity {
    // Filled by the handle-based Windows identity enrichment planned for
    // the next slice. Keeping identity typed and optional avoids inventing
    // an unstable path-derived identifier now.
    FileIdentity { volume_id: None, file_id: None }
}

#[cfg(not(windows))]
fn identity(_: &std::fs::Metadata) -> FileIdentity {
    FileIdentity { volume_id: None, file_id: None }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn projection_does_not_discard_typed_identity() {
        let record = FileRecord { path: "a.txt".into(), identity: FileIdentity { volume_id: Some(1), file_id: Some(2) }, kind: FileKind::File, size: 3, modified: None, sha256: None };
        let files = FileSet::new(vec![record], "test").select(&["path".into()]);
        assert_eq!(files.files[0].identity.file_id, Some(2));
        assert_eq!(files.to_table().rows[0].len(), 1);
    }
}
