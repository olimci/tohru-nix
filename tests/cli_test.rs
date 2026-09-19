use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::json;

#[test]
fn rejects_image_names_that_are_not_literal_path_components() {
    let temporary = tempfile::tempdir().unwrap();
    let manifest = temporary.path().join("manifest.json");
    fs::write(
        &manifest,
        serde_json::to_vec(&json!({ "images": {} })).unwrap(),
    )
    .unwrap();

    for name in ["../outside", "/outside", "image/"] {
        let output = tohru(&manifest, &temporary.path().join("state"), ["status", name]);
        assert!(!output.status.success(), "accepted {name:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("normal path component"));
    }
}

#[test]
#[ignore = "requires a writable Nix store"]
fn mount_check_fix_and_unmount_a_layered_tree() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("root");
    let first = temporary.path().join("first");
    let second = temporary.path().join("second");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("linked"), b"original").unwrap();
    fs::create_dir(root.join("copied")).unwrap();
    fs::write(root.join("copied/original"), b"original").unwrap();
    fs::create_dir(&first).unwrap();
    fs::create_dir(first.join("empty")).unwrap();
    fs::write(first.join("linked"), b"first").unwrap();
    fs::write(first.join("copied"), b"copy").unwrap();
    fs::create_dir(&second).unwrap();
    fs::write(second.join("linked"), b"second").unwrap();
    let first = add_to_store(&first);
    let second = add_to_store(&second);

    let manifest = temporary.path().join("manifest.json");
    fs::write(
        &manifest,
        serde_json::to_vec(&json!({
            "images": {
                "test": {
                    "root": root,
                    "layers": [
                        {
                            "source": first,
                            "target": ".",
                            "rules": "+ copied\n*0600 copied"
                        },
                        { "source": second, "target": ".", "rules": "" }
                    ]
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let state = temporary.path().join("state");

    assert_success(tohru(&manifest, &state, ["mount", "test"]));
    let image_state = state.join("images/test");
    let activation = fs::read_to_string(image_state.join("current")).unwrap();
    let output = Command::new("nix-store")
        .args(["--query", "--roots"])
        .arg(activation.trim_end())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "nix-store query failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let gc_roots = image_state.join("gc-roots");
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|root| Path::new(root).starts_with(&gc_roots))
    );
    assert_eq!(fs::read(root.join("linked")).unwrap(), b"second");
    assert!(
        !fs::symlink_metadata(root.join("copied"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::symlink_metadata(root.join("copied")).unwrap().mode() & 0o7777,
        0o600
    );

    fs::write(root.join("empty/user-file"), b"user").unwrap();
    fs::set_permissions(root.join("copied"), fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(root.join("copied"), b"changed").unwrap();
    assert!(!tohru(&manifest, &state, ["check", "test"]).status.success());
    assert_success(tohru(&manifest, &state, ["check", "test", "--fix"]));
    assert_eq!(fs::read(root.join("copied")).unwrap(), b"copy");

    let invalid = temporary.path().join("invalid");
    fs::create_dir(&invalid).unwrap();
    fs::write(invalid.join("linked"), b"invalid").unwrap();
    fs::write(
        &manifest,
        serde_json::to_vec(&json!({
            "images": {
                "test": {
                    "root": root,
                    "layers": [{ "source": invalid, "target": ".", "rules": "" }]
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
    assert!(
        !tohru(&manifest, &state, ["refresh", "test"])
            .status
            .success()
    );
    assert_eq!(fs::read(root.join("linked")).unwrap(), b"second");
    assert!(!image_state.join("transaction").exists());

    let refreshed = temporary.path().join("refreshed");
    fs::create_dir(&refreshed).unwrap();
    fs::write(refreshed.join("linked"), b"refreshed").unwrap();
    let refreshed = add_to_store(&refreshed);
    fs::write(
        &manifest,
        serde_json::to_vec(&json!({
            "images": {
                "test": {
                    "root": root,
                    "layers": [{ "source": refreshed, "target": ".", "rules": "" }]
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let output = tohru(&manifest, &state, ["refresh", "test"]);
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("refreshed test at "));
    assert_success(output);
    assert_eq!(fs::read(root.join("linked")).unwrap(), b"refreshed");

    assert_success(tohru(&manifest, &state, ["unmount", "test"]));
    assert_eq!(fs::read(root.join("linked")).unwrap(), b"original");
    assert_eq!(fs::read(root.join("copied/original")).unwrap(), b"original");
    assert_eq!(fs::read(root.join("empty/user-file")).unwrap(), b"user");
    assert!(
        !tohru(&manifest, &state, ["refresh", "test"])
            .status
            .success()
    );

    assert_success(tohru(&manifest, &state, ["mount", "test"]));
    let activation = fs::read_to_string(image_state.join("current")).unwrap();
    fs::write(
        image_state.join("transaction"),
        serde_json::to_vec(&json!({
            "Mount": {
                "previous": activation.trim_end(),
                "next": activation.trim_end()
            }
        }))
        .unwrap(),
    )
    .unwrap();
    fs::remove_file(root.join("linked")).unwrap();

    assert_success(tohru(&manifest, &state, ["list"]));
    assert_eq!(fs::read(root.join("linked")).unwrap(), b"refreshed");

    fs::write(
        image_state.join("transaction"),
        serde_json::to_vec(&json!({
            "Unmount": { "activation": activation.trim_end() }
        }))
        .unwrap(),
    )
    .unwrap();
    fs::remove_file(root.join("linked")).unwrap();

    assert_success(tohru(&manifest, &state, ["list"]));
    assert!(!image_state.join("current").exists());
    assert_eq!(fs::read(root.join("linked")).unwrap(), b"original");
    assert_eq!(fs::read(root.join("copied/original")).unwrap(), b"original");
    assert_eq!(fs::read(root.join("empty/user-file")).unwrap(), b"user");
}

fn add_to_store(path: &Path) -> PathBuf {
    let output = Command::new("nix")
        .args(["store", "add", "--mode", "nar", "--name", "tohru-test"])
        .arg(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "nix store add failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    PathBuf::from(String::from_utf8(output.stdout).unwrap().trim_end())
}

fn tohru<const N: usize>(manifest: &Path, state: &Path, arguments: [&str; N]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tohru"))
        .arg("--manifest")
        .arg(manifest)
        .arg("--state")
        .arg(state)
        .args(arguments)
        .output()
        .unwrap()
}

#[track_caller]
fn assert_success(output: Output) {
    assert!(
        output.status.success(),
        "command failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
