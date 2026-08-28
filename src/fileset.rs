use crate::table::{Row, Table};
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileIdentity {
    pub volume_id: Option<u64>,
    pub file_id: Option<[u8; 16]>,
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
    /// Logical directory roots that produced the records. Operations such
    /// as per-root compression use these native paths to preserve grouping.
    pub roots: Vec<PathBuf>,
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
        Self {
            files,
            provenance: FileSetProvenance {
                producer: producer.into(),
                roots: Vec::new(),
            },
            visible_columns,
        }
    }

    pub fn with_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.provenance.roots = roots;
        self
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
        let identity = identity(&path);
        Ok(Self { path, identity, kind, size: metadata.len(), modified, sha256 })
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
fn identity(path: &std::path::Path) -> FileIdentity {
    use std::fs::OpenOptions;
    use std::mem::{size_of, zeroed};
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandleEx, FileIdInfo, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE,
    };

    let file = match OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
    {
        Ok(file) => file,
        Err(_) => return FileIdentity { volume_id: None, file_id: None },
    };
    let mut info: FILE_ID_INFO = unsafe { zeroed() };
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as isize,
            FileIdInfo,
            &mut info as *mut FILE_ID_INFO as *mut std::ffi::c_void,
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok == 0 {
        FileIdentity { volume_id: None, file_id: None }
    } else {
        FileIdentity {
            volume_id: Some(info.VolumeSerialNumber),
            file_id: Some(info.FileId.Identifier),
        }
    }
}

#[cfg(not(windows))]
fn identity(_: &std::path::Path) -> FileIdentity {
    FileIdentity { volume_id: None, file_id: None }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn projection_does_not_discard_typed_identity() {
        let record = FileRecord { path: "a.txt".into(), identity: FileIdentity { volume_id: Some(1), file_id: Some([2; 16]) }, kind: FileKind::File, size: 3, modified: None, sha256: None };
        let files = FileSet::new(vec![record], "test").select(&["path".into()]);
        assert_eq!(files.files[0].identity.file_id, Some([2; 16]));
        assert_eq!(files.to_table().rows[0].len(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn windows_identity_survives_rename() {
        let root = std::env::temp_dir().join(format!("ion-win-file-id-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let before = root.join("before.txt");
        let after = root.join("after.txt");
        std::fs::write(&before, b"identity").unwrap();
        let first = FileRecord::from_path(before.clone(), None).unwrap().identity;
        std::fs::rename(&before, &after).unwrap();
        let second = FileRecord::from_path(after, None).unwrap().identity;
        assert!(first.volume_id.is_some(), "Windows volume identity should be available");
        assert!(first.file_id.is_some(), "Windows file identity should be available");
        assert_eq!(first, second);
        let _ = std::fs::remove_dir_all(root);
    }
}
