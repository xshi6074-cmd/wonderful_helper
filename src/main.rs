//! 链路测试：把 core 的每条关键路径各跑一遍，把指标打出来。
//!
//! 跑法：`cargo run`（WSL 里 `CARGO_TARGET_DIR=~/.cache/premortem-target cargo run`）。
//! 全部用假模型，毫秒级、确定性、不联网。
//!
//! S1  正常一轮 + 落盘顺序 ①history→②commit→③snapshot
//! S2  场景注入：判断段判定场景 → 注入 guidance + 暴露工具 → **回答段照常跑**
//! S2b 模型自主提问：模型自己调 ask_user，UI 渲染选择题，harness 不掐断
//! S3  工具心跳 / S3b 硬超时
//! S4  打断：两段式反馈 + 收尾 + workspace 保留 + 估算记账
//! S5  排队注入 / S6 写权冲突 Stale / S8 去重
//! S7  工具失败**不打扰用户**
//! S9  用户轮中编辑**不进当前 turn**，下一轮才带上
//! S10 上下文分层：两段吃同一份对话，guidance 与案例只给回答段
//! S11 上下文压缩：接近上限时折叠早期对话，摘要留在上下文里
//! S12 一键蒸馏：用户触发，产出草稿文件
//! S13 持久层 bootstrap：落成用户可直接编辑的文件并能读回

use premortem::context::{ContextLimit, Footprint};
use premortem::core::{CoreDeps, CoreSummary, start};
use premortem::handle::CoreHandle;
use premortem::ids::Version;
use premortem::memory::Memory;
use premortem::mock::{EchoTool, FlakyTool, MockModel, SlowTool, ask_call, call};
use premortem::model::{JudgeOut, Mode, Models, Role, StreamEvent, Usage};
use premortem::msg::{SendMode, UiEvent};
use premortem::persist::{WriterCfg, spawn_writer};
use premortem::scene::Playbook;
use premortem::state::{Op, Origin, Path, Source};
use premortem::tools::{AskUser, Registry, ToolConfig};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;

static FAILS: AtomicU32 = AtomicU32::new(0);

fn check(label: &str, cond: bool) {
    if cond {
        println!("    ✓ {label}");
    } else {
        println!("    ✗ {label}");
        FAILS.fetch_add(1, Ordering::Relaxed);
    }
}

struct Rig {
    handle: CoreHandle,
    ui: broadcast::Receiver<UiEvent>,
    audit: Arc<Mutex<Vec<String>>>,
    core_join: tokio::task::JoinHandle<CoreSummary>,
    writer_join: tokio::task::JoinHandle<()>,
    log: Vec<UiEvent>,
}

fn default_registry() -> Registry {
    Registry::new().with(Arc::new(AskUser))
}

struct RigOpts {
    mode: Mode,
    tool_config: ToolConfig,
    context_limit: ContextLimit,
    memory_dir: Option<std::path::PathBuf>,
}

impl Default for RigOpts {
    fn default() -> Self {
        Self {
            mode: Mode::Explore,
            tool_config: ToolConfig::default(),
            context_limit: ContextLimit::default(),
            memory_dir: None,
        }
    }
}

fn rig(model: Arc<MockModel>, registry: Registry, o: RigOpts) -> Rig {
    let audit = Arc::new(Mutex::new(Vec::new()));
    // dir=None：只记审计顺序，不真写盘。落盘顺序在 S1 用 audit 断言。
    let (writer, writer_join) = spawn_writer(WriterCfg { dir: None, audit: Some(audit.clone()) });
    let models = Arc::new(Models::uniform(model));
    let mut deps = CoreDeps::new(
        models,
        Arc::new(registry),
        Arc::new(Memory::empty()),
        writer,
        o.mode,
    );
    deps.tool_config = o.tool_config;
    deps.context_limit = o.context_limit;
    deps.memory_dir = o.memory_dir;
    let started = start(deps);
    Rig {
        handle: started.handle,
        ui: started.ui,
        audit,
        core_join: started.join,
        writer_join,
        log: Vec::new(),
    }
}

