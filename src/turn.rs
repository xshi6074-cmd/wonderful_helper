//! `run_turn`：一轮的**全部**控制流。
//!
//! ```text
//! 重读持久层        ── 用户改了 memory/*.md，这一轮就生效
//! 取一次视图        ── 只为拿 mode 与 scene_override（本轮一次性的东西）
//! 接近上下文上限 ⇒ 折叠早期对话（落成一条 Folded 事件，原文不删）
//! 'turn: loop {
//!     检查点 0  ── 吸收中途插话（只拿计数，内容从视图来）
//!     取视图 ①  ── 拼判断段的 prompt
//!     判断段    ── 判定场景 → 提交推断 → 为组装上下文做检索
//!     检查点 1  ── subagent 跑的这段时间里用户说话了吗 ⇒ continue（重走判断）
//!     取视图 ②  ── ★ 必须重取：判断段刚写进去的推断，回答段要看得见
//!     回答段    ── 注入 scene.guidance + 案例 + 暴露 scene.tools，然后流式
//!       ├─ ToolCalls ⇒ 发 Called → 跑工具 → 发 Returned ⇒ continue
//!       └─ Done      ⇒ 发 Wrote ⇒ break
//! }
//! ```
//!
//! # turn 不持有任何状态副本
//!
//! **每次要拼 prompt 就向 Core 取一次当前视图**（[`CoreHandle::view`]），不缓存、
//! 不冻结、不自己在 turn 里攒一份平行的时间线。
//!
//! 上一版这里错得很典型：`TurnInput` 轮初取一次就冻住，判断段的 `Emit::Infer`
//! 进了 Core 的 workspace，**回答段却还在用轮初那份** —— 判断段刚写下的结论，
//! 回答段看不见。同时 turn 又自己维护一个 `local: Vec<Event>` 把插话拼进去，
//! 而那些事件早就在主时间线上了。两处都是「同一份数据在同一时刻有两个值」。
//!
//! 现在的规则：**Workspace 是即时工作台，消费者拿到的永远是最新的那份。**
//! [`crate::msg::Injected`] 只返回计数，结构上就不可能再把插话拼进局部副本。
//!
//! 这**不等于**「用户编辑会重新驱动 turn」：用户中途改侧栏不会打断、不会让 turn
//! 回退重跑，流程上它只是进 `turn_edits` 参与仲裁。但下一次拼 prompt 时，
//! 图上就是他改过的值 —— 拼 prompt 用的是拼那一刻的真实状态。
//!
//! # harness 在这里**不**做什么
//!
//! 判断段只产出一个**场景判定**，harness 拿它做三件准备 —— 注入 guidance、
//! 注入案例、暴露这个场景的工具 —— 然后就不再干预。模型想提问就自己调 `ask_user`，
//! 想读仓库就自己调 `read_repo`，也可以什么都不调直接回答。
//!
//! # 哪些「打扰用户」的权限被掐掉了
//!
//! | 位置 | 判断 | 现状 |
//! |---|---|---|
//! | harness 抢占：判断段说要问就掐断回答段 | 不该有 | **已删** |
//! | 模型拒绝推进阶段 | 不该有 | **已删**。闸门本来就在用户手里 |
//! | 工具失败 / 超时 → 中断 turn 问用户 | 不该有 | 作为结果返回给模型 |
//! | 判断段失败 → 阻断回答 | 不该有 | 降级为「本轮无场景」并在时间线上留明账 |
//! | 上下文压缩 → 弹窗请示 | 不该有 | 自动折叠 + 事后告知 |
//! | 落盘失败 → 弹窗 | 不该有 | 状态栏降级提示，对话继续 |
//! | 蒸馏写入长期记忆 | **反过来** | 只能用户按一键蒸馏触发 |
//! | 模型调用 `ask_user` | **该有** | 保留，且提问是持久实体 |
//! | 心跳 `StillRunning` | 不是打扰 | 保留。这是进度反馈 |

