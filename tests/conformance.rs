#[test]
fn conformance_suite_passes_for_direct_and_stdio() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_transport-conformance"))
        .args([
            "run",
            "--via",
            "both",
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
