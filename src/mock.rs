//! 可编排的假模型与假工具。**链路测试的全部依赖。**
//!
//! 判断段场景注入、模型自主调工具、工具心跳、打断收尾、注入吸收这几条路径，
//! 用真模型跑一次要几十秒且不可复现，用它可以在毫秒级确定性地跑完。
//! 真实客户端接进来时只要实现同样的 [`ModelClient`] / [`Tool`]，core 一行都不用改
//! —— 这本身就是 R3「模型可配」是不是真做到了的检验。
//!
//! 它还记录**每次实际发出去的 prompt**，用来断言上下文分层：
//! 判断段不含 history、用户轮中的编辑不进当前 turn，都靠这个查。

use crate::model::{
    AnswerReq, BoxFuture, BoxStream, Call, JudgeOut, JudgeReq, ModelClient, ModelError,
    StreamEvent, Usage,
};
use crate::tools::{Concurrency, Tool, ToolResult, ToolResultKind};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// 按脚本回放的模型。脚本用完之后回退到「场景 none + 一句话回答」。
pub struct MockModel {
    judge_script: Mutex<VecDeque<Result<JudgeOut, String>>>,
    answer_script: Mutex<VecDeque<Vec<StreamEvent>>>,
    /// 每次判断段实际收到的 prompt 全文（按顺序）。
    pub seen_judge: Mutex<Vec<String>>,
    /// 每次回答段实际收到的 prompt 全文（按顺序）。
    pub seen_answer: Mutex<Vec<String>>,
    /// 判断段耗时，用来制造「打断/编辑发生在判断段」的场景。
    pub judge_delay: Duration,
    /// 每个 chunk 之间的间隔，用来制造「打断发生在流式中途」的场景。
    pub chunk_delay: Duration,
}

impl Default for MockModel {
    fn default() -> Self {
        Self {
            judge_script: Mutex::new(VecDeque::new()),
            answer_script: Mutex::new(VecDeque::new()),
            seen_judge: Mutex::new(Vec::new()),
            seen_answer: Mutex::new(Vec::new()),
            judge_delay: Duration::from_millis(1),
            chunk_delay: Duration::from_millis(1),
        }
    }
}

impl MockModel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_judge(self, out: JudgeOut) -> Self {
        self.judge_script.lock().unwrap().push_back(Ok(out));
        self
    }

    pub fn on_judge_err(self, e: impl Into<String>) -> Self {
        self.judge_script.lock().unwrap().push_back(Err(e.into()));
        self
    }

    pub fn on_answer(self, events: Vec<StreamEvent>) -> Self {
        self.answer_script.lock().unwrap().push_back(events);
        self
    }

    /// 跑起来之后再往脚本里追加。
    ///
    /// 图的场景需要它：第二轮的 op 要引用第一轮铸出来的真 id，而那个 id
    /// 在建脚本的时刻还不存在。把它写死等于把 `n<seq>_<i>` 的铸法钉进测试。
    pub fn push_judge(&self, out: JudgeOut) {
        self.judge_script.lock().unwrap().push_back(Ok(out));
    }

    pub fn push_answer(&self, events: Vec<StreamEvent>) {
        self.answer_script.lock().unwrap().push_back(events);
    }

    /// 回答段先调一次动作工具提交这批 op，然后（下一圈）继续走脚本。
    pub fn on_ops(self, ops: &[crate::state::Op]) -> Self {
        self.answer_script.lock().unwrap().push_back(op_events(ops));
        self
    }

    pub fn push_ops(&self, ops: &[crate::state::Op]) {
        self.answer_script.lock().unwrap().push_back(op_events(ops));
    }

    pub fn judge_calls(&self) -> usize {
        self.seen_judge.lock().unwrap().len()
    }

    pub fn judge_delay(mut self, d: Duration) -> Self {
        self.judge_delay = d;
        self
    }

    pub fn chunk_delay(mut self, d: Duration) -> Self {
        self.chunk_delay = d;
        self
    }

    pub fn judge_prompt(&self, i: usize) -> String {
        self.seen_judge.lock().unwrap().get(i).cloned().unwrap_or_default()
    }

    pub fn answer_prompt(&self, i: usize) -> String {
        self.seen_answer.lock().unwrap().get(i).cloned().unwrap_or_default()
    }

    pub fn answer_calls(&self) -> usize {
        self.seen_answer.lock().unwrap().len()
    }
}