use crate::context::{CompactPlan, Context, ContextLimit, fold_instruction, plan_compaction};
use crate::event::assemble;
use crate::handle::CoreHandle;
use crate::ids::TurnId;
use crate::memory::Memory;
use crate::model::{AnswerReq, JudgeReq, Message, Models, Role, StreamEvent, Usage, complete};
use crate::msg::{Emit, TurnOutcome, TurnStats, TurnView, UiEvent};
use crate::scene::SceneId;
use crate::tools::{ASK_USER, AskUser, Registry, ToolConfig, ToolCtx, run_tools};
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

/// 拼 prompt 用的上下文。**每次都从当前视图现拼。**
fn ctx_of(v: &TurnView, memory: &Memory, mode_note: &str) -> Context {
    Context::build(&v.ws, v.turn_start, memory, assemble(&v.events), mode_note)
}

pub async fn run_turn(ctx: TurnCtx) -> TurnOutcome {
    let mut stats = TurnStats::default();
    let mut aborted = false;
    let mut partial = String::new();
    // 打断兜底记账用：最后一次发出去的 prompt 有多大。钱已经花了，账不能漏。
    let mut last_prompt_tokens: u32 = 0;

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
    let scene_override: Option<SceneId> = v0.scene_override.clone();
    let mode_note = memory.mode_note(mode).to_string();

    // ── 接近上下文上限 ⇒ 先折叠早期对话 ────────────────────────
    //
    // 位置很重要：**判断段之前、上一轮的推断已提交之后**。那时值得留下的结构化
    // 信息已经在图里，被折叠的是叙述过程。折的是事件区间，原文一条不删。
    {
        let probe = ctx_of(&v0, &memory, &mode_note);
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

    'turn: loop {
        stats.loops += 1;

        // ── 检查点 0：吸收中途插话 ──────────────────────────────
        // 只拿计数。内容不用管：那些事件在用户按下发送时就进时间线了，
        // 下面取视图自然就看到。
        let Some(inj) = ctx.core.take_injections(ctx.id).await else {
            aborted = true;
            break 'turn;
        };
        stats.injections += inj.said;
        stats.answers += inj.answered;

        // ── 取视图 ① ──────────────────────────────────────────
        let Some(v) = ctx.core.view(ctx.id).await else {
            aborted = true;
            break 'turn;
        };
        let context = ctx_of(&v, &memory, &mode_note);
        let phase = v.ws.phase;
        drop(v);

        // ── 判断段 ─────────────────────────────────────────────
        // 判断段和回答段吃同一份对话 —— 判断「这一轮该进哪个场景」本来就要看
        // 用户刚说了什么。两段式省下的是场景 guidance 与案例。
        let t_judge = Instant::now();
        let judge_msgs = context.for_judge();
        last_prompt_tokens = ctx.models.judge.estimate_prompt_tokens(&judge_msgs);
        let req = JudgeReq {
            turn: ctx.id,
            phase,
            mode,
            msgs: judge_msgs,
            token: ctx.token.child_token(),
        };
        let Some(judged) = guarded(&ctx.token, ctx.models.judge.judge(req)).await else {
            aborted = true;
            break 'turn;
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
                    scene: "none".into(),
                    rationale: format!("判断段失败：{e}"),
                    ops: vec![],
                    retrieve: vec![],
                    usage: Usage::default(),
                }
            }
        };

        // 场景：用户点过换场景 ⇒ 以他的为准。**并且真的注入新场景的材料** ——
        // 只换标签不换 prompt，等于用户点了半天模型什么都没感觉到。
        let mut scene_id: SceneId = match &scene_override {
            Some(s) => {
                stats.scene_overridden = true;
                s.clone()
            }
            None => judge.scene.clone(),
        };
        if mode.disabled_scenes().contains(&scene_id.as_str()) {
            scene_id = "none".into();
        }
        let scene = match memory.playbook.get(&scene_id) {
            Some(s) => s.clone(),
            None => {
                // 模型给了不认识的 id ⇒ 回退到 none，不报错、不打扰用户
                stats.scene_unknown = true;
                scene_id = "none".into();
                memory.playbook.get("none").cloned().expect("playbook 必须有 none 场景")
            }
        };
        stats.scene = scene_id.clone();
        ctx.core
            .emit(
                ctx.id,
                Emit::Judged { scene: scene_id.clone(), rationale: judge.rationale.clone() },
            )
            .await;
        // 账记在判定之后：时间线读起来是「判成了 X，为此花了 Y」。
        ctx.core.cost(ctx.id, Role::Judge, judge.usage).await;

        // 判断段顺带推断出的字段 → 交给 Core 仲裁。
        // 它们**当场**进 workspace，所以下面取视图 ② 时回答段就看得见 ——
        // 这正是上一版漏掉的那一条链。
        if !judge.ops.is_empty() {
            stats.inferred_ops += judge.ops.len() as u32;
            match ctx.core.emit(ctx.id, Emit::Infer { ops: judge.ops.clone() }).await {
                None => {
                    aborted = true;
                    break 'turn;
                }
                Some(a) => {
                    stats.dropped_ops += a.dropped.len() as u32;
                    // 被丢的必须回喂，否则模型下一轮还会提交同样的改动，白花钱
                    if let Some(fb) = a.feedback() {
                        ctx.core.emit(ctx.id, Emit::Noted { text: fb }).await;
                    }
                }
            }
        }

        // 判断段为**组装回答段上下文**而做的检索。
        // 取回来的是材料，模型在回答段依然可以自己再调工具。
        //
        // 落成一条 `Noted` 进时间线，而不是只在本轮内存里飘一下：不落的话，
        // 三十轮之后回头看这段对话，会看到一个凭空冒出来的结论。
        if !judge.retrieve.is_empty() {
            let (rs, ts) = run_tools(judge.retrieve.clone(), &ctx.tool_ctx()).await;
            stats.absorb_tools(&ts);
            let text = format!(
                "为本轮检索到的材料：\n{}",
                rs.iter()
                    .map(|r| format!("- [{}] {}", r.name, r.content))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            ctx.core.emit(ctx.id, Emit::Noted { text }).await;
            if ctx.token.is_cancelled() {
                aborted = true;
                break 'turn;
            }
        }

        // ── 检查点 1：subagent 跑的这段时间里用户说话了吗 ────────
        let Some(inj) = ctx.core.take_injections(ctx.id).await else {
            aborted = true;
            break 'turn;
        };
        if !inj.is_empty() {
            stats.injections += inj.said;
            stats.answers += inj.answered;
            continue 'turn; // 有新输入 ⇒ 重走判断，场景可能变了
        }

        // ── 取视图 ②：★ 必须重取 ────────────────────────────────
        // 判断段刚写进去的推断、刚回喂的 Noted、刚检索到的材料，都在取视图 ①
        // 之后才进的时间线与 workspace。拿视图 ① 拼回答段的 prompt 就是错的。
        let Some(v) = ctx.core.view(ctx.id).await else {
            aborted = true;
            break 'turn;
        };
        let context = ctx_of(&v, &memory, &mode_note);
        drop(v);

        // ── 回答段 ─────────────────────────────────────────────
        let t_answer = Instant::now();
        let exposed = memory.playbook.exposed_tools(&scene);
        let tools = ctx.registry.specs(&exposed);
        let cases = memory.cases_for(&scene_id);
        let answer_msgs = context.for_answer(&scene, &cases, &[]);
        last_prompt_tokens = ctx.models.answer.estimate_prompt_tokens(&answer_msgs);
        let req = AnswerReq {
            turn: ctx.id,
            mode,
            msgs: answer_msgs,
            tools,
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

                        let (rs, ts) = run_tools(cs, &ctx.tool_ctx()).await;
                        stats.absorb_tools(&ts);
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
