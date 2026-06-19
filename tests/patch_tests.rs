use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// Integration test against the local ModRetro preservation archive.
/// Skipped silently if the archive is not on this machine.
#[test]
fn apply_modretro_tetris_patch() {
    let archive = PathBuf::from("/Users/nathankellenicki/Desktop/ModRetro_Backups/archive");
    let base_rom = archive.join("tetris 1.0.gbc");
    let patch_file = archive.join("tetris 1.4.ips");

    if !base_rom.exists() || !patch_file.exists() {
        eprintln!("Skipping: local ModRetro archive not found");
        return;
    }

    let rom = std::fs::read(&base_rom).unwrap();
    let patch_data = std::fs::read(&patch_file).unwrap();

    assert_eq!(
        format!("{:x}", Sha256::digest(&rom)),
        "60e708be1eed554b0c0752f42f20d07c33125adc775b3b9b083699771a446d3a"
    );

    let patch = throwback::patch::IpsPatch::load(&patch_data).unwrap();
    let patched = patch.apply(&rom);

    assert_eq!(
        format!("{:x}", Sha256::digest(&patched)),
        "c62248a77795c6ab276979bbef8159e9c587c4fe3e8d2592d2d769ca559dddfc"
    );

    assert!(
        matches!(
            throwback::patch::validate_patched_rom(&patched),
            throwback::patch::Validation::Ok
        ),
        "patched ROM header invalid"
    );
}
