use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs, io,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    atomic,
    projection::{Entry, Tree, make_directory_writable, remove},
};

#[derive(Serialize, Deserialize)]
pub struct Activation {
    pub root: PathBuf,
    pub projection: Tree,
    baseline: Tree,
}

pub struct StatePaths {
    state_root: PathBuf,
    directory: PathBuf,
    current: PathBuf,
    journal: PathBuf,
    preparing: PathBuf,
    gc_roots: PathBuf,
}

#[derive(Serialize, Deserialize)]
enum Transaction {
    Mount {
        previous: Option<PathBuf>,
        next: Option<PathBuf>,
    },
    Unmount {
        activation: PathBuf,
    },
}

impl StatePaths {
    pub fn new(state_root: &Path, image: &str) -> Self {
        let directory = state_root.join("images").join(image);
        Self {
            state_root: state_root.to_path_buf(),
            current: directory.join("current"),
            journal: directory.join("transaction"),
            preparing: directory.join("preparing"),
            gc_roots: directory.join("gc-roots"),
            directory,
        }
    }
}

pub fn recover_all(state_root: &Path) -> Result<()> {
    let images = state_root.join("images");
    let entries = match fs::read_dir(images) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|name| anyhow::anyhow!("image state name is not UTF-8: {name:?}"))?;
        recover(&StatePaths::new(state_root, &name))?;
    }
    Ok(())
}

pub fn mount(image: &str, root: &Path, projection: Tree, paths: &StatePaths) -> Result<()> {
    fs::create_dir_all(&paths.gc_roots)?;
    ensure_root_available(image, root, paths)?;
    let result = (|| {
        let current = load(paths)?;
        if let Some((_, activation)) = &current {
            activation.verify()?;
            activation.ensure_restorable()?;
        }

        let previous = current.as_ref().map(|(path, _)| path.clone());
        if let Some((path, activation)) = &current {
            write_transaction(
                paths,
                &Transaction::Mount {
                    previous: Some(path.clone()),
                    next: None,
                },
            )?;
            activation.deactivate()?;
        }

        remove_preparing(paths)?;
        fs::create_dir(&paths.preparing)?;
        atomic::sync(&paths.directory)?;
        let mut next_backup = 0;
        let baseline = capture_tree(&projection, root, &paths.preparing, &mut next_backup)?;
        let activation = Activation {
            root: root.to_path_buf(),
            projection,
            baseline,
        };
        let activation_path = persist_activation(&activation, paths)?;
        remove_preparing(paths)?;
        write_transaction(
            paths,
            &Transaction::Mount {
                previous,
                next: Some(activation_path.clone()),
            },
        )?;
        activation.activate()?;
        write_current(paths, &activation_path)?;
        clear_transaction(paths)?;
        clean_gc_roots(paths, Some(&activation_path))
    })();
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            if let Err(recovery) = recover(paths) {
                bail!("{error:#}; recovery failed: {recovery:#}");
            }
            Err(error)
        }
    }
}

pub fn unmount(image: &str, paths: &StatePaths) -> Result<()> {
    let Some((activation_path, activation)) = load(paths)? else {
        bail!("image {image} is not mounted");
    };
    activation.verify()?;
    activation.ensure_restorable()?;
    let result = (|| {
        write_transaction(
            paths,
            &Transaction::Unmount {
                activation: activation_path,
            },
        )?;
        activation.deactivate()?;
        remove_current(paths)?;
        clear_transaction(paths)?;
        clean_gc_roots(paths, None)
    })();
    match result {
        Ok(()) => {
            println!("unmounted {image}");
            Ok(())
        }
        Err(error) => {
            if let Err(recovery) = recover(paths) {
                bail!("{error:#}; recovery failed: {recovery:#}");
            }
            Err(error)
        }
    }
}

