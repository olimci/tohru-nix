use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs, io,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use ignore::{Match, gitignore::GitignoreBuilder};
use serde::{Deserialize, Serialize};

use crate::atomic;

#[derive(Deserialize)]
pub struct Image {
    pub root: String,
    pub layers: Vec<Layer>,
}

#[derive(Deserialize)]
pub struct Layer {
    pub source: PathBuf,
    target: PathBuf,
    rules: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tree {
    pub entries: BTreeMap<String, Entry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Entry {
    Absent,
    Directory {
        mode: u32,
        tree: Tree,
    },
    Link {
        source: PathBuf,
    },
    Copy {
        source: PathBuf,
        mode: u32,
    },
    Symlink {
        target: PathBuf,
    },
    Snapshot {
        source: PathBuf,
        modes: BTreeMap<PathBuf, u32>,
    },
}

pub enum Drift {
    Missing(PathBuf),
    Replaced(PathBuf),
    Modified(PathBuf),
    CopyModified { path: PathBuf, expected: PathBuf },
    Permissions(PathBuf),
}

enum RuleAction {
    Ignore,
    Link,
    Copy,
    CopyWithMode(u32),
}

struct Rule {
    matcher: ignore::gitignore::Gitignore,
    action: RuleAction,
}

pub fn compile(image: &Image) -> Result<Tree> {
    let mut projection = Tree {
        entries: BTreeMap::new(),
    };
    for layer in &image.layers {
        if !layer.source.is_absolute() {
            bail!("layer source {} is not absolute", layer.source.display());
        }
        if !fs::symlink_metadata(&layer.source)?.is_dir() {
            bail!("layer source {} is not a directory", layer.source.display());
        }
        if layer.target.as_os_str().is_empty()
            || layer
                .target
                .components()
                .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
        {
            bail!("layer target {} must be relative", layer.target.display());
        }
        let rules = layer
            .rules
            .lines()
            .enumerate()
            .filter_map(|(index, line)| {
                if line.is_empty() || line.starts_with('#') {
                    return None;
                }
                Some(parse_rule(index + 1, line))
            })
            .collect::<Result<Vec<_>>>()?;
        projection.merge(wrap(
            walk(&layer.source, Path::new(""), &rules)?,
            &layer.target,
        ));
    }
    Ok(projection)
}

fn parse_rule(line_number: usize, line: &str) -> Result<Rule> {
    let (action, pattern) = if let Some(pattern) = line.strip_prefix("- ") {
        (RuleAction::Ignore, pattern)
    } else if let Some(pattern) = line.strip_prefix("+ ") {
        (RuleAction::Copy, pattern)
    } else if let Some(pattern) = line.strip_prefix("= ") {
        (RuleAction::Link, pattern)
    } else if let Some(rest) = line.strip_prefix('*') {
        let (mode, pattern) = rest
            .split_once(' ')
            .with_context(|| format!("rule {line_number}: permissions require a pattern"))?;
        if mode.len() != 4 || !mode.bytes().all(|byte| matches!(byte, b'0'..=b'7')) {
            bail!("rule {line_number}: permissions must contain four octal digits");
        }
        (
            RuleAction::CopyWithMode(u32::from_str_radix(mode, 8)?),
            pattern,
        )
    } else {
        bail!("rule {line_number}: expected -, +, =, or *MODE");
    };
    if pattern.is_empty() || pattern.starts_with('!') {
        bail!("rule {line_number}: invalid empty or negated pattern");
    }
    let mut builder = GitignoreBuilder::new("");
    builder
        .add_line(None, pattern)
        .with_context(|| format!("rule {line_number}"))?;
    Ok(Rule {
        matcher: builder.build()?,
        action,
    })
}

fn walk(root: &Path, relative: &Path, rules: &[Rule]) -> Result<Tree> {
    let mut entries = BTreeMap::new();
    for entry in fs::read_dir(root.join(relative))? {
        let entry = entry?;
        let name = entry.file_name().into_string().map_err(|name| {
            anyhow::anyhow!("path is not UTF-8: {}", PathBuf::from(name).display())
        })?;
        let child = relative.join(&name);
        let metadata = fs::symlink_metadata(entry.path())?;
        let is_directory = metadata.file_type().is_dir();
        let action = rules
            .iter()
            .rev()
            .find(|rule| {
                matches!(
                    rule.matcher
                        .matched_path_or_any_parents(&child, is_directory),
                    Match::Ignore(_)
                )
            })
            .map(|rule| &rule.action);
        if matches!(action, Some(RuleAction::Ignore)) {
            continue;
        }
        let desired = if metadata.file_type().is_symlink() {
            Entry::Symlink {
                target: fs::read_link(entry.path())?,
            }
        } else if is_directory {
            let mode = match action {
                Some(RuleAction::CopyWithMode(mode)) => *mode,
                Some(RuleAction::Copy) => metadata.mode() & 0o7777,
                Some(RuleAction::Ignore) => unreachable!(),
                Some(RuleAction::Link) | None => metadata.mode() & 0o7777 | 0o200,
            };
            Entry::Directory {
                mode,
                tree: walk(root, &child, rules)?,
            }
        } else if metadata.is_file() {
            match action {
                Some(RuleAction::CopyWithMode(mode)) => Entry::Copy {
                    source: entry.path(),
                    mode: *mode,
                },
                Some(RuleAction::Copy) => Entry::Copy {
                    source: entry.path(),
                    mode: metadata.mode() & 0o7777,
                },
                Some(RuleAction::Ignore) => unreachable!(),
                Some(RuleAction::Link) | None => Entry::Link {
                    source: entry.path(),
                },
            }
        } else {
            bail!("unsupported filesystem object {}", entry.path().display());
        };
        entries.insert(name, desired);
    }
    Ok(Tree { entries })
}

fn wrap(mut tree: Tree, target: &Path) -> Tree {
    for component in target.components().rev() {
        let Component::Normal(name) = component else {
            continue;
        };
        tree = Tree {
            entries: BTreeMap::from([(
                name.to_string_lossy().into_owned(),
                Entry::Directory { mode: 0o755, tree },
            )]),
        };
    }
    tree
}

impl Tree {
    fn merge(&mut self, later: Tree) {
        for (name, later) in later.entries {
            match (self.entries.get_mut(&name), later) {
                (
                    Some(Entry::Directory { mode, tree }),
                    Entry::Directory {
                        mode: later_mode,
                        tree: later_tree,
                    },
                ) => {
                    *mode = later_mode;
                    tree.merge(later_tree);
                }
                (_, later) => {
                    self.entries.insert(name, later);
                }
            }
        }
    }

    pub fn drifts(&self, root: &Path, drifts: &mut Vec<Drift>) -> Result<()> {
        for (name, entry) in &self.entries {
            entry.drifts(&root.join(name), drifts)?;
        }
        Ok(())
    }

    pub fn repair(&self, root: &Path) -> Result<()> {
        for (name, entry) in &self.entries {
            entry.repair(&root.join(name))?;
        }
        Ok(())
    }

    pub fn sources(&self, sources: &mut Vec<PathBuf>) {
        for entry in self.entries.values() {
            match entry {
                Entry::Directory { tree, .. } => tree.sources(sources),
                Entry::Link { source }
                | Entry::Copy { source, .. }
                | Entry::Snapshot { source, .. } => sources.push(source.clone()),
                Entry::Symlink { target } if target.starts_with("/nix/store") => {
                    sources.push(target.clone());
                }
                Entry::Absent | Entry::Symlink { .. } => {}
            }
        }
    }
}

impl Entry {
    fn drifts(&self, path: &Path, drifts: &mut Vec<Drift>) -> Result<()> {
        let metadata = fs::symlink_metadata(path);
        match self {
            Self::Directory { mode, tree } => match metadata {
                Ok(metadata) if metadata.file_type().is_dir() => {
                    if metadata.mode() & 0o7777 != *mode {
                        drifts.push(Drift::Permissions(path.to_path_buf()));
                    }
                    tree.drifts(path, drifts)
                }
                Ok(_) => {
                    drifts.push(Drift::Replaced(path.to_path_buf()));
                    Ok(())
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    drifts.push(Drift::Missing(path.to_path_buf()));
                    Ok(())
                }
                Err(error) => Err(error.into()),
            },
            Self::Link { source } => {
                let changed = match metadata {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        fs::read_link(path)? != *source
                    }
                    Ok(_) => true,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => true,
                    Err(error) => return Err(error.into()),
                };
                if changed {
                    drifts.push(Drift::Modified(path.to_path_buf()));
                }
                Ok(())
            }
            Self::Copy { source, mode } => {
                let changed = match metadata {
                    Ok(metadata) => {
                        !metadata.is_file()
                            || fs::read(path)? != fs::read(source)?
                            || metadata.mode() & 0o7777 != *mode
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => true,
                    Err(error) => return Err(error.into()),
                };
                if changed {
                    drifts.push(Drift::CopyModified {
                        path: path.to_path_buf(),
                        expected: source.clone(),
                    });
                }
                Ok(())
            }
            Self::Symlink { target } => {
                let changed = match metadata {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        fs::read_link(path)? != *target
                    }
                    Ok(_) => true,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => true,
                    Err(error) => return Err(error.into()),
                };
                if changed {
                    drifts.push(Drift::Modified(path.to_path_buf()));
                }
                Ok(())
            }
            Self::Absent | Self::Snapshot { .. } => {
                unreachable!("projection trees contain materialised entries")
            }
        }
    }

    pub fn matches(&self, path: &Path) -> Result<bool> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(matches!(self, Self::Absent));
            }
            Err(error) => return Err(error.into()),
        };
        match self {
            Self::Absent => Ok(false),
            Self::Directory { mode, tree } => {
                if !metadata.file_type().is_dir() || metadata.mode() & 0o7777 != *mode {
                    return Ok(false);
                }
                for (name, entry) in &tree.entries {
                    if !entry.matches(&path.join(name))? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Self::Link { source } => {
                Ok(metadata.file_type().is_symlink() && fs::read_link(path)? == *source)
            }
            Self::Copy { source, mode } => Ok(metadata.is_file()
                && metadata.mode() & 0o7777 == *mode
                && fs::read(path)? == fs::read(source)?),
            Self::Symlink { target } => {
                Ok(metadata.file_type().is_symlink() && fs::read_link(path)? == *target)
            }
            Self::Snapshot { source, modes } => {
                snapshot_matches(source, path, Path::new(""), modes)
            }
        }
    }

    pub fn materialise(&self, path: &Path) -> Result<()> {
        match self {
            Self::Absent => {}
            Self::Directory { mode, tree } => {
                fs::create_dir(path)?;
                atomic::sync(path.parent().expect("managed path has a parent"))?;
                for (name, entry) in &tree.entries {
                    entry.materialise(&path.join(name))?;
                }
                fs::set_permissions(path, fs::Permissions::from_mode(*mode))?;
                atomic::sync(path)?;
            }
            Self::Link { source } => atomic::link(source, path)?,
            Self::Copy { source, mode } => atomic::copy(source, path, *mode)?,
            Self::Symlink { target } => atomic::link(target, path)?,
            Self::Snapshot { source, modes } => {
                let parent = path.parent().expect("managed path has a parent");
                let temporary = tempfile::tempdir_in(parent)?;
                let staged = temporary.path().join("snapshot");
                copy_entry(source, &staged)?;
                for (relative, mode) in modes.iter().rev() {
                    let path = if relative.as_os_str().is_empty() {
                        staged.clone()
                    } else {
                        staged.join(relative)
                    };
                    fs::set_permissions(&path, fs::Permissions::from_mode(*mode))?;
                    atomic::sync(&path)?;
                }
                fs::rename(staged, path)?;
                atomic::sync(parent)?;
            }
        }
        Ok(())
    }

    fn repair(&self, path: &Path) -> Result<()> {
        if self.matches(path)? {
            return Ok(());
        }
        match self {
            Self::Directory { mode, tree } => {
                match fs::symlink_metadata(path) {
                    Ok(metadata) if metadata.file_type().is_dir() => make_directory_writable(path)?,
                    Ok(_) => {
                        remove(path)?;
                        fs::create_dir(path)?;
                        atomic::sync(path.parent().expect("managed path has a parent"))?;
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        fs::create_dir(path)?;
                        atomic::sync(path.parent().expect("managed path has a parent"))?;
                    }
                    Err(error) => return Err(error.into()),
                }
                tree.repair(path)?;
                fs::set_permissions(path, fs::Permissions::from_mode(*mode))?;
                atomic::sync(path)?;
            }
            Self::Absent => remove(path)?,
            _ => {
                if matches!(fs::symlink_metadata(path), Ok(metadata) if metadata.file_type().is_dir())
                {
                    fs::remove_dir_all(path)?;
                }
                self.materialise(path)?;
            }
        }
        Ok(())
    }
}

