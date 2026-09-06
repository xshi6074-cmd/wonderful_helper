//! 模型接入的 seam：消息、判断输出、流式事件、`ModelClient` trait、三个模型角色。
//!
//! core 不认识任何具体厂商。真实客户端（reqwest + SSE）和 [`crate::mock`] 里的假模型
//! 都实现 [`ModelClient`]，这是 R3「模型可配」的落点。
//!
//! # 为什么 trait 方法返回 `BoxFuture` 而不是 `async fn`
//!
//! 需要 `Arc<dyn ModelClient>` 才能在运行时换模型；trait 里的 `async fn` 至今不是 dyn-safe。
//! 手写 `Pin<Box<dyn Future>>` 换来对象安全，代价是一次装箱，相对几十秒的网络往返可以忽略。
//!
//! # 这里不再有 `Action` 枚举
//!
//! 上一版判断段吐一个 `Action`，harness 照着执行（`AskUser` 就掐断回答段、`Block` 就拒绝推进）。
//! 那是拿状态机替模型做流程决策。现在判断段只产出一个[场景判定][crate::scene]，
//! harness 拿它注入 prompt 和暴露工具，之后不再干预 —— 模型想提问就自己调 `ask_user`。

use crate::ids::TurnId;
use crate::scene::SceneId;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type BoxStream = Pin<Box<dyn futures_util::Stream<Item = StreamEvent> + Send>>;

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("模型调用失败: {0}")]
    Call(String),
    #[error("判断段输出不是合法的场景判定: {0}")]
    Schema(String),
}

/// 三个模型角色，各自可配 provider / model / 参数。
///
/// 分开还有一个 R6 上的用处：能算出**判断段只占总 token 的 X%**，
/// 用来证明两段式循环不贵。合并成一个总数就讲不了这句话。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Role {
    /// 判断段：不带场景 guidance 与案例，可配便宜模型
    Judge,
    /// 回答段：正文与工具调用
    Answer,
    /// subagent：检索、仓库问答、论文抽取，大块上下文不进主循环
    Subagent,
}

/// 三个角色各自的客户端。
pub struct Models {
    pub judge: Arc<dyn ModelClient>,
    pub answer: Arc<dyn ModelClient>,
    pub subagent: Arc<dyn ModelClient>,
}

impl Models {
    /// 三个角色共用一个客户端（测试与最简配置）。
    pub fn uniform(m: Arc<dyn ModelClient>) -> Models {
        Models { judge: m.clone(), answer: m.clone(), subagent: m }
    }

    pub fn of(&self, role: Role) -> &Arc<dyn ModelClient> {
        match role {
            Role::Judge => &self.judge,
            Role::Answer => &self.answer,
            Role::Subagent => &self.subagent,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt: u32,
    pub completion: u32,
    /// true 表示这是**估算**值。
    ///
    /// 流式被打断时拿不到服务端返回的 usage，但 prompt 已经发出去了，钱已经花了。
    /// 不估算兜底 R6 的账目就会漏 —— 而被打断的轮次往往还是最贵的那种。
    pub estimated: bool,
}

impl Usage {
    pub fn total(&self) -> u32 {
        self.prompt + self.completion
    }

    /// 打断兜底：prompt 按实际发出的算，completion 按已收到的 partial 长度粗估。
    /// 3 字符/token 是中英混排的折中，宁可高估也不要让账目显得比实际便宜。
    pub fn estimate(prompt: u32, partial_chars: usize) -> Usage {
        Usage { prompt, completion: (partial_chars / 3) as u32, estimated: true }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MsgRole {
    System,
    User,
    Assistant,
    Tool,
}

/// 一次工具调用。`id` 必须原样回填到对应的 tool 消息上。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Call {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
}

/// 暴露给模型的一个工具。模型自主决定是否调用。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    /// 给模型看的说明。**「什么时候该用这个工具」写在这里，不写进 harness。**
    pub description: String,
    /// 参数的 JSON Schema。
    ///
    /// 真实 API（Anthropic 的 `input_schema`、OpenAI 的 `function.parameters`）
    /// 都要它。默认是个「随便什么对象」，但那样模型只能从 description 里猜参数名 ——
    /// 猜错一次就是一次白烧的往返。所以工具应该自己给准确的 schema。
    #[serde(default = "any_object")]
    pub schema: serde_json::Value,
}

pub fn any_object() -> serde_json::Value {
    serde_json::json!({ "type": "object", "properties": {} })
}

/// 会话历史里的一条消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: MsgRole,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<Call>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// 这条消息是在打断收尾中产生的。UI 显示成灰色/带标记。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub interrupted: bool,
}

impl Message {
    fn make(role: MsgRole, content: impl Into<String>) -> Message {
        Message {
            role,
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: None,
            interrupted: false,
        }
    }

