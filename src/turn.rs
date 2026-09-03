//! `run_turn`：一轮的**全部**控制流。
//!
//! ```text
//! 取一次快照（整轮就这一次）
//! 接近上下文上限 ⇒ 先折叠早期对话
//! 'turn: loop {
//!     检查点 0  ── 吸收中途插话
//!     判断段    ── 判定场景 → 提交推断 diff → 为组装上下文做检索
//!     检查点 1  ── subagent 跑的这段时间里用户说话了吗 ⇒ continue（重走判断）
//!     回答段    ── 注入 scene.guidance + 案例 + 暴露 scene.tools，然后流式
//!       ├─ ToolCalls ⇒ 跑工具 ⇒ continue（带结果再问，顺便重过检查点）
//!       └─ Done      ⇒ break
//! }
//! 打断收尾 close_open_calls
//! ```
//!
//! # harness 在这里**不**做什么
//!
//! 早先判断段吐一个 `Action`，harness 照着执行：`AskUser` 就掐断回答段、
//! `Block` 就拒绝推进、`Enumerate` 就强制列 N 条。那是拿状态机替模型做流程决策。
//!
//! 现在判断段只产出一个**场景判定**，harness 拿它做三件准备工作 —— 注入 guidance、
//! 注入案例、暴露这个场景下的工具 —— 然后就不再干预。模型想提问就自己调 `ask_user`，
//! 想读仓库就自己调 `read_repo`，也可以什么都不调直接回答。
//! **判断优先于回复** 的意思是判断段先跑、且它的结论决定回答段的 prompt，
//! 不是「判断可以掐断回复」。
//!
//! # 哪些「打扰用户」的权限被掐掉了
//!
//! 打扰预算整个删掉了 —— 用限额换安静，本质是在预算耗尽时用降低质量来减少打扰。
//! 正确的做法是把不该有的打扰权限逐个掐掉。清单：
//!
//! | 位置 | 判断 | 现状 |
//! |---|---|---|
//! | harness 抢占：判断段说要问就掐断回答段 | 不该有 | **已删**，模型在回答段自己决定 |
//! | `Action::Block`：模型拒绝推进阶段 | 不该有 | **已删**。阶段闸门本来就在用户手里 |
//! | mode 自动切换建议弹给用户 | 不该有 | **不实现**。mode 由用户显式切换 |
//! | 工具失败 / 超时 → 中断 turn 问用户 | 不该有 | 已经是对的：作为结果返回给模型 |
//! | 判断段失败 → 阻断回答 | 不该有 | 已经是对的：降级为「本轮无场景」并留明账 |
//! | 上下文压缩 → 弹窗请示 | 不该有 | 自动折叠 + 事后告知（`UiEvent::Compacted`） |
//! | 蒸馏写入长期记忆 | **反过来** | 只能用户按一键蒸馏触发；模型不能自己决定写进用户的记忆 |
//! | 模型调用 `ask_user` | **该有** | 保留。这是模型该完成的工作，不需要 harness 审批 |
//! | 心跳 `StillRunning` | 不是打扰 | 保留。这是进度反馈 |
//!
//! 掐干净之后就不需要限额了：模型选一个合适的做法本来就是它的工作，
//! 用户想干预随时可以打断或插话。
//!
//! # 用户编辑不进当前 turn
//!
//! 整轮只在开头取一次快照。用户中途改侧栏会立即写进 state 并落盘（UI 即时反映、
//! 崩了不丢），但**不会**被塞进正在跑的这一轮 —— 用户的修改行为不可预知，
//! 让模型在一轮之内追着变化跑没有好处。它随下一轮的上下文一起发出去。
//! 唯一的例外是撞车时的 `Stale` 回喂，那是为了省掉模型下一轮重复提交的一次浪费。

use crate::context::{CompactPlan, Context, ContextLimit, fold_instruction, folded_message, plan_compaction};
use crate::handle::CoreHandle;
use crate::ids::TurnId;
use crate::memory::Memory;
use crate::model::{
    AnswerReq, JudgeReq, Message, Mode, Models, MsgRole, Role, StreamEvent, Usage, complete,
};
use crate::msg::{TurnOutcome, TurnStats, UiEvent};
use crate::state::Patch;
use crate::tools::{ASK_USER, Registry, ToolConfig, ToolCtx, ToolResult, run_tools};
use futures_util::StreamExt;
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
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
    /// 持久层：场景库、案例、用户偏好、知识评估、项目概述。
    pub memory: Arc<Memory>,
    pub tool_config: ToolConfig,
    pub context_limit: ContextLimit,
    pub mode: Mode,
}

