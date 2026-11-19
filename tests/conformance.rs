#[test]
fn conformance_suite_passes_for_every_via() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_transport-conformance"))
        .args([
            "run",
            "--via",
            "all",
            "--transport",
            env!("CARGO_BIN_EXE_transport"),
            "--require-features",
            "manifest_http",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_required_feature_that_no_trial_exercised_fails_the_run() {
    let run = |extra: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_transport-conformance"))
            .args([
                "run",
                "--via",
                "direct",
                "--transport",
                env!("CARGO_BIN_EXE_transport"),
            ])
            .args(extra)
            .output()
            .unwrap()
    };
    assert!(run(&[]).status.success());
    let strict = run(&["--require-features", "manifest_http"]);
    let stderr = String::from_utf8_lossy(&strict.stderr);
    assert!(!strict.status.success(), "{stderr}");
    for name in [
        "manifest_http_upload",
        "manifest_http_fallback",
        "manifest_http_stop",
    ] {
        assert!(
            stderr.contains(&format!(
                "{name}: required transport feature manifest_http never exercised"
            )),
            "{stderr}"
        );
    }
}

#[test]
fn a_single_scenario_run_still_names_the_required_feature_it_could_not_exercise() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_transport-conformance"))
        .args([
            "run",
            "--via",
            "direct",
            "--transport",
            env!("CARGO_BIN_EXE_transport"),
            "--scenario",
            "manifest_http_upload",
            "--require-features",
            "manifest_http",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("required transport feature manifest_http never exercised"),
        "{stderr}"
    );
}
