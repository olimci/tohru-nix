use std::{
    fs::{self, File},
    io::{self, Write},
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
};

pub fn link(target: &Path, path: &Path) -> io::Result<()> {
    let parent = path.parent().expect("managed path has a parent");
    let temporary = tempfile::tempdir_in(parent)?;
    let link = temporary.path().join("link");
    symlink(target, &link)?;
    fs::rename(link, path)?;
    sync(parent)
}

pub fn copy(source: &Path, path: &Path, mode: u32) -> io::Result<()> {
    let parent = path.parent().expect("managed path has a parent");
    let temporary = tempfile::NamedTempFile::new_in(parent)?;
    fs::copy(source, temporary.path())?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    sync(parent)
}

pub fn write(path: &Path, data: &[u8]) -> io::Result<()> {
    let parent = path.parent().expect("atomic path has a parent");
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(data)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    sync(parent)
}

pub fn remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => sync(path.parent().expect("removed path has a parent")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub fn sync(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}
