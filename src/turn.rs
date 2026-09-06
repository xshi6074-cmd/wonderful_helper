//! `run_turn`：一轮的**全部**控制流。
//!
//! ```text
//! 重读持久层        ── 用户改了 memory/*，这一轮就生效
//! 取一次视图        ── mode 与 scene_override（本轮一次性的东西）
//! 接近上下文上限 ⇒ 折叠早期对话（落成一条 Folded 事件，原文不删）
//!
//! 判断段            ── ★ 一轮**只跑一次**，在回答循环之外
//!   取视图 ①        ── 拼判断段的 prompt
//!   判定场景        ── 只判场景。不写推断、不做检索
//!   落 Judged + Cost
//!
//! 'turn: loop {                       ← 回答循环。判断段不在里面
//!     检查点        ── 吸收中途插话（只拿计数，内容从视图来）
//!     取视图        ── ★ 每次重取：上一圈工具刚写进去的推断要看得见
//!     回答段        ── 注入 scene.guidance + 案例 + 本 mode 的工具，然后流式
//!       ├─ ToolCalls ⇒ 发 Called → 跑工具 → 发 Returned ⇒ continue
//!       └─ Done      ⇒ 发 Wrote ⇒ break
//! }
//! ```
//!
//! # 判断和推断是两回事
//!
//! **判断段只回答「这一轮该进哪个场景」**，它跑在回答段之前、在模型读任何材料之前，
//! 所以一轮跑一次就够了。上一版把它放在循环里，工具每返回一次就重判一次场景 ——
//! 三轮工具就是四次判断段，而判断段吃的是和回答段一模一样的完整对话，
//! 等于把最贵的那段 prompt 发了四遍，换来的只是一个几乎不会变的场景 id。
//!
//! **推断是读完材料之后才形成的东西**，所以它在回答段，而且是模型自己调的工具：
//! `record_graph` 写图，`record_note` 写图外的关键信息（见 [`crate::actions`]）。
//! 理想顺序写在那两个工具的 description 里 —— 先读（`fs_*` / `web_*`），
//! 再写推断，最后写正文。写在 description 里而不是 harness 里，是这个项目一贯的做法。
//!
//! # 中途插话不重判场景
//!
//! 插话在用户按下发送时就进了主时间线，下一次取视图自然带上，回答段看得见。
//! 为它重跑一次判断段是同一笔钱买同一个答案。
//!
//! # turn 不持有任何状态副本
//!
//! **每次要拼 prompt 就向 Core 取一次当前视图**（[`CoreHandle::view`]），不缓存、
//! 不冻结、不自己在 turn 里攒一份平行的时间线。[`crate::msg::Injected`] 只返回计数，
//! 结构上就不可能再把插话拼进局部副本。
//!
//! # harness 在这里**不**做什么
//!
//! 判断段只产出一个**场景判定**，harness 拿它做三件准备 —— 注入 guidance、
//! 注入案例、暴露这个场景与这个 mode 的工具 —— 然后就不再干预。
//!
//! | 位置 | 判断 | 现状 |
//! |---|---|---|
//! | harness 抢占：判断段说要问就掐断回答段 | 不该有 | **已删** |
//! | 模型拒绝推进阶段 | 不该有 | **已删**。闸门本来就在用户手里 |
//! | 工具失败 / 超时 → 中断 turn 问用户 | 不该有 | 作为结果返回给模型 |
//! | 判断段失败 → 阻断回答 | 不该有 | 降级为「本轮无场景」并在时间线上留明账 |
//! | 上下文压缩 → 弹窗请示 | 不该有 | 自动折叠 + 事后告知 |
//! | 落盘失败 → 弹窗 | 不该有 | 状态栏降级提示，对话继续 |
//! | 蒸馏写入长期记忆 | **反过来** | 只能用户按一键蒸馏触发，且写的是草稿 |
//! | 模型调用 `ask_user` | **该有** | 保留，且提问是持久实体 |
//! | 心跳 `StillRunning` | 不是打扰 | 保留。这是进度反馈 |

