//! 主时间线。**只有一条，线性，按时间。**
//!
//! # 这个文件替代了什么
//!
//! 上一版有两套并行的持久化：`history.jsonl`（对话）与 `snapshot.json`（状态），
//! 靠「先写日志再改状态」的人工纪律维持一致，而且有六类数据从来没进过任何一套
//! （inbox / cost / metrics / TurnStats / 提问 / 折叠结果）。
//!
//! 现在只有一条时间线：**所有改动都包成事件，原子写入**。对话是它，状态改动是它，
//! 用户操作是它，工具调用与返回也是它。恢复 = 从头（或从最近 checkpoint）重放。
//!
//! # 三条随之而来的性质
//!
//! 1. **不需要中间 cache / version**。[`crate::state::Workspace`] 是这条流的物化结果，
//!    不是另一份需要同步的真相。checkpoint 是纯功能性的加速，删掉不影响正确性。
//! 2. **小生命周期挂在主线上，不另开表**。工具调用发出与返回、提问与回答，
//!    都是主线上的两条事件，后者用 [`Event::corr`] 指回前者。截断在中间就是
//!    「发出了没返回」，恢复时补一条 `Aborted` —— 与打断收尾走同一段代码。
//! 3. **metadata 不进时间线**。system prompt、场景 guidance、案例、推断图快照
//!    都是**组装时动态拼**的（见 [`crate::context`]）。所以用户改了
//!    `memory/*.md` 或换了场景，下一轮立刻生效，不需要迁移任何历史数据。
//!
//! # 语义要写清楚
//!
//! 用户的一句话和用户对提问的回答，对 harness 来说流程上差别不大，但**语义不同**：
//! 后者要能告诉模型「这是你问的那个问题的答案」。所以它们是两个 body
//! （[`Body::Said`] / [`Body::Answered`]），组装成 prompt 时措辞也不同。
//! 凡是这种「流程一样、含义不同」的地方，一律分成不同的 body，不靠字段区分。

use crate::ids::{Seq, SessionId, TaskId, TurnId};
use crate::model::{Call, Message, Mode, Role, Usage};
use crate::scene::SceneId;
use crate::state::{Op, Path};
use serde::{Deserialize, Serialize};

/// 收一个字符串或一组字符串，都给成 `Vec`。
///
/// 场景从「一个」改成「一组」时用它兜住老时间线：库里存的是
/// `"scene": "trace_code"`，新代码要的是 `["trace_code"]`。
/// 没有它，改完之后所有历史会话都读不回来。
fn one_or_many<'de, D>(d: D) -> Result<Vec<SceneId>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(s) => vec![s],
        OneOrMany::Many(v) => v,
    })
}

/// 主时间线上的一条。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// 属于哪个会话。**事件自描述**，不靠外层容器说明自己是谁的 ——
    /// 分叉之后同一段历史会被多条链读到，事件本身必须说得清它是在哪条链上写下的。
    pub session: SessionId,
    pub seq: Seq,
    /// 属于哪一轮。`None` = 轮外操作（用户编辑侧栏、推进阶段、更换场景）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<TurnId>,
    pub at_ms: u64,
    /// 这条事件回应的是哪一条。工具返回 → 调用，回答 → 提问。
    ///
    /// **这就是「小生命周期」的全部机制**：不需要额外的表，也不需要在内存里
    /// 维护一份未闭合调用的映射。要找未闭合的，扫一遍时间线即可。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corr: Option<Seq>,
    pub body: Body,
}

