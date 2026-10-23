#[test]
fn conformance_suite_passes_for_every_via() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_transport-conformance"))
        .args([
            "run",
            "--via",
            "all",
            "--transport",
            env!("CARGO_BIN_EXE_transport"),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
