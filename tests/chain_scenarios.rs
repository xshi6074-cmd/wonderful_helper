/// 链路场景全在 `main()` 里，Cargo 的常规发现只会编译它、不会执行任何断言。
/// 这个包装把它真的跑起来。
///
/// **不要断言场景个数** —— 上一版写死了「40 个场景」，加一个场景就得回来改一次，
/// 而这个断言本身不检查任何东西。要看的是「有没有失败」。
#[test]
fn original_chain_scenarios() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_premortem"))
        .output()
        .expect("could not start chain scenario runner");
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "chain scenarios failed:\n{out}\n{}",
        String::from_utf8_lossy(&output.stderr));
    assert!(out.contains("0 失败"), "有场景断言没过：\n{out}");
    assert!(!out.contains('\u{2717}'), "有场景断言没过：\n{out}");
}