/// 事件体。
///
/// 分组不是为了好看：**只有前三组会变成 prompt 里的消息**，后两组是状态与观测。
/// 加新 body 时先想清楚它属于哪一组，写进 [`Body::as_message`]。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Body {
    // ───────────── 用户说的（进 prompt） ─────────────
    /// 用户的一句话。
    Said { client_id: String, text: String },
    /// 用户回答了模型的提问。`corr` 指向那条 [`Body::Asked`]。
    ///
    /// 与 `Said` 分开的理由：组装 prompt 时要写成「对提问 #N 的回答」，
    /// 否则模型看到的只是一句孤零零的话，认不出它是自己那个问题的答案。
    Answered { choice: String },

    // ───────────── 模型说的（进 prompt） ─────────────
    /// 助手正文。`interrupted` 表示这是打断收尾时落下的半截。
    Wrote {
        text: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        interrupted: bool,
    },
    /// 助手发起工具调用。**发出即落盘**，所以「发出了没返回」是可观测的状态。
    Called { text: String, calls: Vec<Call> },

    // ───────────── 工具回的（进 prompt） ─────────────
    /// 工具返回。`corr` 指向那条 `Called`。
    Returned {
        call_id: String,
        name: String,
        content: String,
        /// ok | timeout | interrupted | not_found | failed
        outcome: String,
        task: TaskId,
    },
    /// harness 插进对话的一句话：判断段失败、被丢弃的改动回喂、降级告警。
    ///
    /// 它是 system 消息，**必须进 prompt** —— 「你上一步的改动没生效」这类信息
    /// 不回喂给模型，模型下一轮会原样再提交一次，白花钱。
    Noted { text: String },

    // ───────────── 状态改动（不进 prompt，物化进 Workspace） ─────────────
    /// 用户 apply 了侧栏编辑。**只记生效的部分。**
    Edited { ops: Vec<Op> },
    /// 模型推断出的改动。**只记生效的部分**；被 turn 内仲裁丢掉的记在 `dropped` 里，
    /// 那不是状态，是给用户看的审计 + 回喂给模型的依据。
    Inferred {
        ops: Vec<Op>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        dropped: Vec<Path>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        invalid: Vec<Path>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        conflicts: Vec<Path>,
    },
    /// **已废弃。** 阶段闸门（设计讨论 / 交接）删掉了，见 `crate::state` 里那段说明。
    ///
    /// 变体留着**只为反序列化老时间线**：`Body` 是内部 tag 的枚举，
    /// 少一个变体就意味着老库里的那条事件解不出来、整条会话读不回来。
    /// 它不进 prompt、不改 Workspace、不再被任何地方产生。
    PhaseSet { to: String },

    // ───────────── 流程与观测（不进 prompt） ─────────────
    TurnOpened { mode: Mode },
    TurnClosed { aborted: bool, stats: String },
    /// 判断段判定的场景。它不进 prompt —— 它**决定了拼什么**，不是被拼进去的内容。
    ///
    /// **是一组，不是一个。** 一轮里「目标还没说清」和「预算和方案对不上」
    /// 可以同时成立，只准判一个的话，另一条的 guidance 就永远注不进去。
    Judged {
        #[serde(alias = "scene", deserialize_with = "one_or_many")]
        scenes: Vec<SceneId>,
        rationale: String,
    },
    /// 用户手动改场景。下一轮组装上下文时以它为准，并注入这些场景的 guidance 与案例。
    SceneOverridden {
        #[serde(deserialize_with = "one_or_many")]
        from: Vec<SceneId>,
        #[serde(deserialize_with = "one_or_many")]
        to: Vec<SceneId>,
    },
    /// 模型通过 `ask_user` 提的问题。`corr` 指向那条 `Called`。
    ///
    /// 它是**持久实体**而不是一条有损的 UI 事件：turn 结束、界面刷新、进程重启，
    /// 那道选择题都还在，直到有一条 `Answered` 指回来。
    Asked { question: String, options: Vec<String> },
    /// 早期对话被折叠。`from..=to` 之间的事件不再进 prompt，代之以 `summary`。
    /// **原文不删** —— UI 可以展开，蒸馏可以读全程。
    Folded { from: Seq, to: Seq, summary: String, folded: u32 },
    Cost {
        role: Role,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task: Option<TaskId>,
        usage: Usage,
    },
    /// 上一次运行留下的未闭合调用，在启动时被补上。
    ///
    /// 与打断收尾产生的 `[interrupted]` 是同一件事的两个时机：进程还活着时由
    /// `close_open_calls` 补，进程没了就由恢复补。不补的话，下一次请求里会有
    /// 一条带 `tool_calls` 却没有对应 tool 消息的 assistant，被 API 直接拒掉。
    Aborted { call_id: String, why: String },
}