fn flatten(msgs: &[crate::model::Message]) -> String {
    msgs.iter()
        .map(|m| format!("<{:?}>{}", m.role, m.content))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn default_judge() -> JudgeOut {
    judge_of("none")
}

/// 判成某个场景。**判断段只判场景** —— 推断改动走回答段的动作工具，
/// 见 [`op_events`]。
pub fn judge_of(scene: &str) -> JudgeOut {
    judge_all(&[scene])
}

/// 一次判出多个场景。
pub fn judge_all(scenes: &[&str]) -> JudgeOut {
    JudgeOut {
        scenes: scenes.iter().map(|s| s.to_string()).collect(),
        rationale: format!("判成 {}", scenes.join("+")),
        usage: Usage { prompt: 120, completion: 30, estimated: false },
    }
}

fn default_answer() -> Vec<StreamEvent> {
    vec![
        StreamEvent::Chunk("好的".into()),
        StreamEvent::Chunk("。".into()),
        StreamEvent::Done(Usage { prompt: 400, completion: 60, estimated: false }),
    ]
}

impl ModelClient for MockModel {
    fn judge<'a>(&'a self, req: JudgeReq) -> BoxFuture<'a, Result<JudgeOut, ModelError>> {
        self.seen_judge.lock().unwrap().push(flatten(&req.msgs));
        let step = self.judge_script.lock().unwrap().pop_front();
        let delay = self.judge_delay;
        Box::pin(async move {
            // 真实客户端也该这样：把 token 传进去，取消时立刻中止请求，别让钱白烧。
            tokio::select! {
                _ = req.token.cancelled() => Err(ModelError::Call("cancelled".into())),
                _ = tokio::time::sleep(delay) => match step {
                    Some(Ok(o)) => Ok(o),
                    Some(Err(e)) => Err(ModelError::Call(e)),
                    None => Ok(default_judge()),
                },
            }
        })
    }

    fn stream<'a>(&'a self, req: AnswerReq) -> BoxFuture<'a, Result<BoxStream, ModelError>> {
        self.seen_answer.lock().unwrap().push(flatten(&req.msgs));
        let events = self.answer_script.lock().unwrap().pop_front().unwrap_or_else(default_answer);
        let delay = self.chunk_delay;
        let token = req.token.clone();
        Box::pin(async move {
            let st = futures_util::stream::unfold(
                (events.into_iter(), delay, token),
                |(mut it, d, tok)| async move {
                    let ev = it.next()?;
                    if !d.is_zero() {
                        tokio::select! {
                            _ = tok.cancelled() => return None,
                            _ = tokio::time::sleep(d) => {}
                        }
                    }
                    Some((ev, (it, d, tok)))
                },
            );
            Ok(Box::pin(st) as BoxStream)
        })
    }
}

/// 一个会磨蹭的工具，用来看心跳和硬超时。
pub struct SlowTool {
    pub name: String,
    pub dur: Duration,
    pub concurrency: Concurrency,
}

impl SlowTool {
    pub fn parallel(name: &str, dur: Duration) -> Self {
        Self { name: name.into(), dur, concurrency: Concurrency::Parallel }
    }

    pub fn sequential(name: &str, dur: Duration) -> Self {
        Self { name: name.into(), dur, concurrency: Concurrency::Sequential }
    }
}

impl Tool for SlowTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "一个耗时的假工具，用于测试心跳与硬超时"
    }

    fn concurrency(&self) -> Concurrency {
        self.concurrency
    }

    fn run<'a>(&'a self, call: Call, token: CancellationToken) -> BoxFuture<'a, ToolResult> {
        let dur = self.dur;
        Box::pin(async move {
            tokio::select! {
                _ = token.cancelled() => ToolResult::interrupted(&call),
                _ = tokio::time::sleep(dur) => {
                    ToolResult::ok(&call, format!("{} 跑了 {:?}", call.name, dur))
                }
            }
        })
    }
}

/// 立刻返回的工具。
pub struct EchoTool;

impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "原样回显参数。args: 任意 JSON"
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Parallel
    }

    fn run<'a>(&'a self, call: Call, _token: CancellationToken) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let c = call.args.to_string();
            ToolResult::ok(&call, c)
        })
    }
}

/// 一定失败的工具。
///
/// 用来验证**工具失败不打扰用户**：它既不抛异常、也不中断 turn、更不弹给用户，
/// 而是作为一条 tool 消息回给模型，由模型应对和调整。
pub struct FlakyTool;

impl Tool for FlakyTool {
    fn name(&self) -> &str {
        "flaky"
    }

    fn description(&self) -> &str {
        "一个总是失败的假工具，用于测试失败路径"
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Parallel
    }

    fn run<'a>(&'a self, call: Call, _token: CancellationToken) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move { ToolResult::failed(&call, "上游 500") })
    }
}

pub fn call(id: &str, name: &str) -> Call {
    Call { id: id.into(), name: name.into(), args: serde_json::json!({}) }
}

/// 带参数的调用。图/工具链的场景要给工具真的传东西。
pub fn call_with(id: &str, name: &str, args: serde_json::Value) -> Call {
    Call { id: id.into(), name: name.into(), args }
}

/// 把一批 op 包成回答段的一次动作调用。
///
/// 图内的走 `record_graph`，图外的走 `record_note`，两者**在同一批里**发出 ——
/// 那正是别名作用域的边界，也是真实模型该有的用法。
pub fn op_calls(ops: &[crate::state::Op]) -> Vec<Call> {
    let (g, n): (Vec<_>, Vec<_>) =
        ops.iter().cloned().partition(crate::actions::is_graph_op);
    let mut out = Vec::new();
    let pack = |name: &str, v: Vec<crate::state::Op>| Call {
        id: format!("{name}-1"),
        name: name.into(),
        args: serde_json::json!({ "ops": v }),
    };
    if !g.is_empty() {
        out.push(pack(crate::actions::RECORD_GRAPH, g));
    }
    if !n.is_empty() {
        out.push(pack(crate::actions::RECORD_NOTE, n));
    }
    out
}

/// 回答段脚本：先提交一批推断，什么都不说。下一圈由 `on_answer` 接着走。
pub fn op_events(ops: &[crate::state::Op]) -> Vec<StreamEvent> {
    vec![StreamEvent::ToolCalls(op_calls(ops))]
}

pub fn ask_call(id: &str, question: &str, options: &[&str]) -> Call {
    Call {
        id: id.into(),
        name: crate::tools::ASK_USER.into(),
        args: serde_json::json!({ "question": question, "options": options }),
    }
}

/// 断言用：结果里有没有失败态。
pub fn has_kind(rs: &[ToolResult], k: ToolResultKind) -> bool {
    rs.iter().any(|r| r.kind == k)
}