pub fn load(paths: &StatePaths) -> Result<Option<(PathBuf, Activation)>> {
    let activation_path = match fs::read_to_string(&paths.current) {
        Ok(path) => PathBuf::from(path.trim_end()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    Ok(Some((
        activation_path.clone(),
        read_activation(&activation_path)?,
    )))
}

fn recover(paths: &StatePaths) -> Result<()> {
    let transaction: Transaction = match fs::read(&paths.journal) {
        Ok(transaction) => serde_json::from_slice(&transaction)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            remove_preparing(paths)?;
            let current = load(paths)?;
            return clean_gc_roots(paths, current.as_ref().map(|(path, _)| path));
        }
        Err(error) => return Err(error.into()),
    };
    match transaction {
        Transaction::Mount {
            previous: _,
            next: Some(next),
        } => {
            read_activation(&next)?.activate()?;
            write_current(paths, &next)?;
            clear_transaction(paths)?;
            remove_preparing(paths)?;
            clean_gc_roots(paths, Some(&next))?;
        }
        Transaction::Mount {
            previous: Some(previous),
            next: None,
        } => {
            read_activation(&previous)?.activate()?;
            write_current(paths, &previous)?;
            clear_transaction(paths)?;
            remove_preparing(paths)?;
            clean_gc_roots(paths, Some(&previous))?;
        }
        Transaction::Mount {
            previous: None,
            next: None,
        } => unreachable!("an initial mount is journalled after its activation is prepared"),
        Transaction::Unmount { activation } => {
            read_activation(&activation)?.deactivate()?;
            remove_current(paths)?;
            clear_transaction(paths)?;
            remove_preparing(paths)?;
            clean_gc_roots(paths, None)?;
        }
    }
    Ok(())
}

fn ensure_root_available(image: &str, root: &Path, paths: &StatePaths) -> Result<()> {
    let images = paths.state_root.join("images");
    for entry in fs::read_dir(images)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|name| anyhow::anyhow!("image state name is not UTF-8: {name:?}"))?;
        if name == image {
            continue;
        }
        let Some((_, active)) = load(&StatePaths::new(&paths.state_root, &name))? else {
            continue;
        };
        if root.starts_with(&active.root) || active.root.starts_with(root) {
            bail!(
                "image {name} is already mounted at overlapping root {}",
                active.root.display()
            );
        }
    }
    Ok(())
}

impl Activation {
    fn verify(&self) -> Result<()> {
        let mut drifts = Vec::new();
        self.projection.drifts(&self.root, &mut drifts)?;
        if let Some(drift) = drifts.first() {
            bail!("managed path {} has changed", drift.path().display());
        }
        Ok(())
    }

    fn ensure_restorable(&self) -> Result<()> {
        for (name, projected) in &self.projection.entries {
            ensure_restorable(
                projected,
                &self.baseline.entries[name],
                &self.root.join(name),
            )?;
        }
        Ok(())
    }

    fn activate(&self) -> Result<()> {
        transition_tree(&self.baseline, &self.projection, &self.root)
    }

    fn deactivate(&self) -> Result<()> {
        transition_tree(&self.projection, &self.baseline, &self.root)
    }
}

fn capture_tree(
    projection: &Tree,
    path: &Path,
    backup_roots: &Path,
    next_backup: &mut usize,
) -> Result<Tree> {
    let mut entries = BTreeMap::new();
    for (name, projected) in &projection.entries {
        entries.insert(
            name.clone(),
            capture(projected, &path.join(name), backup_roots, next_backup)?,
        );
    }
    Ok(Tree { entries })
}

fn capture(
    projected: &Entry,
    path: &Path,
    backup_roots: &Path,
    next_backup: &mut usize,
) -> Result<Entry> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Entry::Absent),
        Err(error) => return Err(error.into()),
    };
    if let Entry::Directory { tree, .. } = projected
        && metadata.file_type().is_dir()
    {
        return Ok(Entry::Directory {
            mode: metadata.mode() & 0o7777,
            tree: capture_tree(tree, path, backup_roots, next_backup)?,
        });
    }
    let mut modes = BTreeMap::new();
    collect_modes(path, Path::new(""), &metadata, &mut modes)?;
    let source = add_store_path(
        path,
        "nar",
        "tohru-backup",
        &backup_roots.join(next_backup.to_string()),
    )?;
    *next_backup += 1;
    Ok(Entry::Snapshot { source, modes })
}

fn collect_modes(
    root: &Path,
    relative: &Path,
    metadata: &fs::Metadata,
    modes: &mut BTreeMap<PathBuf, u32>,
) -> Result<()> {
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if !metadata.is_file() && !metadata.is_dir() {
        bail!(
            "cannot back up special file {}",
            root.join(relative).display()
        );
    }
    modes.insert(relative.to_path_buf(), metadata.mode() & 0o7777);
    if metadata.is_dir() {
        for entry in fs::read_dir(root.join(relative))? {
            let entry = entry?;
            let child = relative.join(entry.file_name());
            collect_modes(root, &child, &fs::symlink_metadata(entry.path())?, modes)?;
        }
    }
    Ok(())
}

fn ensure_restorable(projected: &Entry, baseline: &Entry, path: &Path) -> Result<()> {
    match (projected, baseline) {
        (
            Entry::Directory { tree, .. },
            Entry::Directory {
                tree: baseline_tree,
                ..
            },
        ) => {
            for (name, child) in &tree.entries {
                ensure_restorable(child, &baseline_tree.entries[name], &path.join(name))?;
            }
        }
        (Entry::Directory { tree, .. }, Entry::Snapshot { .. }) => {
            ensure_no_unmanaged(tree, path)?;
        }
        _ => {}
    }
    Ok(())
}