impl Body {
    /// 这条事件在 prompt 里对应哪条消息。`None` = 不进 prompt。
    ///
    /// **只有这一个地方决定「什么进上下文」**。加了新 body 忘了在这里处理，
    /// 结果就是它对模型不可见 —— 所以这里用穷举 match，不写 `_ =>`。
    pub fn as_message(&self, seq: Seq, corr: Option<Seq>) -> Option<Message> {
        match self {
            Body::Said { text, .. } => Some(Message::user(text)),
            Body::Answered { choice } => Some(Message::user(match corr {
                Some(q) => format!("[对提问 {} 的回答] {choice}", crate::ids::QuestionId(q)),
                None => choice.clone(),
            })),
            Body::Wrote { text, interrupted } => {
                let mut m = Message::assistant(text);
                m.interrupted = *interrupted;
                Some(m)
            }
            Body::Called { text, calls } => Some(Message::assistant_with_calls(text, calls.clone())),
            Body::Returned { call_id, content, .. } => Some(Message::tool(call_id, content)),
            Body::Aborted { call_id, why } => {
                let mut m = Message::tool(call_id, why);
                m.interrupted = true;
                Some(m)
            }
            Body::Noted { text } => Some(Message::system(text)),
            Body::Folded { summary, folded, .. } => Some(Message::system(format!(
                "[已折叠早期对话 {folded} 条 · 展开见 {seq}]\n{summary}"
            ))),
            // 状态与观测不进 prompt：推断图作为 metadata 动态拼接，
            // 场景判定决定拼什么而不是被拼进去，其余是流水。
            Body::Edited { .. }
            | Body::Inferred { .. }
            | Body::PhaseSet { .. }
            | Body::TurnOpened { .. }
            | Body::TurnClosed { .. }
            | Body::Judged { .. }
            | Body::SceneOverridden { .. }
            | Body::Asked { .. }
            | Body::Cost { .. } => None,
        }
    }

    /// 给 UI 的一行摘要，也用于链路测试断言时间线形状。
    pub fn tag(&self) -> &'static str {
        match self {
            Body::Said { .. } => "said",
            Body::Answered { .. } => "answered",
            Body::Wrote { .. } => "wrote",
            Body::Called { .. } => "called",
            Body::Returned { .. } => "returned",
            Body::Aborted { .. } => "aborted",
            Body::Noted { .. } => "noted",
            Body::Edited { .. } => "edited",
            Body::Inferred { .. } => "inferred",
            Body::PhaseSet { .. } => "phase",
            Body::TurnOpened { .. } => "turn_open",
            Body::TurnClosed { .. } => "turn_close",
            Body::Judged { .. } => "judged",
            Body::SceneOverridden { .. } => "scene_override",
            Body::Asked { .. } => "asked",
            Body::Folded { .. } => "folded",
            Body::Cost { .. } => "cost",
        }
    }
}

/// 还没分配 seq 的事件。Core 在 `commit` 里补上 session、seq 与时间戳。
pub struct Draft {
    pub turn: Option<TurnId>,
    pub corr: Option<Seq>,
    pub body: Body,
}