use crate::actions;
use crate::context::{CompactPlan, Context, ContextLimit, fold_instruction, plan_compaction};
use crate::event::assemble;
use crate::handle::CoreHandle;
use crate::ids::TurnId;
use crate::memory::{Memory, ModePrompt};
use crate::model::{
    AnswerReq, Call, JudgeReq, Message, Models, Role, StreamEvent, Usage, complete,
};
use crate::msg::{Emit, TurnOutcome, TurnStats, TurnView, UiEvent};
use crate::scene::SceneId;
use crate::state::Op;
use crate::tools::{ASK_USER, AskUser, Registry, ToolConfig, ToolCtx, ToolResult, run_tools};
use futures_util::StreamExt;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Instant;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// turn task 跑起来需要的一切。由 Core 在 `start_turn` 里装配。
pub struct TurnCtx {
    pub id: TurnId,
    /// 本轮的父 token。子任务一律拿 `child_token()`，
    /// 打断一个 turn 就是 cancel 这个父 token，整棵收敛。
    pub token: CancellationToken,
    pub core: CoreHandle,
    pub ui: broadcast::Sender<UiEvent>,
    /// 判断 / 回答 / subagent 三个角色分别可配（R3）。
    pub models: Arc<Models>,
    pub registry: Arc<Registry>,
    /// 持久层目录。**每轮重读**，None 则用 `memory`。
    pub memory_dir: Option<PathBuf>,
    /// 没有目录时的兜底持久层。
    pub memory: Arc<Memory>,
    pub tool_config: ToolConfig,
    pub context_limit: ContextLimit,
    /// TaskId 分配器，与 Core 共享。
    pub next_task: Arc<AtomicU64>,
}

impl TurnCtx {
    fn tool_ctx(&self) -> ToolCtx {
        ToolCtx {
            token: self.token.clone(),
            ui: self.ui.clone(),
            registry: self.registry.clone(),
            config: self.tool_config,
            next_task: self.next_task.clone(),
        }
    }
}

/// 把一个长 await 包进取消保护。返回 `None` 表示被打断。
///
/// **每个长 await 都必须过这一层。** 否则 cancel 之后任务还在跑，钱照烧。
/// `biased` 让取消分支永远先被轮询：用户点了停，不该因为模型刚好也就绪了而被无视。
pub async fn guarded<T>(token: &CancellationToken, fut: impl Future<Output = T>) -> Option<T> {
    tokio::select! {
        biased;
        _ = token.cancelled() => None,
        r = fut => Some(r),
    }
}

/// 一轮最多注入几个场景的 guidance。
///
/// 判断段一次判出五个场景时，回答段的 prompt 会被 guidance 淹掉 ——
/// 而「什么都强调」等于「什么都没强调」。三个是让「目标没说清 + 预算对不上 +
/// 该收尾了」这类真实组合装得下，同时挡住摊大饼。
pub const MAX_SCENES: usize = 3;

/// 拼 prompt 用的上下文。**每次都从当前视图现拼。**
fn ctx_of(v: &TurnView, memory: &Memory, mode_note: &str) -> Context {
    Context::build(&v.ws, v.turn_start, memory, assemble(&v.events), mode_note)
}