fn ensure_no_unmanaged(tree: &Tree, path: &Path) -> Result<()> {
    let expected = tree
        .entries
        .keys()
        .map(OsString::from)
        .collect::<BTreeSet<_>>();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if !expected.contains(&entry.file_name()) {
            bail!(
                "cannot restore over directory with unmanaged entries {}",
                path.display()
            );
        }
        if let Entry::Directory { tree, .. } = &tree.entries[entry
            .file_name()
            .to_str()
            .context("managed directory contains a non-UTF-8 entry")?]
            && fs::symlink_metadata(entry.path())?.file_type().is_dir()
        {
            ensure_no_unmanaged(tree, &entry.path())?;
        }
    }
    Ok(())
}

fn transition_tree(from: &Tree, to: &Tree, root: &Path) -> Result<()> {
    for (name, from) in &from.entries {
        let path = root.join(name);
        transition(from, &to.entries[name], &path)
            .with_context(|| format!("transition {}", path.display()))?;
    }
    Ok(())
}

fn transition(from: &Entry, to: &Entry, path: &Path) -> Result<()> {
    if to.matches(path)? {
        return Ok(());
    }
    match (from, to) {
        (
            Entry::Directory {
                tree: from_tree, ..
            },
            Entry::Directory {
                mode,
                tree: to_tree,
            },
        ) => {
            if !matches!(fs::symlink_metadata(path), Ok(metadata) if metadata.file_type().is_dir())
            {
                bail!("managed path {} changed during transition", path.display());
            }
            make_directory_writable(path)?;
            transition_tree(from_tree, to_tree, path)?;
            fs::set_permissions(path, fs::Permissions::from_mode(*mode))?;
            atomic::sync(path)?;
        }
        (Entry::Directory { tree, .. }, Entry::Absent) => {
            if !matches!(fs::symlink_metadata(path), Ok(metadata) if metadata.file_type().is_dir())
            {
                bail!("managed path {} changed during transition", path.display());
            }
            remove_managed(tree, path)?;
            match fs::remove_dir(path) {
                Ok(()) => atomic::sync(path.parent().expect("managed path has a parent"))?,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::NotFound
                    ) => {}
                Err(error) => return Err(error.into()),
            }
        }
        (Entry::Directory { tree, .. }, _) => {
            match fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_dir() => {
                    ensure_no_unmanaged(tree, path)?;
                    fs::remove_dir_all(path)?;
                    atomic::sync(path.parent().expect("managed path has a parent"))?;
                }
                Ok(_) if from.matches(path)? => remove(path)?,
                Ok(_) => bail!("managed path {} changed during transition", path.display()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            to.materialise(path)?;
        }
        (_, Entry::Directory { mode, tree }) => match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_dir() => {
                make_directory_writable(path)?;
                tree.repair(path)?;
                fs::set_permissions(path, fs::Permissions::from_mode(*mode))?;
                atomic::sync(path)?;
            }
            Ok(_) if from.matches(path)? => {
                remove(path)?;
                to.materialise(path)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => to.materialise(path)?,
            Ok(_) => bail!("managed path {} changed during transition", path.display()),
            Err(error) => return Err(error.into()),
        },
        (_, Entry::Absent) => {
            if !from.matches(path)? {
                bail!("managed path {} changed during transition", path.display());
            }
            remove(path)?;
        }
        _ => {
            if matches!(
                fs::symlink_metadata(path),
                Err(error) if error.kind() == io::ErrorKind::NotFound
            ) {
                to.materialise(path)?;
                return Ok(());
            }
            if !from.matches(path)? {
                bail!("managed path {} changed during transition", path.display());
            }
            let current_is_directory = matches!(
                fs::symlink_metadata(path),
                Ok(metadata) if metadata.file_type().is_dir()
            );
            let target_is_directory = matches!(
                to,
                Entry::Snapshot { source, .. }
                    if fs::symlink_metadata(source)?.file_type().is_dir()
            );
            if current_is_directory || target_is_directory {
                remove(path)?;
            }
            to.materialise(path)?;
        }
    }
    Ok(())
}

fn remove_managed(tree: &Tree, path: &Path) -> Result<()> {
    make_directory_writable(path)?;
    for (name, entry) in &tree.entries {
        let child = path.join(name);
        if let Entry::Directory { tree, .. } = entry
            && matches!(fs::symlink_metadata(&child), Ok(metadata) if metadata.file_type().is_dir())
        {
            remove_managed(tree, &child)?;
            match fs::remove_dir(&child) {
                Ok(()) => atomic::sync(path)?,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::NotFound
                    ) => {}
                Err(error) => return Err(error.into()),
            }
        } else {
            remove(&child)?;
        }
    }
    Ok(())
}

