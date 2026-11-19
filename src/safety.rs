use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        },
    },
    path::Path,
};

const MAX_PRIVATE_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    dev: libc::dev_t,
    ino: libc::ino_t,
    mode: libc::mode_t,
    nlink: libc::nlink_t,
    size: libc::off_t,
    mtime_sec: libc::time_t,
    mtime_nsec: libc::c_long,
    ctime_sec: libc::time_t,
    ctime_nsec: libc::c_long,
}

fn identity(metadata: &fs::Metadata) -> FileIdentity {
    FileIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
        mode: metadata.mode(),
        nlink: metadata.nlink(),
        size: metadata.size() as libc::off_t,
        mtime_sec: metadata.mtime(),
        mtime_nsec: metadata.mtime_nsec(),
        ctime_sec: metadata.ctime(),
        ctime_nsec: metadata.ctime_nsec(),
    }
}

fn name_bytes(name: &std::ffi::OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| io::Error::other("path contains a null byte"))
}

fn entry_identity(parent: &File, name: &std::ffi::OsStr) -> io::Result<FileIdentity> {
    let name = name_bytes(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    Ok(FileIdentity {
        dev: stat.st_dev,
        ino: stat.st_ino,
        mode: stat.st_mode,
        nlink: stat.st_nlink,
        size: stat.st_size,
        mtime_sec: stat.st_mtime,
        mtime_nsec: stat.st_mtime_nsec,
        ctime_sec: stat.st_ctime,
        ctime_nsec: stat.st_ctime_nsec,
    })
}

pub(crate) fn entry_is_directory(parent: &File, name: &std::ffi::OsStr) -> io::Result<bool> {
    Ok(entry_identity(parent, name)?.mode & libc::S_IFMT == libc::S_IFDIR)
}

pub(crate) fn rebind_entry(parent: &File, file: &File, name: &std::ffi::OsStr) -> io::Result<()> {
    if identity(&file.metadata()?) != entry_identity(parent, name)? {
        return Err(io::Error::other("named entry changed while held open"));
    }
    Ok(())
}

pub(crate) fn open_at(
    parent: &File,
    name: &std::ffi::OsStr,
    flags: libc::c_int,
) -> io::Result<File> {
    let name = name_bytes(name)?;
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(test)]
type TestHook = Box<dyn FnOnce() + Send>;
#[cfg(test)]
static READ_PRIVATE_AFTER_METADATA_HOOK: std::sync::OnceLock<std::sync::Mutex<Option<TestHook>>> =
    std::sync::OnceLock::new();
#[cfg(test)]
fn set_read_private_after_metadata_hook(hook: Option<Box<dyn FnOnce() + Send>>) {
    *READ_PRIVATE_AFTER_METADATA_HOOK
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap() = hook;
}
#[cfg(test)]
fn after_read_private_metadata() {
    if let Some(hook) = READ_PRIVATE_AFTER_METADATA_HOOK
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap()
        .take()
    {
        hook();
    }
}
#[cfg(not(test))]
fn after_read_private_metadata() {}
pub fn directory(path: &Path) -> io::Result<File> {
    // Verify every existing component; do not traverse a supplied symlink.
    let mut current = std::path::PathBuf::new();
    for component in path.components() {
        current.push(component);
        let meta = fs::symlink_metadata(&current)?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err(io::Error::other(
                "directory path contains a link or non-directory",
            ));
        }
    }
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)
}
pub struct HomeLock {
    _file: File,
}
impl HomeLock {
    pub fn acquire(paths: &crate::settings::Paths) -> io::Result<Self> {
        directory(&paths.home)?;
        directory(&paths.state)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(paths.owner_lock())?;
        let meta = file.metadata()?;
        if !meta.is_file()
            || meta.nlink() != 1
            || meta.uid() != unsafe { libc::geteuid() }
            || meta.mode() & 0o077 != 0
        {
            return Err(io::Error::other(
                "home lock must be one private owned regular file",
            ));
        }
        if unsafe {
            libc::flock(
                std::os::fd::AsRawFd::as_raw_fd(&file),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        } != 0
        {
            return Err(io::Error::other("home is active; stop the executor first"));
        }
        Ok(Self { _file: file })
    }
}
pub fn private_mkdir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    // Walk every component: an existing link or non-directory anywhere is refused, and
    // each missing level is created with mode 0700 (never the umask default), so an
    // intermediate directory outside the home is not left group- or world-accessible.
    let mut current = std::path::PathBuf::new();
    for component in path.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(io::Error::other(
                    "directory path contains a link or non-directory",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::DirBuilder::new().mode(0o700).create(&current)?;
            }
            Err(error) => return Err(error),
        }
    }
    directory(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    File::open(path)?.sync_all()?;
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}
pub fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    File::open(
        path.parent()
            .ok_or_else(|| io::Error::other("file parent missing"))?,
    )?
    .sync_all()
}
pub fn read_private(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("file parent missing"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("file name missing"))?;
    let directory = directory(parent)?;
    let mut f = open_at(
        &directory,
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
    )?;
    let initial = f.metadata()?;
    if !initial.is_file()
        || initial.nlink() != 1
        || initial.uid() != unsafe { libc::geteuid() }
        || initial.mode() & 0o077 != 0
        || initial.len() > MAX_PRIVATE_BYTES
    {
        return Err(io::Error::other("expected bounded private owned file"));
    }
    let expected = identity(&initial);
    after_read_private_metadata();
    let mut bytes = Vec::with_capacity(initial.len() as usize + 1);
    (&mut f)
        .take(MAX_PRIVATE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PRIVATE_BYTES
        || identity(&f.metadata()?) != expected
        || entry_identity(&directory, name)? != expected
    {
        return Err(io::Error::other(
            "private file changed or exceeded its bounded read",
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn read_private_rejects_growth_after_initial_metadata() {
        let temp = tempfile::tempdir().unwrap();
        private_mkdir(temp.path()).unwrap();
        let path = temp.path().join("receipt");
        write_new(&path, &vec![b'a'; 4 * 1024 * 1024]).unwrap();
        set_read_private_after_metadata_hook(Some(Box::new({
            let path = path.clone();
            move || {
                OpenOptions::new()
                    .append(true)
                    .open(path)
                    .unwrap()
                    .write_all(b"overflow")
                    .unwrap();
            }
        })));
        let result = read_private(&path);
        set_read_private_after_metadata_hook(None);
        assert!(result.is_err());
    }
}