pub async fn run_turn(ctx: TurnCtx) -> TurnOutcome {
    let mut stats = TurnStats::default();
    let mut aborted = false;
    let mut partial = String::new();
    // 打断兜底记账用：最后一次发出去的 prompt 有多大。钱已经花了，账不能漏。
    // 不给初值：判断段一定会在任何 return 之前写它，编译器替我们盯着这一条。
    let mut last_prompt_tokens: u32;

    // ── 每轮重读持久层 ────────────────────────────────────────
    // 「metadata 动态拼接，改动立即生效」的落点：用户改了 project.md、
    // playbook.toml 或 prompts.toml，这一轮就是新的，不用重启。
    let memory: Arc<Memory> = match &ctx.memory_dir {
        Some(d) => Arc::new(Memory::load_or_bootstrap(d).await),
        None => ctx.memory.clone(),
    };
    // 改坏了要像界面报错一样报出来，不能只 eprintln 然后静悄悄用内置内容。
    for (file, err) in &memory.warnings {
        let _ = ctx.ui.send(UiEvent::MemoryDegraded { file: file.clone(), err: err.clone() });
    }

    // ── 取第一次视图：只为拿本轮一次性的东西 ────────────────────
    let Some(v0) = ctx.core.view(ctx.id).await else {
        return TurnOutcome { aborted: true, stats };
    };
    let mode = v0.mode;
    // scene_override 是一次性的：Core 在第一次取视图时就 take 掉了。
    let scene_override: Option<Vec<SceneId>> = v0.scene_override.clone();
    // 这个 mode 的全套提示词：system 段那一句、额外暴露的工具、
    // 以及每个工具在这个 mode 下要追加的说明。全部来自 prompts.toml。
    let mp: ModePrompt = memory.mode(mode);

    // ── 接近上下文上限 ⇒ 先折叠早期对话 ────────────────────────
    //
    // 位置很重要：**判断段之前、上一轮的推断已提交之后**。那时值得留下的结构化
    // 信息已经在图里，被折叠的是叙述过程。折的是事件区间，原文一条不删。
    {
        let probe = ctx_of(&v0, &memory, &mp.note);
        let fp = probe.footprint();
        if let CompactPlan::Fold { from, to, msgs, count } =
            plan_compaction(&v0.events, fp.cacheable + fp.inference, &ctx.context_limit)
        {
            let mut ask =
                vec![Message::system(fold_instruction(&memory.prompts.fold, &probe.inference))];
            ask.extend(msgs);
            let fut = complete(ctx.models.subagent.as_ref(), ask, ctx.token.child_token());
            match guarded(&ctx.token, fut).await {
                None => return TurnOutcome { aborted: true, stats },
                Some(Ok((summary, usage))) => {
                    ctx.core.cost(ctx.id, Role::Subagent, usage).await;
                    ctx.core.emit(ctx.id, Emit::Folded { from, to, summary, folded: count }).await;
                    stats.compacted = count;
                }
                // 摘要失败就带着超长上下文继续 —— 折叠失败不该让用户这一轮用不了。
                Some(Err(e)) => eprintln!("[turn] 折叠失败，本轮带全量上下文继续: {e}"),
            }
        }
    }
    drop(v0);

    // ══════════ 判断段：一轮一次，在回答循环之外 ══════════
    //
    // 判断段和回答段吃同一份对话 —— 判断「这一轮该进哪个场景」本来就要看
    // 用户刚说了什么。两段式省下的是场景 guidance 与案例。
    let scene_ids: Vec<SceneId>;
    {
        let Some(v) = ctx.core.view(ctx.id).await else {
            return TurnOutcome { aborted: true, stats };
        };
        let context = ctx_of(&v, &memory, &mp.note);
        drop(v);

        let t_judge = Instant::now();
        let judge_msgs = context.for_judge();
        last_prompt_tokens = ctx.models.judge.estimate_prompt_tokens(&judge_msgs);
        let req = JudgeReq {
            turn: ctx.id,
            mode,
            msgs: judge_msgs,
            token: ctx.token.child_token(),
        };
        let Some(judged) = guarded(&ctx.token, ctx.models.judge.judge(req)).await else {
            return TurnOutcome { aborted: true, stats };
        };
        stats.judge_ms += t_judge.elapsed().as_millis() as u64;

        // 判断段失败 ⇒ 降级为「本轮没有判出场景」，继续回答段。
        // 不因为一次 API 抖动就把整轮废掉，也不因此打扰用户。代价是要留明账。
        let judge = match judged {
            Ok(j) => j,
            Err(e) => {
                ctx.core
                    .emit(
                        ctx.id,
                        Emit::Noted { text: format!("[判断段失败，本轮未做场景判定] {e}") },
                    )
                    .await;
                crate::model::JudgeOut {
                    scenes: vec!["none".into()],
                    rationale: format!("判断段失败：{e}"),
                    usage: Usage::default(),
                }
            }
        };

        // 场景：用户手动选过 ⇒ 以他的为准。**并且真的注入这些场景的材料** ——
        // 只换标签不换 prompt，等于用户选了半天模型什么都没感觉到。
        let mut ids: Vec<SceneId> = match &scene_override {
            Some(s) => {
                stats.scene_overridden = true;
                s.clone()
            }
            None => judge.scenes.clone(),
        };
        ids.retain(|id| !mode.disabled_scenes().contains(&id.as_str()));
        let (resolved, unknown) = memory.playbook.resolve(&ids);
        if !unknown.is_empty() {
            // 模型给了不认识的 id ⇒ 丢掉，不报错、不打扰用户，但记进指标
            stats.scene_unknown = true;
        }
        // 一组场景 = 一组 guidance 全都进 prompt，所以要有上限：
        // 判断段判出五个场景时，回答段的 prompt 会被 guidance 淹掉，
        // 而那时候「什么都强调」等于「什么都没强调」。
        let mut ids: Vec<SceneId> = resolved.iter().map(|s| s.id.clone()).collect();
        ids.truncate(MAX_SCENES);
        // 一个都不剩 ⇒ 回退到 none。playbook 保证它存在。
        if ids.is_empty() {
            ids.push("none".into());
        }
        stats.scenes = ids.clone();
        scene_ids = ids;
        ctx.core
            .emit(
                ctx.id,
                Emit::Judged { scenes: scene_ids.clone(), rationale: judge.rationale.clone() },
            )
            .await;
        // 账记在判定之后：时间线读起来是「判成了 X，为此花了 Y」。
        ctx.core.cost(ctx.id, Role::Judge, judge.usage).await;
    }

    let (scenes, _) = memory.playbook.resolve(&scene_ids);
    let cases = memory.cases_for(&scene_ids);

    // 本轮暴露的工具：场景的 + 这个 mode 额外给的。
    // 认不出的名字**要报出来** —— 上一版这里静默丢弃，结果整套 fs_* / web_*
    // 从来没被暴露过，而症状只是「模型好像从来不调工具」。
    let exposed_names = memory.playbook.exposed_tools(&scenes, &mp.tools);
    let exposed = ctx.registry.specs(&exposed_names, &mp.tool_notes);
    if !exposed.missing.is_empty() {
        let _ = ctx.ui.send(UiEvent::MemoryDegraded {
            file: "playbook.toml".into(),
            err: format!(
                "场景 {} / mode {} 里这些工具名注册表里没有，已跳过：{}。现有的是：{}",
                scene_ids.join("+"),
                mode.key(),
                exposed.missing.join("、"),
                ctx.registry.names().join("、")
            ),
        });
    }

    // ══════════ 回答循环 ══════════
    'turn: loop {
        stats.loops += 1;

        // ── 检查点：吸收中途插话 ────────────────────────────────
        // 只拿计数，**不重判场景**。那些事件在用户按下发送时就进时间线了，
        // 下面取视图自然就看到。
        let Some(inj) = ctx.core.take_injections(ctx.id).await else {
            aborted = true;
            break 'turn;
        };
        stats.injections += inj.said;
        stats.answers += inj.answered;

        // ── 取视图：★ 每圈重取 ──────────────────────────────────
        // 上一圈 record_graph / record_note 刚写进去的推断、刚回喂的工具结果，
        // 都要在这一圈的 prompt 里看得见。
        let Some(v) = ctx.core.view(ctx.id).await else {
            aborted = true;
            break 'turn;
        };
        let context = ctx_of(&v, &memory, &mp.note);
        drop(v);

        // ── 回答段 ─────────────────────────────────────────────
        let t_answer = Instant::now();
        let answer_msgs = context.for_answer(&scenes, &cases, &[]);
        last_prompt_tokens = ctx.models.answer.estimate_prompt_tokens(&answer_msgs);
        let req = AnswerReq {
            turn: ctx.id,
            mode,
            msgs: answer_msgs,
            tools: exposed.specs.clone(),
            token: ctx.token.child_token(),
        };
        let Some(opened) = guarded(&ctx.token, ctx.models.answer.stream(req)).await else {
            aborted = true;
            break 'turn;
        };
        let mut stream = match opened {
            Ok(s) => s,
            Err(e) => {
                ctx.core.emit(ctx.id, Emit::Noted { text: format!("[回答段失败] {e}") }).await;
                stats.answer_ms += t_answer.elapsed().as_millis() as u64;
                break 'turn;
            }
        };
        partial.clear();

        loop {
            tokio::select! {
                biased;
                _ = ctx.token.cancelled() => {
                    aborted = true;
                    stats.answer_ms += t_answer.elapsed().as_millis() as u64;
                    break 'turn;
                }
                ev = stream.next() => match ev {
                    // 流没给 Done 就结束了：当作一次失败的回答，但已收到的正文要留下
                    None => {
                        stats.answer_ms += t_answer.elapsed().as_millis() as u64;
                        if !partial.is_empty() {
                            ctx.core.emit(ctx.id, Emit::Wrote {
                                text: std::mem::take(&mut partial), interrupted: false }).await;
                        }
                        break 'turn;
                    }
                    Some(StreamEvent::Chunk(d)) => {
                        partial.push_str(&d);
                        // 流式 chunk 直发 UI，不经 Core、不进时间线 ——
                        // 整段完成后由一条 Wrote 落盘，没必要一个字一条事件。
                        let _ = ctx.ui.send(UiEvent::Delta { turn: ctx.id, text: d });
                    }
                    Some(StreamEvent::ToolCalls(cs)) => {
                        // 模型自己决定调什么、调几个。harness 只负责跑和保序。
                        //
                        // **发出即落盘**：Called 先进时间线，Core 记下 call_id → seq。
                        // 这样「发出了没返回」是可观测的，被打断/被 kill 都能补上 Aborted。
                        ctx.core.emit(ctx.id, Emit::Called {
                            text: std::mem::take(&mut partial), calls: cs.clone() }).await;
                        // 模型选择用选择题提问 ⇒ 提问本身是持久实体，落一条 Asked。
                        // 界面刷新、turn 结束、进程重启，那道题都还在。
                        for c in cs.iter().filter(|c| c.name == ASK_USER) {
                            if let Some((question, options)) = AskUser::parse(c) {
                                stats.asked_user += 1;
                                ctx.core.emit(ctx.id, Emit::Asked {
                                    call_id: c.id.clone(), question, options }).await;
                            }
                        }

                        let Some(rs) = run_batch(&ctx, cs, &mut stats).await else {
                            aborted = true;
                            stats.answer_ms += t_answer.elapsed().as_millis() as u64;
                            break 'turn;
                        };
                        for r in &rs {
                            ctx.core.emit(ctx.id, Emit::Returned {
                                call_id: r.call_id.clone(),
                                name: r.name.clone(),
                                content: r.content.clone(),
                                outcome: r.kind.tag().to_string(),
                                task: r.task,
                            }).await;
                        }
                        stats.answer_ms += t_answer.elapsed().as_millis() as u64;
                        if ctx.token.is_cancelled() { aborted = true; break 'turn; }
                        continue 'turn; // 带结果再问模型，顺便重过检查点
                    }
                    Some(StreamEvent::Done(usage)) => {
                        ctx.core.cost(ctx.id, Role::Answer, usage).await;
                        ctx.core.emit(ctx.id, Emit::Wrote {
                            text: std::mem::take(&mut partial), interrupted: false }).await;
                        stats.answer_ms += t_answer.elapsed().as_millis() as u64;
                        break 'turn;
                    }
                    Some(StreamEvent::Failed(e)) => {
                        ctx.core.emit(ctx.id, Emit::Noted {
                            text: format!("[流中断] {e}") }).await;
                        if !partial.is_empty() {
                            ctx.core.emit(ctx.id, Emit::Wrote {
                                text: std::mem::take(&mut partial), interrupted: true }).await;
                        }
                        stats.answer_ms += t_answer.elapsed().as_millis() as u64;
                        break 'turn;
                    }
                }
            }
        }
    }

    if aborted {
        // 半截正文要落下来。不落，历史里就是「用户问了、助手没答」，
        // 模型下一轮的行为会很怪。
        if !partial.trim().is_empty() {
            ctx.core.emit(ctx.id, Emit::Wrote { text: partial.clone(), interrupted: true }).await;
        }
        // 流式中断拿不到 usage，但 prompt 已经发出去了，钱已经花了。
        let est = Usage::estimate(last_prompt_tokens, partial.chars().count());
        ctx.core.cost(ctx.id, Role::Answer, est).await;
        // 未闭合的工具调用由 Core 在 Finished 里统一补 Aborted ——
        // 它才是唯一知道「哪些调用发出去了」的地方（open_calls 是它维护的），
        // 而且被 kill 时走的也是同一段逻辑（恢复时补）。
    }

    TurnOutcome { aborted, stats }
}

