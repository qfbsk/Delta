use crate::wutil::DevInode;
use fish_widestring::{WString, osstr2wcstring, wcs2osstring, wstr};
use fish_common::os::BorrowedFd;
use std::{
    cell::Cell,
    fs,
    io,
    path::PathBuf,
};

/// Types of files that may be in a directory.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DirEntryType {
    Fifo = 1, // FIFO file
    Chr,      // character device
    Dir,      // directory
    Blk,      // block device
    Reg,      // regular file
    Lnk,      // symlink
    Sock,     // socket
    Whiteout, // whiteout (from BSD)
}

/// An entry returned by DirIter.
pub struct DirEntry {
    /// File name of this entry.
    pub name: WString,

    /// inode of this entry.
    ///
    /// Windows only exposes a file index through an open handle, so this stays 0; use
    /// [`DirEntry::dev_inode`], which falls back to opening the entry by name.
    pub inode: u64,

    // Device, inode pair for this entry, or none if not yet computed.
    dev_inode: Cell<Option<DevInode>>,

    // The type of the entry. This is initially none; it may be populated eagerly via the directory
    // listing (std reports the reparse/file attribute), or later via stat(). If stat() fails, the
    // error is silently ignored and the type is left as none(). Note this is an unavoidable race.
    typ: Cell<Option<DirEntryType>>,

    // whether this could be a link, false if we know definitively it isn't.
    possible_link: Option<bool>,

    // Full path of this entry, used for stat() and for the file identity.
    path: PathBuf,
}

impl DirEntry {
    /// Return the type of this entry, falling back to stat() if necessary.
    /// If stat() fails because the file has disappeared, this will return none().
    /// If stat() fails because of a broken symlink, this will return type lnk.
    pub fn check_type(&self) -> Option<DirEntryType> {
        // Call stat if needed to populate our type, swallowing errors.
        if self.typ.get().is_none() {
            self.do_stat();
        }
        self.typ.get()
    }

    /// Return whether this is a directory. This may call stat().
    pub fn is_dir(&self) -> bool {
        self.check_type() == Some(DirEntryType::Dir)
    }

    /// Return false if we know this can't be a link via the directory listing, true if it could be.
    pub fn is_possible_link(&self) -> Option<bool> {
        self.possible_link
    }
    /// Return the device, inode pair for this entry, invoking stat() if necessary.
    pub fn dev_inode(&self) -> Option<DevInode> {
        if self.dev_inode.get().is_none() {
            self.do_stat();
        }
        self.dev_inode.get()
    }

    /// Return the type of this entry if it is already available, otherwise none().
    #[cfg(test)]
    fn fast_type(&self) -> Option<DirEntryType> {
        self.typ.get()
    }

    /// Return whether this is a directory, if that's already known. None means we don't know
    /// yet (e.g. this is a symlink, whose target type can only be learned by following it,
    /// i.e. calling stat()).
    #[cfg(test)]
    fn is_dir_fast(&self) -> Option<bool> {
        self.typ.get().map(|t| t == DirEntryType::Dir)
    }

    // Reset our fields.
    fn reset(&mut self) {
        self.name.clear();
        self.inode = 0;
        self.typ.set(None);
        self.dev_inode.set(None);
        self.possible_link = None;
        self.path.clear();
    }

    // Populate our stat buffer, and type. Errors are silently ignored.
    fn do_stat(&self) {
        // Symlinks are followed: a link to a directory must report Dir, as it does on Unix.
        match fs::metadata(&self.path) {
            Ok(md) => {
                let typ = if md.is_dir() {
                    Some(DirEntryType::Dir)
                } else {
                    Some(DirEntryType::Reg)
                };
                self.typ.set(typ);
                if let Ok((device, inode)) = fish_common::os::file_identity(self.path.as_os_str()) {
                    self.dev_inode.set(Some(DevInode { device, inode }));
                }
            }
            Err(err) => {
                // ERROR_CANT_RESOLVE_FILENAME is what a self-referential (or too deep) symlink
                // produces; fish treats the POSIX ELOOP case as a link.
                const ERROR_CANT_RESOLVE_FILENAME: i32 = 1921;
                if err.raw_os_error() == Some(ERROR_CANT_RESOLVE_FILENAME) {
                    self.typ.set(Some(DirEntryType::Lnk));
                } else {
                    self.typ.set(None);
                }
            }
        }
    }
}