    pub fn user(text: impl Into<String>) -> Message {
        Self::make(MsgRole::User, text)
    }

    pub fn system(text: impl Into<String>) -> Message {
        Self::make(MsgRole::System, text)
    }

    pub fn assistant(text: impl Into<String>) -> Message {
        Self::make(MsgRole::Assistant, text)
    }

    pub fn assistant_with_calls(text: impl Into<String>, calls: Vec<Call>) -> Message {
        let mut m = Self::make(MsgRole::Assistant, text);
        m.tool_calls = calls;
        m
    }

    pub fn tool(call_id: impl Into<String>, content: impl Into<String>) -> Message {
        let mut m = Self::make(MsgRole::Tool, content);
        m.tool_call_id = Some(call_id.into());
        m
    }
}

/// 判断段的输入。
pub struct JudgeReq {
    pub turn: TurnId,
    pub mode: Mode,
    /// 由 [`crate::context::Context::for_judge`] 组装：共享层 + 完整对话，
    /// **不含任何场景的 guidance 与案例** —— 那些只给回答段。
    ///
    /// 上一版还带了一份 `StateView`，但推断图已经拼在 `msgs` 的共享层里了，
    /// 多一份就是多一处可能不同步的副本。
    pub msgs: Vec<Message>,
    /// 传给客户端用于中止底层 HTTP 请求。
    /// turn task 侧另有 `guarded` 兜底，两者都要有：前者省钱，后者保证控制流。
    pub token: CancellationToken,
}

/// 判断段的输出：**只有一次场景判定。**
///
/// # 这里原来还有 `ops` 和 `retrieve`，都删了
///
/// - `ops`：**判断和推断是两回事。** 判断跑在回答段之前、在模型读任何材料之前，
///   而推断是读完之后才形成的东西。挂在这里等于要求模型在还没看材料时就下结论。
///   而且那条路从来没通过 —— 工具 schema 里 `ops` 是个空对象，没有任何地方
///   告诉过模型 op 长什么样，真实模型跑出来永远是 `inferred_ops: 0`。
///   写推断现在是回答段的两个工具，见 [`crate::actions`]。
/// - `retrieve`：真实客户端里恒为 `vec![]`，turn 侧那段消费代码是死的。
///   要检索，回答段自己调 `fs_*` / `web_*`。
///
/// 注意它不含任何「让 harness 去做某事」的字段。harness 拿到 `scene` 之后
/// 只做三件准备工作：注入 guidance、注入案例、暴露工具。
#[derive(Debug, Clone)]
pub struct JudgeOut {
    /// 判成了哪几个场景。**一组，不是一个** —— 一轮里「目标还没说清」和
    /// 「预算和方案对不上」可以同时成立，只准判一个的话另一条就永远注不进去。
    /// 认不出的 id 由 turn 侧丢掉；一个都不剩就回退到 `none`。
    pub scenes: Vec<SceneId>,
    /// 为什么判成这个场景。UI 侧栏要显示它，用户才能判断要不要一键更换场景。
    pub rationale: String,
    pub usage: Usage,
}

/// 回答段的输入。
pub struct AnswerReq {
    pub turn: TurnId,
    pub mode: Mode,
    /// 由 [`crate::context::Context::for_answer`] 组装 —— 含 guidance、案例与 history。
    pub msgs: Vec<Message>,
    /// 本轮暴露给模型的工具。**用不用、用几次、什么顺序，由模型决定。**
    pub tools: Vec<ToolSpec>,
    pub token: CancellationToken,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Chunk(String),
    ToolCalls(Vec<Call>),
    Done(Usage),
    Failed(String),
}

/// 探索 / 行动。**两套 prompt，不是一个开关。**
///
/// # 原来有第三个旋钮「打扰预算」，已删除
///
/// 那个设计是拿限额换安静：预算耗尽就让模型「直接给结论」——
/// 也就是在用**降低质量**来减少打扰。真正的问题不在次数，在于有些地方本来就不该
/// 给模型打扰用户的权限。那些地方已经逐个掐掉了（见 `turn.rs` 头部的清单），
/// 掐掉之后就不需要限额了：模型选一个合适的做法本来就是它该完成的工作，
/// 用户想干预随时可以打断或插话。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    Explore,
    Go,
}