impl Drift {
    pub fn path(&self) -> &Path {
        match self {
            Self::Missing(path)
            | Self::Replaced(path)
            | Self::Modified(path)
            | Self::CopyModified { path, .. }
            | Self::Permissions(path) => path,
        }
    }

    pub fn expected(&self) -> Option<&Path> {
        match self {
            Self::CopyModified { expected, .. } => Some(expected),
            _ => None,
        }
    }
}

impl fmt::Display for Drift {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Missing(_) => "missing",
            Self::Replaced(_) => "replaced",
            Self::Modified(_) | Self::CopyModified { .. } => "modified",
            Self::Permissions(_) => "permissions",
        })
    }
}

pub fn remove(path: &Path) -> Result<()> {
    let removed = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            fs::remove_dir_all(path)?;
            true
        }
        Ok(_) => {
            fs::remove_file(path)?;
            true
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    if removed {
        atomic::sync(path.parent().expect("managed path has a parent"))?;
    }
    Ok(())
}

pub fn make_directory_writable(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.mode() & 0o200 == 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(metadata.mode() | 0o200))?;
        atomic::sync(path)?;
    }
    Ok(())
}

fn copy_entry(source: &Path, target: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        symlink(fs::read_link(source)?, target)?;
    } else if metadata.is_dir() {
        fs::create_dir(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_entry(&entry.path(), &target.join(entry.file_name()))?;
        }
        atomic::sync(target)?;
    } else {
        fs::copy(source, target)?;
        fs::File::open(target)?.sync_all()?;
    }
    Ok(())
}