/// 跑一批工具调用，**按原顺序**返回结果。
///
/// # 动作工具在这里就地提交
///
/// `record_graph` / `record_note` 要写主时间线，而 `Tool::run` 只拿得到 call 和
/// token —— 握着 [`CoreHandle`] 的只有 turn 这一层。所以它们和 `ask_user` 落
/// `Asked` 事件一样，在这里特判。
///
/// **同一批调用合并成一次 `Emit::Infer`**：别名（`$enc`）的作用域就是一次提交，
/// 拆成两次，`record_note` 里 `anchor: "$enc"` 就指不到同一批 `record_graph`
/// 刚建的那个节点了。批 = 模型的一条 assistant 消息 = 一个别名作用域。
///
/// 返回 `None` = Core 没了（被打断或关闭）。
async fn run_batch(
    ctx: &TurnCtx,
    calls: Vec<Call>,
    stats: &mut TurnStats,
) -> Option<Vec<ToolResult>> {
    let n = calls.len();
    let mut slots: Vec<Option<ToolResult>> = (0..n).map(|_| None).collect();

    // ── 动作调用：解析 → 合并 → 一次提交 ──
    let act_idx: Vec<usize> =
        (0..n).filter(|&i| actions::is_action(&calls[i].name)).collect();
    if !act_idx.is_empty() {
        let mut ops: Vec<Op> = Vec::new();
        // (下标, 这次调用贡献了几条 op, 解析不了的, 放错工具的条数)
        let mut per: Vec<(usize, usize, Vec<String>, usize)> = Vec::new();
        for &i in &act_idx {
            let (o, bad) = actions::parse(&calls[i]);
            let want_graph = calls[i].name == actions::RECORD_GRAPH;
            let misplaced =
                o.iter().filter(|op| actions::is_graph_op(op) != want_graph).count();
            per.push((i, o.len(), bad, misplaced));
            ops.extend(o);
        }
        stats.inferred_ops += ops.len() as u32;
        // 动作调用也是模型发出的工具调用，要进 tools_run。不计的话，
        // 「模型调了两次 record_graph」在指标上仍然是 tools_run: 0。
        stats.tools_run += act_idx.len() as u32;
        let applied = ctx.core.emit(ctx.id, Emit::Infer { ops }).await?;
        stats.dropped_ops += applied.dropped.len() as u32;
        // 被丢的必须回喂，否则模型下一轮还会提交同样的改动，白花钱。
        // 回喂走工具结果，不再另发一条 Noted —— 工具结果本来就是给模型的回话。
        let feedback = applied.feedback();
        for (i, count, bad, misplaced) in per {
            let mut msg = if count == 0 {
                "一条改动都没提交。".to_string()
            } else {
                format!("提交了 {count} 条改动。")
            };
            if !bad.is_empty() {
                msg.push_str(&format!(
                    "\n有 {} 条解析不了、已丢弃：\n{}",
                    bad.len(),
                    bad.iter().map(|b| format!("- {b}")).collect::<Vec<_>>().join("\n")
                ));
            }
            if misplaced > 0 {
                let (here, there) = if calls[i].name == actions::RECORD_GRAPH {
                    ("record_graph", "record_note")
                } else {
                    ("record_note", "record_graph")
                };
                msg.push_str(&format!(
                    "\n其中 {misplaced} 条不是 {here} 该收的（已照样提交），下次放 {there}。"
                ));
            }
            if let Some(f) = &feedback {
                msg.push('\n');
                msg.push_str(f);
            }
            let kind = if count == 0 {
                stats.tools_failed += 1;
                crate::tools::ToolResultKind::Failed
            } else {
                crate::tools::ToolResultKind::Ok
            };
            slots[i] = Some(ToolResult::of(&calls[i], msg, kind));
        }
    }

    // ── 其余走正常的工具调度 ──
    let rest_idx: Vec<usize> = (0..n).filter(|i| slots[*i].is_none()).collect();
    if !rest_idx.is_empty() {
        let rest: Vec<Call> = rest_idx.iter().map(|&i| calls[i].clone()).collect();
        let (rs, ts) = run_tools(rest, &ctx.tool_ctx()).await;
        stats.absorb_tools(&ts);
        for (slot, r) in rest_idx.into_iter().zip(rs) {
            slots[slot] = Some(r);
        }
    }

    Some(slots.into_iter().flatten().collect())
}

impl TurnStats {
    fn absorb_tools(&mut self, ts: &crate::tools::ToolRunStats) {
        self.tools_run += ts.run;
        self.tools_timeout += ts.timeout;
        self.tools_interrupted += ts.interrupted;
        self.tools_failed += ts.failed;
        self.heartbeats += ts.heartbeats;
        self.tool_ms += ts.elapsed_ms;
    }
}