impl Mode {
    /// `prompts.toml` 里 `[mode.*]` 的键。
    pub fn key(&self) -> &'static str {
        match self {
            Mode::Explore => "explore",
            Mode::Go => "go",
        }
    }

    /// 旋钮一：这个 mode 下不启用的场景。
    ///
    /// 行动 mode 关掉讲解类场景 —— 用户已经表示要往前走了，就别倒回去讲基础。
    pub fn disabled_scenes(&self) -> &'static [&'static str] {
        match self {
            Mode::Explore => &[],
            Mode::Go => &["teach_background"],
        }
    }

    // 原来这里还有 note() 和 extraction_target()：mode 的全部内容就是硬编码的两句话。
    // 现在整套（note + 额外工具 + 每个工具在这个 mode 下的补充说明）都在
    // `prompts.toml` 的 [mode.explore] / [mode.go] 里，见 `Memory::mode`。
}

/// 跑一次非流式补全：把流收干，返回全文与用量。
///
/// 摘要（上下文压缩）和蒸馏都用它。它们不需要流式 —— 用户看的是结果不是过程。
pub async fn complete(
    client: &dyn ModelClient,
    msgs: Vec<Message>,
    token: CancellationToken,
) -> Result<(String, Usage), ModelError> {
    use futures_util::StreamExt;
    let mut st = client
        .stream(AnswerReq {
            turn: TurnId(u64::MAX),
            mode: Mode::Go,
            msgs,
            tools: vec![],
            token: token.clone(),
        })
        .await?;
    let mut out = String::new();
    let mut usage = Usage::default();
    loop {
        tokio::select! {
            biased;
            _ = token.cancelled() => return Err(ModelError::Call("cancelled".into())),
            ev = st.next() => match ev {
                None => break,
                Some(StreamEvent::Chunk(d)) => out.push_str(&d),
                Some(StreamEvent::Done(u)) => { usage = u; break }
                Some(StreamEvent::Failed(e)) => return Err(ModelError::Call(e)),
                // 摘要/蒸馏不给工具，模型仍然吐 tool_calls 只能是它跑偏了，忽略
                Some(StreamEvent::ToolCalls(_)) => {}
            }
        }
    }
    if usage.total() == 0 {
        usage = Usage::estimate(0, out.chars().count());
    }
    Ok((out, usage))
}

/// 模型客户端。R3 的接口边界。
pub trait ModelClient: Send + Sync {
    fn judge<'a>(&'a self, req: JudgeReq) -> BoxFuture<'a, Result<JudgeOut, ModelError>>;
    fn stream<'a>(&'a self, req: AnswerReq) -> BoxFuture<'a, Result<BoxStream, ModelError>>;
    /// 估算一次请求的 prompt token 数，供打断兜底记账用。
    fn estimate_prompt_tokens(&self, msgs: &[Message]) -> u32 {
        msgs.iter().map(|m| (m.content.chars().count() / 3) as u32).sum()
    }
}