impl Draft {
    pub fn new(turn: Option<TurnId>, body: Body) -> Draft {
        Draft { turn, corr: None, body }
    }
    pub fn reply_to(turn: Option<TurnId>, corr: Seq, body: Body) -> Draft {
        Draft { turn, corr: Some(corr), body }
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ───────────────────────── 从时间线组装 ─────────────────────────

/// 把一段时间线组装成 prompt 里的消息序列。
///
/// 折叠在这里生效：一条 `Folded{from,to}` 会把 `from..=to` 之间的事件跳过，
/// 代之以摘要本身。**原始事件仍在时间线上**，所以 UI 能展开、蒸馏能读全程 ——
/// 折叠只影响「拼给模型看的那份」。
///
/// # 必须先扫一遍区间，不能边走边跳
///
/// `Folded` 事件是在它覆盖的区间**之后**才追加的（先折叠、再记录折了什么）。
/// 上一版边走边设 `skip_until`，等读到 `Folded` 时被折的那几条早就吐出去了 ——
/// 结果是摘要和原文同时进 prompt，折叠一个 token 都没省下，而且模型会看到
/// 同一段话说了两遍。所以这里先收区间再过滤。
///
/// 嵌套折叠自然按最外层生效：内层那条 `Folded` 的 seq 落在外层区间里，
/// 会连同被它覆盖的原文一起被跳过，不会重复注入两份摘要。
/// 组装**全部**原文，忽略折叠。
///
/// 蒸馏用它。`assemble` 会跳过被 `Folded` 覆盖的区间、只留一条摘要 ——
/// 那对拼 prompt 是对的（就是为了省 token），但对蒸馏是错的：
/// 长会话里最该被沉淀的恰恰是早期那段，读到摘要等于二次压缩。
/// 原文一直在时间线上，这里就直接读原文，只把 `Folded` 那条摘要本身跳过。
pub fn assemble_full(events: &[Event]) -> Vec<Message> {
    events
        .iter()
        .filter(|e| !matches!(e.body, Body::Folded { .. }))
        .filter_map(|e| e.body.as_message(e.seq, e.corr))
        .collect()
}

/// 给 compact / distill 的结构化变更记录。最终 Workspace 只能说明“现在是什么”，
/// 不能说明谁在什么时候改了什么；这里只选会影响决定与纠错的事件，不塞 phase/cost 心跳。
pub fn change_log(events: &[Event], range: Option<(Seq, Seq)>) -> String {
    events
        .iter()
        .filter(|event| range.is_none_or(|(from, to)| event.seq >= from && event.seq <= to))
        .filter(|event| matches!(
            event.body,
            Body::Edited { .. }
                | Body::Inferred { .. }
                | Body::SceneOverridden { .. }
                | Body::Judged { .. }
        ))
        .filter_map(|event| {
            serde_json::to_string(&event.body).ok().map(|body| {
                format!("#{} turn={} {}", event.seq.0, event.turn.map(|t| t.0.to_string()).unwrap_or_else(|| "outside".into()), body)
            })
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn assemble(events: &[Event]) -> Vec<Message> {
    let folds: Vec<(Seq, Seq)> = events
        .iter()
        .filter_map(|e| match &e.body {
            Body::Folded { from, to, .. } => Some((*from, *to)),
            _ => None,
        })
        .collect();
    let covered = |s: Seq| folds.iter().any(|(f, t)| s >= *f && s <= *t);

    let mut out = Vec::with_capacity(events.len());
    for e in events {
        if covered(e.seq) {
            continue;
        }
        if let Some(m) = e.body.as_message(e.seq, e.corr) {
            out.push(m);
        }
    }
    out
}

/// 扫出「发出了但没返回」的工具调用。
///
/// 恢复时用它补 [`Body::Aborted`]；也用于 UI 显示「上次异常退出时有 N 个调用未完成」。
/// 返回 `(Called 的 seq, call_id, 工具名)`。
pub fn unclosed_calls(events: &[Event]) -> Vec<(Seq, String, String)> {
    let mut open: Vec<(Seq, String, String)> = Vec::new();
    for e in events {
        match &e.body {
            Body::Called { calls, .. } => {
                for c in calls {
                    open.push((e.seq, c.id.clone(), c.name.clone()));
                }
            }
            Body::Returned { call_id, .. } | Body::Aborted { call_id, .. } => {
                open.retain(|(_, id, _)| id != call_id);
            }
            _ => {}
        }
    }
    open
}

/// 扫出还没被回答的提问。恢复后 UI 要把它们重新画出来。
pub fn open_questions(events: &[Event]) -> Vec<(Seq, String, Vec<String>)> {
    let mut open: Vec<(Seq, String, Vec<String>)> = Vec::new();
    for e in events {
        match &e.body {
            Body::Asked { question, options } => {
                open.push((e.seq, question.clone(), options.clone()))
            }
            Body::Answered { .. } => {
                if let Some(q) = e.corr {
                    open.retain(|(s, _, _)| *s != q);
                }
            }
            _ => {}
        }
    }
    open
}

/// 找出「开了没关」的轮次。启动时据此把它们标成异常退出，并告诉用户。
pub fn crashed_turns(events: &[Event]) -> Vec<TurnId> {
    let mut open: Vec<TurnId> = Vec::new();
    for e in events {
        match (&e.body, e.turn) {
            (Body::TurnOpened { .. }, Some(t)) => open.push(t),
            (Body::TurnClosed { .. }, Some(t)) => open.retain(|x| *x != t),
            _ => {}
        }
    }
    open
}
