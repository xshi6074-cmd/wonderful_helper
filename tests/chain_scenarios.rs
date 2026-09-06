/// The original 40 scenarios live in main(), so Cargo's normal test discovery
/// previously compiled them without executing any assertions.
#[test]
fn original_chain_scenarios() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_premortem"))
        .output()
        .expect("could not start chain scenario runner");
    assert!(output.status.success(), "chain scenarios failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("40 个场景"));
}
