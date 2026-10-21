#[tokio::test]
async fn conformance_suite_passes_for_direct_and_stdio() {
    let bin = std::path::PathBuf::from(env!("CARGO_BIN_EXE_transport"));
    transport::conformance::run_suite(&bin, None, transport::conformance::SuiteVia::Both)
        .await
        .unwrap();
}