fn persist_activation(activation: &Activation, paths: &StatePaths) -> Result<PathBuf> {
    let temporary = tempfile::NamedTempFile::new_in(&paths.directory)?;
    serde_json::to_writer(temporary.as_file(), activation)?;
    temporary.as_file().sync_all()?;
    let activation_path = add_store_path(
        temporary.path(),
        "text",
        "tohru-activation",
        &paths.preparing.join("activation"),
    )?;
    let gc_root = activation_gc_root(&activation_path, paths)?;
    if !gc_root.exists() {
        fs::create_dir(&gc_root)?;
        atomic::sync(&paths.gc_roots)?;
        let mut objects = vec![activation_path.clone()];
        activation.projection.sources(&mut objects);
        activation.baseline.sources(&mut objects);
        let mut objects = objects
            .iter()
            .map(|object| store_object_path(object))
            .collect::<Result<Vec<_>>>()?;
        objects.sort();
        objects.dedup();
        for (index, object) in objects.into_iter().enumerate() {
            register_gc_root(&gc_root.join(index.to_string()), &object)?;
        }
        atomic::sync(&gc_root)?;
    }
    Ok(activation_path)
}

fn read_activation(path: &Path) -> Result<Activation> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn add_store_path(path: &Path, mode: &str, name: &str, root: &Path) -> Result<PathBuf> {
    loop {
        let output = Command::new("nix")
            .args(["store", "add", "--mode", mode, "--name", name])
            .arg(path)
            .output()
            .context("run nix store add")?;
        if !output.status.success() {
            bail!(
                "nix store add failed: {}",
                String::from_utf8_lossy(&output.stderr).trim_end()
            );
        }
        let store_path = PathBuf::from(String::from_utf8(output.stdout)?.trim_end());
        match register_gc_root(root, &store_path) {
            Ok(()) => return Ok(store_path),
            Err(_)
                if matches!(
                    fs::symlink_metadata(&store_path),
                    Err(error) if error.kind() == io::ErrorKind::NotFound
                ) =>
            {
                atomic::remove(root)?;
            }
            Err(error) => return Err(error),
        }
    }
}

fn register_gc_root(root: &Path, object: &Path) -> Result<()> {
    let output = Command::new("nix-store")
        .arg("--add-root")
        .arg(root)
        .arg("--realise")
        .arg(object)
        .output()
        .context("register activation GC root")?;
    if !output.status.success() {
        bail!(
            "nix-store failed to register activation GC root: {}",
            String::from_utf8_lossy(&output.stderr).trim_end()
        );
    }
    Ok(())
}

fn store_object_path(path: &Path) -> Result<PathBuf> {
    let relative = path
        .strip_prefix("/nix/store")
        .with_context(|| format!("{} is not in the Nix store", path.display()))?;
    let Some(std::path::Component::Normal(name)) = relative.components().next() else {
        bail!("{} does not identify a Nix store object", path.display());
    };
    Ok(Path::new("/nix/store").join(name))
}

fn activation_gc_root(activation: &Path, paths: &StatePaths) -> Result<PathBuf> {
    Ok(paths.gc_roots.join(
        activation
            .file_name()
            .context("activation has no store object name")?,
    ))
}

fn clean_gc_roots(paths: &StatePaths, current: Option<&PathBuf>) -> Result<()> {
    let entries = match fs::read_dir(&paths.gc_roots) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let keep = current
        .map(|activation| activation_gc_root(activation, paths))
        .transpose()?;
    for entry in entries {
        let path = entry?.path();
        if Some(&path) != keep.as_ref() {
            fs::remove_dir_all(path)?;
        }
    }
    atomic::sync(&paths.gc_roots)?;
    Ok(())
}

fn write_transaction(paths: &StatePaths, transaction: &Transaction) -> Result<()> {
    atomic::write(&paths.journal, &serde_json::to_vec(transaction)?)?;
    Ok(())
}

fn clear_transaction(paths: &StatePaths) -> Result<()> {
    atomic::remove(&paths.journal)?;
    Ok(())
}

fn write_current(paths: &StatePaths, activation: &Path) -> Result<()> {
    atomic::write(
        &paths.current,
        format!("{}\n", activation.display()).as_bytes(),
    )?;
    Ok(())
}

fn remove_current(paths: &StatePaths) -> Result<()> {
    atomic::remove(&paths.current)?;
    Ok(())
}

fn remove_preparing(paths: &StatePaths) -> Result<()> {
    match fs::remove_dir_all(&paths.preparing) {
        Ok(()) => atomic::sync(&paths.directory)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}
