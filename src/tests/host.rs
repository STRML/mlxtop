// SPDX-License-Identifier: MIT
use super::*;

#[test]
fn system_host_reads_files_directories_and_commands() {
    let root = std::env::temp_dir().join(format!("mlxtop-host-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("zone0")).unwrap();
    fs::write(root.join("zone0/temp"), "41000\n").unwrap();
    let host = System;
    assert_eq!(
        host.read_file(&root.join("zone0/temp")).as_deref(),
        Some("41000\n")
    );
    assert_eq!(host.read_file(&root.join("missing")), None);
    assert_eq!(host.read_dir(&root), vec![root.join("zone0")]);
    assert!(host.read_dir(&root.join("missing")).is_empty());
    assert_eq!(host.command_u64("sh", &["-c", "echo ' 42 '"]), Some(42));
    assert_eq!(host.command_u64("sh", &["-c", "echo nope"]), None);
    assert_eq!(host.command("sh", &["-c", "exit 3"]), None);
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn platform_matches_the_build_target() {
    let expected = if cfg!(target_os = "macos") {
        Platform::MacOs
    } else {
        Platform::Linux
    };
    assert_eq!(Platform::current(), expected);
}