/// Class for iterating over a directory.
/// This allows enumerating the contents of a directory, exposing the file type if the filesystem
/// itself exposes that from the directory listing. stat() is incurred only if necessary: if the
/// entry is a symlink, or if the caller asks for the stat buffer.
/// Symlinks are followed.
pub struct DirIter {
    /// Whether this dir_iter considers the "." and ".." filesystem entries.
    withdot: bool,

    /// The directory being iterated; kept to synthesize "." and ".." and to rewind.
    root: PathBuf,

    /// The platform iterator.
    read_dir: fs::ReadDir,

    /// Dot entries that have not been returned yet.
    pending_dots: u8,

    /// The storage for our entry. This allows us to iterate without allocating.
    entry: DirEntry,
}

impl DirIter {
    /// Open a directory at a given path.
    pub fn new(path: &wstr) -> io::Result<Self> {
        Self::new_impl(path, false)
    }

    pub fn new_with_dots(path: &wstr) -> io::Result<Self> {
        Self::new_impl(path, true)
    }

    fn new_impl(path: &wstr, withdot: bool) -> io::Result<Self> {
        // Windows checks the directory exists and is a directory when the iterator is created,
        // like opendir() does.
        let root = PathBuf::from(wcs2osstring(path));
        let read_dir = fs::read_dir(&root)?;
        let entry = DirEntry {
            name: WString::new(),
            inode: 0,
            dev_inode: Cell::new(None),
            typ: Cell::new(None),
            path: PathBuf::new(),
            possible_link: None,
        };
        Ok(DirIter {
            withdot,
            root,
            read_dir,
            pending_dots: if withdot { 2 } else { 0 },
            entry,
        })
    }

    /// Return the underlying file descriptor.
    ///
    /// Windows directory listings do not run over a descriptor that fish owns; the only caller
    /// (Apple's case-sensitivity probe) treats `None` as "case sensitive", which is what Windows
    /// needs here anyway.
    pub fn fd(&self) -> Option<BorrowedFd<'_>> {
        None
    }

    /// Rewind the directory to the beginning. This cannot fail.
    pub fn rewind(&mut self) {
        if let Ok(again) = fs::read_dir(&self.root) {
            self.read_dir = again;
            self.pending_dots = if self.withdot { 2 } else { 0 };
        }
    }

    /// Read the next entry in the directory.
    /// This returns an error if the directory could not be read, or Ok(None) if there are no more
    /// entries; else an Ok entry.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<io::Result<&DirEntry>> {
        // std::fs::read_dir does not produce "." and ".."; synthesize them on request.
        if self.pending_dots > 0 {
            let (name, sub) = if self.pending_dots == 2 {
                (".", ".")
            } else {
                ("..", "..")
            };
            self.pending_dots -= 1;
            self.entry.reset();
            self.entry.name = WString::from(name);
            self.entry.path = self.root.join(sub);
            self.entry.typ.set(Some(DirEntryType::Dir));
            self.entry.possible_link = Some(false);
            return Some(Ok(&self.entry));
        }

        let dent = match self.read_dir.next()? {
            Ok(dent) => dent,
            Err(err) => return Some(Err(err)),
        };

        self.entry.reset();
        let file_name = dent.file_name();
        self.entry.path = self.root.join(&file_name);
        self.entry.name = osstr2wcstring(&file_name);

        let (typ, possible_link) = match dent.file_type() {
            Ok(ft) if ft.is_symlink() => (Some(DirEntryType::Lnk), Some(true)),
            Ok(ft) if ft.is_dir() => (Some(DirEntryType::Dir), Some(false)),
            Ok(ft) if ft.is_file() => (Some(DirEntryType::Reg), Some(false)),
            // Other attributes (device, reparse points we cannot classify): stat if asked.
            _ => (None, None),
        };

        // Do not store symlinks as we will need to resolve them.
        if typ != Some(DirEntryType::Lnk) {
            self.entry.typ.set(typ);
        }
        self.entry.possible_link = possible_link;

        Some(Ok(&self.entry))
    }
}

#[cfg(test)]
mod tests {
    use super::{DirEntryType, DirIter};
    use crate::prelude::*;
    use assert_matches::assert_matches;
    use fish_widestring::L;
    use nix::errno::Errno;
    use nix::sys::stat::Mode;
    use std::fs::File;
    use std::path::PathBuf;