impl TurnCtx {
    fn tool_ctx(&self) -> ToolCtx {
        ToolCtx {
            token: self.token.clone(),
            ui: self.ui.clone(),
            registry: self.registry.clone(),
            config: self.tool_config,
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

/// 打断收尾。**最容易漏的一处。**
///
/// ① 最后一条 assistant 若带 `tool_calls` 而缺对应的 tool 消息，
///    为每个缺失的 `call_id` 补一条 `[interrupted]`。不补，下一次请求会被 API 直接拒掉。
/// ② `partial` 非空则写成 assistant 消息并打 interrupted 标记。
///    不写，历史里就是「用户问了、助手没答」，模型下一轮的行为会很怪。
///
/// 前提是**永远不用 `JoinHandle::abort()`**：abort 会在任意 await 点撕掉 task，
/// 这里根本跑不到。
pub fn close_open_calls(msgs: &mut Vec<Message>, partial: &str) -> (usize, usize) {
    let mut added_tool = 0usize;

    if let Some(pos) = msgs
        .iter()
        .rposition(|m| matches!(m.role, MsgRole::Assistant) && !m.tool_calls.is_empty())
    {
        let want: Vec<String> = msgs[pos].tool_calls.iter().map(|c| c.id.clone()).collect();
        let have: HashSet<String> =
            msgs[pos + 1..].iter().filter_map(|m| m.tool_call_id.clone()).collect();
        for id in want.into_iter().filter(|id| !have.contains(id)) {
            let mut m = Message::tool(id, "[interrupted] 用户在工具返回前打断");
            m.interrupted = true;
            msgs.push(m);
            added_tool += 1;
        }
    }

    let mut added_assistant = 0usize;
    if !partial.trim().is_empty() {
        let mut m = Message::assistant(partial);
        m.interrupted = true;
        msgs.push(m);
        added_assistant = 1;
    }
    (added_tool, added_assistant)
}

/// 按调用顺序转成 tool 消息。`run_tools` 已保序。
fn tool_messages(results: &[ToolResult]) -> Vec<Message> {
    results.iter().map(|r| Message::tool(&r.call_id, &r.content)).collect()
}

pub async fn run_turn(ctx: TurnCtx) -> TurnOutcome {
    let mut msgs: Vec<Message> = Vec::new();
    let mut stats = TurnStats::default();
    let mut aborted = false;
    let mut partial = String::new();
    // 打断兜底记账用：最后一次发出去的 prompt 有多大。钱已经花了，账不能漏。
    let mut last_prompt_tokens: u32 = 0;

    // ── 整轮唯一一次快照 ──────────────────────────────────────
    // 用户中途的编辑不进这一轮，见模块头部。
    let Some(snap) = ctx.core.snapshot().await else {
        return TurnOutcome { msgs, aborted: true, stats };
    };
    let base_version = snap.version;

    // ── 接近上下文上限 ⇒ 先折叠早期对话 ────────────────────────
    //
    // 压缩必然有损，所以位置很重要：它发生在**判断段之前、上一轮的推断提交之后**。
    // 那时推断图刚被更新过，值得留下的结构化信息已经进图了，被折叠的是叙述过程。
    let mut history = snap.history.clone();
    {
        let probe = Context::build(&snap, &ctx.memory, history.clone(), &ctx.mode.note());
        let fp = probe.footprint();
        if let CompactPlan::Fold { fold, keep } =
            plan_compaction(&history, fp.cacheable + fp.inference, &ctx.context_limit)
        {
            let mut ask = vec![Message::system(fold_instruction(&probe.inference))];
            ask.extend(fold.iter().cloned());
            let fut = complete(ctx.models.subagent.as_ref(), ask, ctx.token.child_token());
            match guarded(&ctx.token, fut).await {
                None => {
                    return TurnOutcome { msgs, aborted: true, stats };
                }
                Some(Ok((summary, usage))) => {
                    ctx.core.cost(ctx.id, Role::Subagent, usage).await;
                    let sm = folded_message(&summary, fold.len());
                    ctx.core.compact_history(ctx.id, sm.clone(), keep.clone(), fold.len()).await;
                    history = std::iter::once(sm).chain(keep).collect();
                    stats.compacted = fold.len() as u32;
                }
                // 摘要失败就带着超长上下文继续 —— 折叠失败不该让用户这一轮用不了。
                Some(Err(e)) => {
                    eprintln!("[turn] 折叠早期对话失败，本轮带全量上下文继续: {e}");
                }
            }
        }
    }

    'turn: loop {
        stats.loops += 1;

        // ── 检查点 0：吸收中途插话 ──────────────────────────────
        let Some(inj) = ctx.core.take_injections(ctx.id).await else {
            aborted = true;
            break 'turn;
        };
        stats.injections += inj.len() as u32;
        msgs.extend(inj.into_iter().map(|m| Message::user(m.text)));

        // 上下文分两次构建：判断段之后 msgs 还会增长（Stale 回喂、检索材料），
        // 回答段必须看到那些新增的内容。
        let mk_context = |extra: &[Message]| {
            Context::build(
                &snap,
                &ctx.memory,
                history.iter().cloned().chain(extra.iter().cloned()).collect(),
                &ctx.mode.note(),
            )
        };
        let context = mk_context(&msgs);

        // ── 判断段 ─────────────────────────────────────────────
        // 判断段和回答段吃同一份对话 —— 判断「这一轮该进哪个场景」本来就要看
        // 用户刚说了什么、之前谈到哪了。两段式省下的是场景 guidance 与案例。
        let t_judge = Instant::now();
        let judge_msgs = context.for_judge();
        last_prompt_tokens = ctx.models.judge.estimate_prompt_tokens(&judge_msgs);
        let req = JudgeReq {
            turn: ctx.id,
            phase: snap.ws.phase,
            mode: ctx.mode,
            view: snap.clone(),
            msgs: judge_msgs,
            token: ctx.token.child_token(),
        };
        let Some(judged) = guarded(&ctx.token, ctx.models.judge.judge(req)).await else {
            aborted = true;
            break 'turn;
        };
        stats.judge_ms += t_judge.elapsed().as_millis() as u64;

        // 判断段失败 ⇒ 降级为「本轮没有判出场景」，继续回答段。
        //
        // 不因为一次 API 抖动就把整轮废掉 —— 那会让工具在网络差的时候完全不可用，
        // 而且「判断失败就拒绝回答」本身就是一次不该有的打扰。
        // 代价是这一轮没做场景判定，所以必须在历史里留一条明账，UI 也要显示。
        let judge = match judged {
            Ok(j) => j,
            Err(e) => {
                msgs.push(Message::system(format!("[判断段失败，本轮未做场景判定] {e}")));
                crate::model::JudgeOut {
                    scene: "none".into(),
                    rationale: format!("判断段失败：{e}"),
                    ops: vec![],
                    retrieve: vec![],
                    usage: Usage::default(),
                }
            }
        };
        ctx.core.cost(ctx.id, Role::Judge, judge.usage).await;

        // 解析场景。模型给了不认识的 id ⇒ 回退到 none，不报错、不打扰用户。
        let mut scene_id = judge.scene.clone();
        if ctx.mode.disabled_scenes().contains(&scene_id.as_str()) {
            scene_id = "none".into();
        }
        let scene = match ctx.memory.playbook.get(&scene_id) {
            Some(s) => s.clone(),
            None => {
                stats.scene_unknown = true;
                scene_id = "none".into();
                ctx.memory.playbook.get("none").cloned().expect("playbook 必须有 none 场景")
            }
        };
        stats.scene = scene_id.clone();
        ctx.core.scene_decided(ctx.id, scene_id.clone(), judge.rationale.clone()).await;

        // 判断段顺带推断出的字段 → 提交给 Core 裁决，只进 pending
        if !judge.ops.is_empty() {
            stats.proposed_ops += judge.ops.len() as u32;
            let patch = Patch::model(ctx.id, base_version, judge.ops.clone());
            match ctx.core.propose(patch).await {
                None => {
                    aborted = true;
                    break 'turn;
                }
                Some(rep) => {
                    stats.rejected_ops += rep.rejected.len() as u32;
                    // rejected 必须回喂，否则模型下一轮还会提交同样的改动，白花钱
                    if let Some(fb) = rep.feedback() {
                        msgs.push(Message::system(fb));
                    }
                }
            }
        }

        // 判断段为**组装回答段上下文**而做的检索。
        // 这不是代替模型执行动作：取回来的是材料，模型在回答段依然可以自己再调工具。
        let mut retrieved: Vec<Message> = Vec::new();
        if !judge.retrieve.is_empty() {
            let (rs, ts) = run_tools(judge.retrieve.clone(), &ctx.tool_ctx()).await;
            stats.tools_run += ts.run;
            stats.tools_timeout += ts.timeout;
            stats.tools_interrupted += ts.interrupted;
            stats.tools_failed += ts.failed;
            stats.heartbeats += ts.heartbeats;
            stats.tool_ms += ts.elapsed_ms;
            retrieved.push(Message::system(format!(
                "为本轮检索到的材料：\n{}",
                rs.iter()
                    .map(|r| format!("- [{}] {}", r.name, r.content))
                    .collect::<Vec<_>>()
                    .join("\n")
            )));
            if ctx.token.is_cancelled() {
                aborted = true;
                break 'turn;
            }
        }

        let _ = ctx.ui.send(UiEvent::SceneChosen {
            turn: ctx.id,
            scene: scene_id.clone(),
            rationale: judge.rationale.clone(),
        });

        // ── 检查点 1：subagent 跑的这段时间里用户说话了吗 ────────
        let Some(inj) = ctx.core.take_injections(ctx.id).await else {
            aborted = true;
            break 'turn;
        };
        if !inj.is_empty() {
            stats.injections += inj.len() as u32;
            msgs.extend(inj.into_iter().map(|m| Message::user(m.text)));
            continue 'turn; // 有新输入 ⇒ 重走判断，场景可能变了
        }

        // ── 回答段 ─────────────────────────────────────────────
        // 注入 guidance 与案例，暴露这个场景的工具。之后 harness 不再干预。
        let t_answer = Instant::now();
        let exposed = ctx.memory.playbook.exposed_tools(&scene);
        let tools = ctx.registry.specs(&exposed);
        let cases = ctx.memory.cases_for(&scene_id);
        let context = mk_context(&msgs);
        let answer_msgs = context.for_answer(&scene, &cases, &retrieved);
        last_prompt_tokens = ctx.models.answer.estimate_prompt_tokens(&answer_msgs);
        let req = AnswerReq {
            turn: ctx.id,
            mode: ctx.mode,
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
                msgs.push(Message::system(format!("[回答段失败] {e}")));
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
                        if !partial.is_empty() { msgs.push(Message::assistant(&partial)); }
                        break 'turn;
                    }
                    Some(StreamEvent::Chunk(d)) => {
                        partial.push_str(&d);
                        // 流式 chunk 直发 UI，不经 Core
                        let _ = ctx.ui.send(UiEvent::Delta(d));
                    }
                    Some(StreamEvent::ToolCalls(cs)) => {
                        // 模型自己决定调什么、调几个。harness 只负责跑和保序。
                        stats.asked_user += cs.iter().filter(|c| c.name == ASK_USER).count() as u32;
                        msgs.push(Message::assistant_with_calls(&partial, cs.clone()));
                        partial.clear();
                        let (rs, ts) = run_tools(cs, &ctx.tool_ctx()).await;
                        stats.tools_run += ts.run;
                        stats.tools_timeout += ts.timeout;
                        stats.tools_interrupted += ts.interrupted;
                        stats.tools_failed += ts.failed;
                        stats.heartbeats += ts.heartbeats;
                        stats.tool_ms += ts.elapsed_ms;
                        msgs.extend(tool_messages(&rs));
                        stats.answer_ms += t_answer.elapsed().as_millis() as u64;
                        if ctx.token.is_cancelled() {
                            aborted = true;
                            break 'turn;
                        }
                        continue 'turn; // 带结果再问模型，顺便重过检查点
                    }
                    Some(StreamEvent::Done(usage)) => {
                        ctx.core.cost(ctx.id, Role::Answer, usage).await;
                        msgs.push(Message::assistant(&partial));
                        partial.clear();
                        stats.answer_ms += t_answer.elapsed().as_millis() as u64;
                        break 'turn;
                    }
                    Some(StreamEvent::Failed(e)) => {
                        msgs.push(Message::system(format!("[流中断] {e}")));
                        if !partial.is_empty() { msgs.push(Message::assistant(&partial)); }
                        partial.clear();
                        stats.answer_ms += t_answer.elapsed().as_millis() as u64;
                        break 'turn;
                    }
                }
            }
        }
    }

    if aborted {
        // 流式中断拿不到 usage，但 prompt 已经发出去了，钱已经花了。
        let est = Usage::estimate(last_prompt_tokens, partial.chars().count());
        ctx.core.cost(ctx.id, Role::Answer, est).await;
        close_open_calls(&mut msgs, &partial);
    }

    TurnOutcome { msgs, aborted, stats }
}
