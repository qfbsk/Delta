use crate::wutil::wstr;
use fish_common::os::File;
use fish_widestring::wcs2zstring;
use std::{
    ffi::{CStr, OsStr},
    fs::{self, Metadata},
};

/// Struct for representing a file's inode. We use this to detect and avoid symlink loops, among
/// other things.
#[derive(Debug, Copy, Clone, Eq, PartialEq, PartialOrd, Hash)]
pub struct DevInode {
    pub device: u64,
    pub inode: u64,
}

/// While an inode / dev pair is sufficient to distinguish co-existing files, Linux
/// seems to aggressively reuse inodes, so it cannot determine if a file has been deleted
/// (ABA problem). Therefore we include richer information to detect file changes.
#[derive(Debug, Clone, Hash, PartialEq, PartialOrd)]
pub struct FileId {
    pub dev_inode: DevInode,
    pub size: u64,
    pub change_seconds: i64,
    pub change_nanoseconds: i64,
    pub mod_seconds: i64,
    pub mod_nanoseconds: i64,
}

fn system_time_parts(time: std::io::Result<std::time::SystemTime>) -> (i64, i64) {
    match time.and_then(|t| {
        t.duration_since(std::time::UNIX_EPOCH)
            .map_err(std::io::Error::other)
    }) {
        Ok(duration) => (duration.as_secs() as i64, duration.subsec_nanos() as i64),
        Err(_) => (0, 0),
    }
}

impl FileId {
    fn from_md_and_identity(buf: &Metadata, dev_inode: DevInode) -> Self {
        let (change_seconds, change_nanoseconds) = system_time_parts(buf.created());
        let (mod_seconds, mod_nanoseconds) = system_time_parts(buf.modified());
        FileId {
            dev_inode,
            size: buf.len(),
            change_seconds,
            change_nanoseconds,
            mod_seconds,
            mod_nanoseconds,
        }
    }

    /// Build a FileId from metadata alone.
    ///
    /// On Unix the metadata carries the device/inode pair. On Windows it does not, so without a
    /// handle or a path the identity is reported as zero and callers should prefer
    /// `file_id_for_file` / `file_id_for_path`.
    pub fn from_md(buf: &Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self::from_md_and_identity(
                buf,
                DevInode {
                    device: buf.dev(),
                    inode: buf.ino(),
                },
            )
        }
        #[cfg(windows)]
        {
            Self::from_md_and_identity(buf, DevInode { device: 0, inode: 0 })
        }
    }

    /// Return true if \param rhs has higher mtime seconds than this file_id_t.
    /// If identical, nanoseconds are compared.
    pub fn older_than(&self, rhs: &FileId) -> bool {
        let lhs = (self.mod_seconds, self.mod_nanoseconds);
        let rhs = (rhs.mod_seconds, rhs.mod_nanoseconds);
        lhs.cmp(&rhs).is_lt()
    }
}

pub const INVALID_FILE_ID: FileId = FileId {
    dev_inode: DevInode {
        device: u64::MAX,
        inode: u64::MAX,
    },
    size: u64::MAX,
    change_seconds: i64::MIN,
    change_nanoseconds: i64::MIN,
    mod_seconds: i64::MIN,
    mod_nanoseconds: i64::MIN,
};

#[cfg(unix)]
fn identity_of_file(file: &File) -> Option<DevInode> {
    use std::os::unix::fs::MetadataExt;
    let md = file.metadata().ok()?;
    Some(DevInode {
        device: md.dev(),
        inode: md.ino(),
    })
}

#[cfg(windows)]
fn identity_of_file(file: &File) -> Option<DevInode> {
    fish_common::os::file_identity_of(file)
        .ok()
        .map(|(device, inode)| DevInode { device, inode })
}

/// Get a FileId corresponding to a `file`, or `INVALID_FILE_ID` if it fails.
pub fn file_id_for_file(file: &File) -> FileId {
    let Ok(md) = file.metadata() else {
        return INVALID_FILE_ID;
    };
    let identity = identity_of_file(file).unwrap_or(DevInode {
        device: 0,
        inode: 0,
    });
    FileId::from_md_and_identity(&md, identity)
}

/// Get a FileId corresponding to a `path`, or `INVALID_FILE_ID` if it fails.
pub fn file_id_for_path(path: &wstr) -> FileId {
    file_id_for_path_narrow(&wcs2zstring(path))
}

pub fn file_id_for_path_narrow(path: &CStr) -> FileId {
    // fish's narrow paths are UTF-8; Windows widens them when it reaches the OS.
    let path = String::from_utf8_lossy(path.to_bytes());
    let path = OsStr::new(path.as_ref());
    let Ok(md) = fs::metadata(path) else {
        return INVALID_FILE_ID;
    };
    #[cfg(windows)]
    let identity = fish_common::os::file_identity(path)
        .map(|(device, inode)| DevInode { device, inode })
        .unwrap_or(DevInode {
            device: 0,
            inode: 0,
        });
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        DevInode {
            device: md.dev(),
            inode: md.ino(),
        }
    };
    FileId::from_md_and_identity(&md, identity)
}