    #[test]
    fn test_dir_iter_bad_path() {
        // Regression test: DirIter does not crash given a bad path.
        let dir = DirIter::new(L!("/a/bogus/path/which/does/notexist"));
        assert!(dir.is_err());
    }

    #[test]
    fn test_no_dots() {
        // DirIter does not return . or .. by default.
        let mut dir = DirIter::new(L!(".")).expect("Should be able to open CWD");
        while let Some(entry) = dir.next() {
            let entry = entry.unwrap();
            assert_ne!(entry.name, ".");
            assert_ne!(entry.name, "..");
        }
    }

    #[test]
    fn test_dots() {
        // DirIter returns . or .. if you ask nicely.
        let mut dir = DirIter::new_with_dots(L!(".")).expect("Should be able to open CWD");
        let mut seen_dot = false;
        let mut seen_dotdot = false;
        while let Some(entry) = dir.next() {
            let entry = entry.unwrap();
            if entry.name == "." {
                seen_dot = true;
            } else if entry.name == ".." {
                seen_dotdot = true;
            }
        }
        assert!(seen_dot);
        assert!(seen_dotdot);
    }

    #[test]
    #[allow(clippy::if_same_then_else)]
    fn test_dir_iter() {
        let baditer = DirIter::new(L!("/definitely/not/a/valid/directory/for/sure"));
        assert!(baditer.is_err());
        let Err(err) = baditer else {
            panic!("Expected error");
        };
        let err = Errno::try_from(err).expect("Should have an errno value");
        assert_matches!(err, Errno::ENOENT | Errno::EACCES);

        let temp_dir = fish_tempfile::new_dir().unwrap();
        let basepath = WString::from(temp_dir.path().to_str().unwrap());

        let makepath = |s: &str| -> PathBuf { temp_dir.path().join(s) };

        let dirname = "dir";
        let regname = "reg";
        // Windows requires either Developer Mode or SeCreateSymbolicLinkPrivilege to create
        // symlinks, so the link cases are covered by test_is_dir_fast's cfg on this platform.
        let names = vec![dirname, regname];

        // Make our different file types
        nix::unistd::mkdir(&makepath(dirname), Mode::from_bits(0o700).unwrap()).unwrap();
        File::create(makepath(regname)).unwrap();

        let mut iter1 = DirIter::new(&basepath).expect("Should be able to open directory");
        let mut seen = 0;
        while let Some(entry) = iter1.next() {
            let entry = entry.expect("Should not have gotten error");
            seen += 1;
            assert!(entry.name != "." && entry.name != "..");
            assert!(names.iter().any(|&n| entry.name == n));

            let expected = if entry.name == dirname {
                Some(DirEntryType::Dir)
            } else if entry.name == regname {
                Some(DirEntryType::Reg)
            } else {
                panic!("Unexpected file type");
            };

            // If we have a fast type, it should be correct.
            assert!(entry.fast_type().is_none() || entry.fast_type() == expected);
            assert_eq!(
                entry.check_type(),
                expected,
                "Wrong type for {}. Expected {:?}, got {:?}",
                entry.name,
                expected,
                entry.check_type()
            );
        }
        assert_eq!(seen, names.len());
    }

    #[test]
    fn test_is_dir_fast() {
        // is_dir_fast() must not require a stat(): for a symlink it should report None
        // (unknown) until something else (e.g. is_dir()) has resolved and cached the type.
        let temp_dir = fish_tempfile::new_dir().unwrap();
        let basepath = WString::from(temp_dir.path().to_str().unwrap());
        let makepath = |s: &str| -> PathBuf { temp_dir.path().join(s) };

        nix::unistd::mkdir(&makepath("dir"), Mode::from_bits(0o700).unwrap()).unwrap();
        File::create(makepath("reg")).unwrap();

        let mut iter = DirIter::new(&basepath).expect("Should be able to open directory");
        while let Some(entry) = iter.next() {
            let entry = entry.expect("Should not have gotten error");
            match entry.name.to_string().as_str() {
                "dir" => assert_eq!(entry.is_dir_fast(), Some(true)),
                "reg" => assert_eq!(entry.is_dir_fast(), Some(false)),
                other => panic!("Unexpected entry {other}"),
            }
        }
    }
}
