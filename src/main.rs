mod atomic;
mod projection;
mod state;

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::Deserialize;

use crate::{
    projection::{Image, compile},
    state::{Activation, StatePaths},
};

#[derive(Parser)]
#[command(about = "Safely materialise Nix-built file trees")]
struct Arguments {
    #[arg(long, global = true, hide = true)]
    manifest: Option<PathBuf>,
    #[arg(long, global = true, hide = true)]
    state: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Mount {
        image: String,
    },
    Refresh {
        image: String,
    },
    Unmount {
        image: String,
    },
    Check {
        image: String,
        #[arg(long)]
        fix: bool,
        #[arg(long)]
        diff: bool,
        #[arg(long, requires = "diff")]
        diff_tool: Option<OsString>,
    },
    Status {
        image: Option<String>,
    },
    List,
}

#[derive(Deserialize)]
struct Manifest {
    images: BTreeMap<String, Image>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let arguments = Arguments::parse();
    match &arguments.command {
        Command::Mount { image }
        | Command::Refresh { image }
        | Command::Unmount { image }
        | Command::Check { image, .. } => validate_name(image)?,
        Command::Status { image: Some(image) } => validate_name(image)?,
        Command::Status { image: None } | Command::List => {}
    }
    let manifest_path = arguments
        .manifest
        .context("this executable is not bound to a manifest")?;
    let manifest: Manifest = serde_json::from_slice(
        &fs::read(&manifest_path)
            .with_context(|| format!("read manifest {}", manifest_path.display()))?,
    )?;
    validate_names(&manifest)?;

    let state_root = match arguments.state {
        Some(path) => path,
        None => std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
            })
            .context("HOME or XDG_STATE_HOME must be set")?
            .join("tohru"),
    };
    fs::create_dir_all(&state_root)?;
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .truncate(false)
        .write(true)
        .open(state_root.join("lock"))?;
    lock.lock()?;
    state::recover_all(&state_root)?;

    let refreshing = matches!(&arguments.command, Command::Refresh { .. });
    match arguments.command {
        Command::Mount { image } | Command::Refresh { image } => {
            let declared = manifest
                .images
                .get(&image)
                .with_context(|| format!("unknown image {image}"))?;
            let root = resolve_root(&declared.root)?;
            let projection = compile(declared)?;
            let paths = StatePaths::new(&state_root, &image);
            if refreshing && state::load(&paths)?.is_none() {
                bail!("image {image} is not mounted");
            }
            state::mount(&image, &root, projection, &paths)?;
            println!(
                "{} {image} at {}",
                if refreshing { "refreshed" } else { "mounted" },
                root.display()
            );
            Ok(())
        }
        Command::Unmount { image } => state::unmount(&image, &StatePaths::new(&state_root, &image)),
        Command::Check {
            image,
            fix,
            diff,
            diff_tool,
        } => check(
            &image,
            fix,
            diff,
            diff_tool,
            &StatePaths::new(&state_root, &image),
        ),
        Command::Status { image } => status(image, &manifest, &state_root),
        Command::List => {
            for name in manifest.images.keys() {
                let mounted = state::load(&StatePaths::new(&state_root, name))?.is_some();
                println!(
                    "{}\t{}",
                    if mounted { "mounted" } else { "unmounted" },
                    name
                );
            }
            Ok(())
        }
    }
}

fn validate_names(manifest: &Manifest) -> Result<()> {
    for name in manifest.images.keys() {
        validate_name(name)?;
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<()> {
    let path = Path::new(name);
    if name.contains('/')
        || path.components().count() != 1
        || !matches!(
            path.components().next(),
            Some(std::path::Component::Normal(_))
        )
    {
        bail!("image name {name:?} must be a normal path component");
    }
    Ok(())
}

fn resolve_root(root: &str) -> Result<PathBuf> {
    let path = if root == "~" {
        PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
    } else if let Some(relative) = root.strip_prefix("~/") {
        PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(relative)
    } else {
        PathBuf::from(root)
    };
    if !path.is_absolute() {
        bail!("image root {root:?} must be ~, below ~, or absolute");
    }
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("read image root {}", path.display()))?;
    if !metadata.is_dir() {
        bail!("image root {} is not a directory", path.display());
    }
    Ok(fs::canonicalize(path)?)
}

fn check(
    image: &str,
    fix: bool,
    show_diff: bool,
    diff_tool: Option<OsString>,
    paths: &StatePaths,
) -> Result<()> {
    let Some((_, active)) = state::load(paths)? else {
        bail!("image {image} is not mounted");
    };
    let mut drifts = Vec::new();
    active.projection.drifts(&active.root, &mut drifts)?;
    if drifts.is_empty() {
        println!("{image} is intact");
        return Ok(());
    }
    for drift in &drifts {
        println!("{}\t{}", drift, drift.path().display());
    }
    if show_diff {
        let tool = diff_tool
            .or_else(|| std::env::var_os("TOHRU_DIFF_TOOL"))
            .unwrap_or_else(|| OsString::from("diff"));
        for (source, target) in drifts
            .iter()
            .filter_map(|drift| drift.expected().map(|source| (source, drift.path())))
        {
            ProcessCommand::new(&tool)
                .arg(source)
                .arg(target)
                .status()
                .with_context(|| format!("run diff tool {}", Path::new(&tool).display()))?;
        }
    }
    if fix {
        active.projection.repair(&active.root)?;
        println!("repaired {image}");
        Ok(())
    } else {
        bail!("image {image} has changed")
    }
}

fn status(image: Option<String>, manifest: &Manifest, root: &Path) -> Result<()> {
    let names = match image {
        Some(name) => vec![name],
        None => manifest.images.keys().cloned().collect(),
    };
    for name in names {
        match state::load(&StatePaths::new(root, &name))? {
            None => println!("unmounted\t{name}"),
            Some((
                _,
                Activation {
                    root, projection, ..
                },
            )) => {
                let mut drifts = Vec::new();
                projection.drifts(&root, &mut drifts)?;
                println!(
                    "{}\t{name}\t{}",
                    if drifts.is_empty() {
                        "mounted"
                    } else {
                        "changed"
                    },
                    root.display()
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_name;

    #[test]
    fn image_names_are_single_literal_path_components() {
        assert!(validate_name("shell").is_ok());
        for name in ["", ".", "..", "shell/", "shell//", "shell/editor", "/shell"] {
            assert!(validate_name(name).is_err(), "accepted {name:?}");
        }
    }
}