impl Rig {
    async fn wait<F: FnMut(&UiEvent) -> bool>(&mut self, mut pred: F) -> bool {
        let rx = &mut self.ui;
        let log = &mut self.log;
        let fut = async {
            loop {
                match rx.recv().await {
                    Ok(e) => {
                        let hit = pred(&e);
                        log.push(e);
                        if hit {
                            return true;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return false,
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(5), fut).await.unwrap_or(false)
    }

    fn count<F: Fn(&UiEvent) -> bool>(&self, pred: F) -> usize {
        self.log.iter().filter(|e| pred(e)).count()
    }

    async fn finish(self) -> (CoreSummary, Vec<String>) {
        self.handle.shutdown().await;
        let s = self.core_join.await.expect("core panicked");
        let _ = self.writer_join.await;
        let a = self.audit.lock().unwrap().clone();
        (s, a)
    }
}

fn judged(scene: &str, ops: Vec<Op>) -> JudgeOut {
    JudgeOut {
        scene: scene.into(),
        rationale: format!("判成 {scene} 的理由"),
        ops,
        retrieve: vec![],
        usage: Usage { prompt: 200, completion: 40, estimated: false },
    }
}

// ───────────────────────────── 场景 ─────────────────────────────

async fn s1_normal_turn() {
    println!("\nS1 正常一轮 + 落盘顺序");
    let model = Arc::new(MockModel::new().on_judge(judged(
        "none",
        vec![Op::set("spec.claim", "减少灾难性遗忘")],
    )));
    let mut r = rig(model, default_registry(), RigOpts::default());
    r.handle.user_input("我想验证新的 replay 策略", SendMode::Queue).await;
    check("turn 正常收敛", r.wait(|e| matches!(e, UiEvent::TurnClosed { .. })).await);
    check("侧栏先亮", r.count(|e| matches!(e, UiEvent::PendingChanged(_))) == 1);
    check("有流式 Delta", r.count(|e| matches!(e, UiEvent::Delta(_))) > 0);
    check("场景被报给 UI", r.count(|e| matches!(e, UiEvent::SceneChosen { .. })) >= 1);
    check("上下文占用被报给状态栏", r.count(|e| matches!(e, UiEvent::ContextFootprint { .. })) >= 1);

    let (s, audit) = r.finish().await;
    println!("    落盘顺序: {audit:?}");
    let hi = audit.iter().position(|x| x.starts_with("history"));
    let sn = audit.iter().position(|x| x.starts_with("snapshot"));
    check("① history 早于 ③ snapshot", matches!((hi, sn), (Some(a), Some(b)) if a < b));
    check("模型的改动最终被提交", s.ws.fields.contains_key(&Path::new("spec.claim")));
    let f = s.ws.fields.get(&Path::new("spec.claim")).unwrap();
    check("出处是 Model", f.origin == Origin::Model);
    check("没标来源的推断算虚线", f.is_dashed());
    check("来源与置信度是可选的，没填就不填", f.source.is_none() && f.confidence.is_none());
    check("会话历史留下了", !s.history.is_empty());
    println!("    {}", s.metrics.line());
    println!("    {}", s.cost.line());
}

async fn s2_scene_injection() {
    println!("\nS2 场景注入（harness 只准备材料，不代替模型决定流程）");
    let model = Arc::new(MockModel::new().on_judge(judged("clarify_goal", vec![])));
    let mut r = rig(model.clone(), default_registry(), RigOpts::default());
    r.handle.user_input("在 FlyGCL 上加个新损失看能不能减少遗忘", SendMode::Queue).await;
    check("turn 收敛", r.wait(|e| matches!(e, UiEvent::TurnClosed { .. })).await);

    let ap = model.answer_prompt(0);
    check("回答段确实跑了（harness 没有抢占掉它）", model.answer_calls() == 1);
    check("scene 的 guidance 被注入", ap.contains("用户的目标还没落定"));
    check("scene 的 label 被注入", ap.contains("澄清目标"));
    check("system 里有「不主导流程」约束", ap.contains("不主导流程"));
    check("system 里有替代锁的那句 prompt 约束", ap.contains("[用户设定]"));

    let (s, _) = r.finish().await;
    check(
        "判断段与回答段都记了账",
        s.cost.of(Role::Judge).total() > 0 && s.cost.of(Role::Answer).total() > 0,
    );
    println!("    {}", s.metrics.line());
    println!("    {}", s.cost.line());
}

async fn s2b_model_asks() {
    println!("\nS2b 模型自主提问（ask_user 是工具，不是 harness 动作）");
    let model = Arc::new(
        MockModel::new()
            .on_judge(judged("clarify_goal", vec![]))
            .on_answer(vec![
                StreamEvent::Chunk("我先说一句我的理解。".into()),
                StreamEvent::ToolCalls(vec![ask_call(
                    "a1",
                    "你说的「减少遗忘」，打算用哪个量来观测？",
                    &["BWT", "逐任务准确率矩阵", "只讲平均精度"],
                )]),
            ])
            .on_judge(judged("clarify_goal", vec![]))
            .on_answer(vec![
                StreamEvent::Chunk("等你选完我们继续。".into()),
                StreamEvent::Done(Usage { prompt: 500, completion: 40, estimated: false }),
            ]),
    );
    let mut r = rig(model.clone(), default_registry(), RigOpts::default());
    r.handle.user_input("加个新损失减少遗忘", SendMode::Queue).await;
    check("turn 收敛", r.wait(|e| matches!(e, UiEvent::TurnClosed { .. })).await);

    check("UI 收到选择题", r.count(|e| matches!(e, UiEvent::Choice { .. })) == 1);
    let opts = r.log.iter().find_map(|e| match e {
        UiEvent::Choice { options, .. } => Some(options.len()),
        _ => None,
    });
    check("选择题带 3 个候选项", opts == Some(3));
    check(
        "模型在提问前先说了正文（harness 没把回答替换成问题）",
        r.count(|e| matches!(e, UiEvent::Delta(d) if d.contains("我先说一句"))) == 1,
    );
    check("提问之后模型自己决定继续说", model.answer_calls() == 2);

    let (s, _) = r.finish().await;
    println!("    {}", s.metrics.line());
}

async fn s3_heartbeat() {
    println!("\nS3 工具心跳（长任务不阻塞，turn 仍串行）");
    let model = Arc::new(MockModel::new().on_judge(JudgeOut {
        retrieve: vec![call("c1", "slow"), call("c2", "echo")],
        ..judged("trace_code", vec![])
    }));
    let reg = default_registry()
        .with(Arc::new(SlowTool::parallel("slow", Duration::from_millis(260))))
        .with(Arc::new(EchoTool));
    let o = RigOpts {
        tool_config: ToolConfig {
            heartbeat: Duration::from_millis(50),
            hard_limit: Duration::from_secs(10),
        },
        ..RigOpts::default()
    };
    let mut r = rig(model, reg, o);
    r.handle.user_input("先把 generator 那条链路讲通", SendMode::Queue).await;
    check("turn 收敛", r.wait(|e| matches!(e, UiEvent::TurnClosed { .. })).await);
    let hb = r.count(|e| matches!(e, UiEvent::StillRunning { .. }));
    println!("    心跳次数 = {hb}（260ms 任务 / 50ms 心跳，期望 ≥3）");
    check("长任务期间发出了心跳", hb >= 3);
    check("两个工具各报一次 TaskDone", r.count(|e| matches!(e, UiEvent::TaskDone { .. })) == 2);
    let (s, _) = r.finish().await;
    println!("    {}", s.metrics.line());
}

async fn s3b_hard_limit() {
    println!("\nS3b 硬超时（带 timeout 结果继续，不是整轮失败）");
    let model = Arc::new(MockModel::new().on_judge(JudgeOut {
        retrieve: vec![call("c1", "forever")],
        ..judged("none", vec![])
    }));
    let reg =
        default_registry().with(Arc::new(SlowTool::parallel("forever", Duration::from_secs(60))));
    let o = RigOpts {
        tool_config: ToolConfig {
            heartbeat: Duration::from_millis(40),
            hard_limit: Duration::from_millis(120),
        },
        ..RigOpts::default()
    };
    let mut r = rig(model, reg, o);
    r.handle.user_input("跑个永远回不来的工具", SendMode::Queue).await;
    check(
        "硬超时之后这一轮仍然正常收敛",
        r.wait(|e| matches!(e, UiEvent::TurnClosed { aborted: false, .. })).await,
    );
    check("超时前发过心跳", r.count(|e| matches!(e, UiEvent::StillRunning { .. })) >= 2);
    let (s, _) = r.finish().await;
    println!("    {}", s.metrics.line());
}

async fn s4_interrupt() {
    println!("\nS4 打断（两段式 + 收尾 + workspace 保留）");
    let model = Arc::new(
        MockModel::new()
            .on_judge(judged(
                "none",
                vec![Op::grounded(
                    "spec.baseline",
                    "FlyGCL 原版",
                    Source::Repo("configs/flygcl.yaml:12".into()),
                    0.9,
                )],
            ))
            .on_answer(vec![
                StreamEvent::Chunk("我先说第一点".into()),
                StreamEvent::Chunk("……第二点".into()),
                StreamEvent::Chunk("……第三点".into()),
                StreamEvent::Done(Usage { prompt: 500, completion: 80, estimated: false }),
            ])
            .chunk_delay(Duration::from_millis(60)),
    );
    let mut r = rig(model, default_registry(), RigOpts::default());
    r.handle.user_input("讲讲这个设计", SendMode::Queue).await;

    let mut turn = None;
    r.wait(|e| {
        if let UiEvent::TurnStarted { turn: t } = e {
            turn = Some(*t);
        }
        matches!(e, UiEvent::Delta(_))
    })
    .await;
    let turn = turn.expect("没拿到 TurnStarted");
    r.handle.interrupt(turn).await;

    check("① Stopping 立刻发", r.wait(|e| matches!(e, UiEvent::Stopping { .. })).await);
    check(
        "② TurnClosed{aborted} 等收敛完再发",
        r.wait(|e| matches!(e, UiEvent::TurnClosed { aborted: true, .. })).await,
    );

    let (s, audit) = r.finish().await;
    check(
        "打断之后 workspace 保留：pending 照样提交",
        s.ws.fields.contains_key(&Path::new("spec.baseline")),
    );
    let f = s.ws.fields.get(&Path::new("spec.baseline")).unwrap();
    check("有实证来源的推断不画虚线", !f.is_dashed());
    check("被打断的一轮也写了历史", audit.iter().any(|x| x.starts_with("history")));
    check("打断有估算记账兜底", s.cost.estimated_entries >= 1);
    check("turns_aborted 计到了", s.metrics.turns_aborted == 1);
    println!("    {}", s.metrics.line());
    println!("    {}", s.cost.line());
}

async fn s5_injection() {
    println!("\nS5 排队注入（turn 运行中插话，检查点吸收）");
    let model = Arc::new(MockModel::new().judge_delay(Duration::from_millis(120)));
    let mut r = rig(model, default_registry(), RigOpts::default());
    r.handle.user_input("第一句", SendMode::Queue).await;
    r.wait(|e| matches!(e, UiEvent::TurnStarted { .. })).await;
    r.handle.user_input("补充：只在 CIFAR 上做", SendMode::Queue).await;
    check("运行中的输入被排队而不是丢弃", r.wait(|e| matches!(e, UiEvent::Queued { .. })).await);
    check("turn 收敛", r.wait(|e| matches!(e, UiEvent::TurnClosed { .. })).await);

    let (s, _) = r.finish().await;
    check("插话被吸收（inbox_taken ≥ 2）", s.metrics.inbox_taken >= 2);
    check("只开了一轮", s.metrics.turns_started == 1);
    println!("    {}", s.metrics.line());
}

async fn s6_stale() {
    println!("\nS6 写权冲突（模型基于旧快照 → Stale → 回喂）");
    let model = Arc::new(
        MockModel::new()
            .on_judge(judged("none", vec![Op::set("spec.claim", "模型写的")]))
            .judge_delay(Duration::from_millis(150)),
    );
    let mut r = rig(model.clone(), default_registry(), RigOpts::default());
    r.handle.user_input("开始", SendMode::Queue).await;
    r.wait(|e| matches!(e, UiEvent::TurnStarted { .. })).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    r.handle.user_edit(Version::ZERO, vec![Op::set("spec.claim", "用户写的")]).await;
    check("turn 收敛", r.wait(|e| matches!(e, UiEvent::TurnClosed { .. })).await);

    let (s, _) = r.finish().await;
    check("模型的同路径改动被判 Stale", s.metrics.ops_rejected_stale == 1);
    let f = s.ws.fields.get(&Path::new("spec.claim")).unwrap();
    check("用户的值胜出", f.value.as_str() == Some("用户写的"));
    check("胜出值的出处是 User", f.origin == Origin::User);
    check("用户没标来源时记成用户口述", f.source == Some(Source::User));
    check("Stale 回喂进了本轮回答段 prompt", model.answer_prompt(0).contains("没有生效"));
    println!("    {}", s.metrics.line());
}

async fn s7_tool_failure_no_bother() {
    println!("\nS7 工具失败不打扰用户（返回给模型，让它应对）");
    let model = Arc::new(
        MockModel::new()
            .on_judge(judged("none", vec![]))
            .on_answer(vec![
                StreamEvent::Chunk("我查一下。".into()),
                StreamEvent::ToolCalls(vec![call("f1", "flaky"), call("f2", "nonexistent")]),
            ])
            .on_judge(judged("none", vec![]))
            .on_answer(vec![
                StreamEvent::Chunk("工具挂了，我换个说法。".into()),
                StreamEvent::Done(Usage { prompt: 300, completion: 30, estimated: false }),
            ]),
    );
    let reg = default_registry().with(Arc::new(FlakyTool));
    let mut r = rig(model.clone(), reg, RigOpts::default());
    r.handle.user_input("查一下仓库", SendMode::Queue).await;
    check(
        "turn 正常收敛（工具失败没有中断它）",
        r.wait(|e| matches!(e, UiEvent::TurnClosed { aborted: false, .. })).await,
    );
    check("没有因为工具失败去打扰用户", r.count(|e| matches!(e, UiEvent::Choice { .. })) == 0);
    let ap = model.answer_prompt(1);
    check("失败作为结果回给了模型", ap.contains("[failed]"));
    check("不存在的工具名也回给了模型", ap.contains("[not_found]"));
    check("模型据此调整了说法", model.answer_calls() == 2);
    let (s, _) = r.finish().await;
    println!("    {}", s.metrics.line());
}

async fn s8_dedupe() {
    println!("\nS8 去重（用户手抖点两次）");
    let model = Arc::new(MockModel::new().judge_delay(Duration::from_millis(60)));
    let mut r = rig(model, default_registry(), RigOpts::default());
    let id = r.handle.user_input("同一句话", SendMode::Queue).await.unwrap();
    r.handle.user_input_with_id(id, "同一句话", SendMode::Queue).await;
    check("turn 收敛", r.wait(|e| matches!(e, UiEvent::TurnClosed { .. })).await);
    let (s, _) = r.finish().await;
    check("重复的 client_id 被丢弃", s.metrics.deduped_inputs == 1);
    check("只开一轮", s.metrics.turns_started == 1);
    println!("    {}", s.metrics.line());
}

async fn s9_edit_not_pushed_into_turn() {
    println!("\nS9 用户轮中编辑不进当前 turn，下一轮才带上");
    let model = Arc::new(MockModel::new().judge_delay(Duration::from_millis(140)));
    let mut r = rig(model.clone(), default_registry(), RigOpts::default());
    r.handle.user_input("第一轮", SendMode::Queue).await;
    r.wait(|e| matches!(e, UiEvent::TurnStarted { .. })).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    r.handle.user_edit(Version::ZERO, vec![Op::set("spec.dataset", "只用 CIFAR100")]).await;
    check("第一轮收敛", r.wait(|e| matches!(e, UiEvent::TurnClosed { .. })).await);

    check("用户编辑立即生效并推给了 UI", r.count(|e| matches!(e, UiEvent::StateChanged(_))) == 1);
    check(
        "本轮的回答段 prompt 里没有它（没有追着用户的改动跑）",
        !model.answer_prompt(0).contains("只用 CIFAR100"),
    );

    r.handle.user_input("第二轮", SendMode::Queue).await;
    check("第二轮收敛", r.wait(|e| matches!(e, UiEvent::TurnClosed { .. })).await);
    check("第二轮的 prompt 里带上了用户的修改", model.judge_prompt(1).contains("只用 CIFAR100"));

    let (s, _) = r.finish().await;
    check("用户编辑立即落盘了（不等 turn）", s.ws.fields.contains_key(&Path::new("spec.dataset")));
    println!("    {}", s.metrics.line());
}

async fn s10_context_layering() {
    println!("\nS10 上下文分层（两段吃同一份对话；guidance 与案例只给回答段）");
    let model = Arc::new(
        MockModel::new()
            .on_judge(judged("none", vec![]))
            .on_judge(judged("clarify_goal", vec![])),
    );
    let mut r = rig(model.clone(), default_registry(), RigOpts::default());
    r.handle.user_input("第一轮说的话-AAA", SendMode::Queue).await;
    r.wait(|e| matches!(e, UiEvent::TurnClosed { .. })).await;
    r.handle.user_input("第二轮说的话-BBB", SendMode::Queue).await;
    r.wait(|e| matches!(e, UiEvent::TurnClosed { .. })).await;

    let jp = model.judge_prompt(1);
    let ap = model.answer_prompt(1);
    check("判断段带了场景目录", jp.contains("可选的协作场景"));
    check("判断段带了最新一条用户输入", jp.contains("BBB"));
    check("判断段也带了更早的对话（判断需要 history）", jp.contains("AAA"));
    check("判断段**不含**任何场景的 guidance", !jp.contains("用户的目标还没落定"));
    check("回答段含选中场景的 guidance", ap.contains("用户的目标还没落定"));
    println!("    判断段 {} 字符 / 回答段 {} 字符", jp.chars().count(), ap.chars().count());
    check("判断段仍然比回答段便宜", jp.chars().count() < ap.chars().count());

    let (s, _) = r.finish().await;
    println!("    {}", s.cost.line());
}

async fn s11_compaction() {
    println!("\nS11 上下文压缩（接近上限才折叠，折叠后摘要留在上下文里）");
    let model = Arc::new(MockModel::new());
    // 把上限压到很小，第二三轮就会触发
    let o = RigOpts {
        context_limit: ContextLimit {
            max_prompt_tokens: 260,
            trigger_at: 0.8,
            keep_recent_turns: 1,
        },
        ..RigOpts::default()
    };
    let mut r = rig(model.clone(), default_registry(), o);
    for i in 1..=4 {
        r.handle.user_input(format!("第 {i} 轮：{}", "内容".repeat(40)), SendMode::Queue).await;
        r.wait(|e| matches!(e, UiEvent::TurnClosed { .. })).await;
    }
    check("触发了折叠", r.count(|e| matches!(e, UiEvent::Compacted { .. })) >= 1);
    let shrank = r.log.iter().any(|e| {
        matches!(e, UiEvent::Compacted { before_tokens, after_tokens, .. } if after_tokens < before_tokens)
    });
    check("折叠之后上下文确实变小了", shrank);

    let no_choice = r.count(|e| matches!(e, UiEvent::Choice { .. })) == 0;
    let (s, _) = r.finish().await;
    check("折叠计到了指标里", s.metrics.compactions >= 1 && s.metrics.msgs_folded > 0);
    check(
        "摘要作为一条消息留在了历史里",
        s.history.iter().any(|m| m.content.contains("已折叠的早期对话摘要")),
    );
    check("折叠是自动的，没有弹窗问用户", no_choice);
    println!("    {}", s.metrics.line());
    println!("    {}", s.cost.line());
}

async fn s12_distill() {
    println!("\nS12 一键蒸馏（用户操作触发，产出草稿而不是直接覆盖）");
    let dir = std::env::temp_dir().join("premortem-distill-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let model = Arc::new(
        MockModel::new()
            // ① 正常那一轮的回答
            .on_answer(vec![
                StreamEvent::Chunk("聊完了。".into()),
                StreamEvent::Done(Usage { prompt: 300, completion: 20, estimated: false }),
            ])
            // ② 蒸馏走的是同一个 stream 接口（complete 把流收干）
            .on_answer(vec![
                StreamEvent::Chunk("## 项目\n蒸馏出来的草稿".into()),
                StreamEvent::Done(Usage { prompt: 900, completion: 120, estimated: false }),
            ]),
    );
    let o = RigOpts { memory_dir: Some(dir.clone()), ..RigOpts::default() };
    let mut r = rig(model.clone(), default_registry(), o);
    r.handle.user_input("聊两句", SendMode::Queue).await;
    r.wait(|e| matches!(e, UiEvent::TurnClosed { .. })).await;

    r.handle.distill().await;
    check("蒸馏完成并给出草稿路径", r.wait(|e| matches!(e, UiEvent::Distilled { .. })).await);
    let draft = r.log.iter().find_map(|e| match e {
        UiEvent::Distilled { draft } => Some(draft.clone()),
        _ => None,
    });
    let ok = draft.as_ref().map(|p| std::path::Path::new(p).exists()).unwrap_or(false);
    check("草稿文件真的写出来了", ok);
    if let Some(p) = &draft {
        let body = std::fs::read_to_string(p).unwrap_or_default();
        check("草稿内容是模型产出的", body.contains("蒸馏出来的草稿"));
        check("是草稿文件，不是直接覆盖持久层", p.contains("draft-"));
    }

    let no_choice = r.count(|e| matches!(e, UiEvent::Choice { .. })) == 0;
    let (s, _) = r.finish().await;
    check("蒸馏过程没有打扰用户", no_choice);
    check("蒸馏计到了指标里", s.metrics.distills == 1);
    check("蒸馏的开销记在 subagent 角色下", s.cost.of(Role::Subagent).total() > 0);
    let _ = std::fs::remove_dir_all(&dir);
    println!("    {}", s.metrics.line());
    println!("    {}", s.cost.line());
}

async fn s13_memory_bootstrap() {
    println!("\nS13 持久层 bootstrap（落成用户可直接编辑的文件）");
    let dir = std::env::temp_dir().join("premortem-memory-test");
    let _ = std::fs::remove_dir_all(&dir);

    let m = Memory::load_or_bootstrap(&dir).await;
    for f in ["project.md", "preferences.md", "knowledge.md", "playbook.toml"] {
        check(&format!("写出了 {f}"), dir.join(f).exists());
    }
    check("cases/ 目录建好了", dir.join("cases").is_dir());
    check("内置场景都在", m.playbook.scenes.len() >= 8);

    // 用户改一行，重新加载应该读到改过的版本
    let p = dir.join("project.md");
    std::fs::write(&p, "# 项目\n我自己写的项目说明\n").unwrap();
    let m2 = Memory::load_or_bootstrap(&dir).await;
    check("用户改过的内容会被读回（文件就是界面）", m2.project.contains("我自己写的"));
    check("持久层进了 prompt", m2.prompt_block().contains("我自己写的"));

    // playbook.toml 能往返
    let toml = std::fs::read_to_string(dir.join("playbook.toml")).unwrap();
    let pb = Playbook::from_toml_str(&toml);
    check("playbook.toml 能被读回来", pb.as_ref().map(|p| p.scenes.len()) == Ok(m.playbook.scenes.len()));
    check("缺 none 场景会被拒绝", Playbook::from_toml_str("default_tools = []").is_err());

    let _ = std::fs::remove_dir_all(&dir);
}

fn print_footprint_note() {
    let f = Footprint { cacheable: 1200, inference: 300, history: 500, turns: 4 };
    println!("\n上下文占用示例：{}", f.line());
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> std::process::ExitCode {
    println!("=== Premortem core 链路测试 ===");
    s1_normal_turn().await;
    s2_scene_injection().await;
    s2b_model_asks().await;
    s3_heartbeat().await;
    s3b_hard_limit().await;
    s4_interrupt().await;
    s5_injection().await;
    s6_stale().await;
    s7_tool_failure_no_bother().await;
    s8_dedupe().await;
    s9_edit_not_pushed_into_turn().await;
    s10_context_layering().await;
    s11_compaction().await;
    s12_distill().await;
    s13_memory_bootstrap().await;
    print_footprint_note();

    let f = FAILS.load(Ordering::Relaxed);
    println!(
        "\n=== 结果：{} ===",
        if f == 0 { "全部通过".to_string() } else { format!("{f} 项失败") }
    );
    if f == 0 { std::process::ExitCode::SUCCESS } else { std::process::ExitCode::FAILURE }
}
