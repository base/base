//! Negative deployment validation smoke tests.

use std::process::Command;

use base_system_tests::SetupImage;

#[test]
fn rejects_post_denim_block_without_whole_second_timestamp() {
    SetupImage::ensure_built().unwrap();
    let output = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--network",
            "none",
            "--entrypoint",
            "op-deployer",
            "-e",
            "SEQUENCER_ADDR=0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc",
            "devnet-setup:local-v2",
            "--denim-block",
            "25",
            "--zenith-block",
            "26",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("zenith must align to a whole second after Denim")
    );
}