fn snapshot_matches(
    source: &Path,
    target: &Path,
    relative: &Path,
    modes: &BTreeMap<PathBuf, u32>,
) -> Result<bool> {
    let source_path = if relative.as_os_str().is_empty() {
        source.to_path_buf()
    } else {
        source.join(relative)
    };
    let target_path = if relative.as_os_str().is_empty() {
        target.to_path_buf()
    } else {
        target.join(relative)
    };
    let source_metadata = fs::symlink_metadata(&source_path)?;
    let target_metadata = match fs::symlink_metadata(&target_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if source_metadata.file_type().is_symlink() {
        return Ok(target_metadata.file_type().is_symlink()
            && fs::read_link(source_path)? == fs::read_link(target_path)?);
    }
    if source_metadata.is_file() {
        return Ok(target_metadata.is_file()
            && target_metadata.mode() & 0o7777 == modes[relative]
            && fs::read(source_path)? == fs::read(target_path)?);
    }
    if !target_metadata.file_type().is_dir() || target_metadata.mode() & 0o7777 != modes[relative] {
        return Ok(false);
    }
    let source_names = fs::read_dir(source_path)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<BTreeSet<_>>>()?;
    let target_names = fs::read_dir(target_path)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<BTreeSet<_>>>()?;
    if source_names != target_names {
        return Ok(false);
    }
    for name in source_names {
        if !snapshot_matches(source, target, &relative.join(name), modes)? {
            return Ok(false);
        }
    }
    Ok(true)
}
