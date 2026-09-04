//! 工具接入 + `run_tools`：并行/串行调度、心跳、硬超时。
//!
//! # 并行与否由工具自己声明
//!
//! turn 内部是串行的（一条控制流读得下来），但一批工具调用之间要不要并行
//! 不该由 turn 决定——只有工具自己知道它能不能并发（改文件的不能，读的能）。
//! 所以 [`Tool::concurrency`] 是工具的属性，`run_tools` 照做。
//!
//! # 心跳：长任务不阻塞，但 turn 仍然串行
//!
//! 这是「定时先返回一个仍在进行的状态」的落点。turn task 在同一个 `select!` 里
//! 同时照看**取消、心跳、结果**三件事，串行性没有被破坏——它只是没有把自己
//! 阻塞在「等全部结果」这一件事上。
//!
//! 后面要优化成「先处理已经好了的部分」，改的是这个 `while` 的退出条件：
//! 把「全部 Some」换成「关键项 Some」，其余降级成 timeout 结果继续。**结构不用动。**

use crate::ids::TaskId;
use crate::model::Call;
use crate::msg::UiEvent;
use futures_util::stream::{FuturesUnordered, StreamExt};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Concurrency {
    Parallel,
    Sequential,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolResultKind {
    Ok,
    /// 超过 HARD_LIMIT 还没回来。**带着这个结果继续**，不是整轮失败——
    /// 模型可以据此重试或换个做法，比把用户已经等了两分钟的一轮整个废掉好。
    Timeout,
    /// 用户打断。
    Interrupted,
    /// 模型编了一个不存在的工具名。
    NotFound,
    /// 工具自己报错。
    ///
    /// **所有这些失败态都是作为结果返回给模型的，不抛异常、不中断 turn、不问用户。**
    /// 工具失败是模型该应对和调整的事；把它升级成一次对用户的打扰是工程债。
    Failed,
}

impl ToolResultKind {
    /// 落进时间线的字符串。R6 按它分类统计工具的成败分布。
    pub fn tag(&self) -> &'static str {
        match self {
            ToolResultKind::Ok => "ok",
            ToolResultKind::Timeout => "timeout",
            ToolResultKind::Interrupted => "interrupted",
            ToolResultKind::NotFound => "not_found",
            ToolResultKind::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub call_id: String,
    pub name: String,
    pub content: String,
    pub kind: ToolResultKind,
    /// 本地调用序号。**R6「分账到具体调用」的落点** —— 上一版 `TaskId`
    /// 定义了却没有任何地方用，成本只能按 role 聚合到底。
    pub task: TaskId,
}

impl ToolResult {
    fn of(call: &Call, content: impl Into<String>, kind: ToolResultKind) -> Self {
        Self {
            call_id: call.id.clone(),
            name: call.name.clone(),
            content: content.into(),
            kind,
            task: TaskId(0),
        }
    }

    pub fn ok(call: &Call, content: impl Into<String>) -> Self {
        Self::of(call, content, ToolResultKind::Ok)
    }

    pub fn timeout(call: &Call) -> Self {
        Self::of(call, "[timeout] 工具超过硬上限未返回，本轮按未完成处理", ToolResultKind::Timeout)
    }

    pub fn interrupted(call: &Call) -> Self {
        Self::of(call, "[interrupted] 用户打断", ToolResultKind::Interrupted)
    }

    pub fn failed(call: &Call, why: impl Into<String>) -> Self {
        Self::of(call, format!("[failed] {}", why.into()), ToolResultKind::Failed)
    }

    pub fn not_found(call: &Call) -> Self {
        Self::of(
            call,
            format!("[not_found] 没有名为 {} 的工具", call.name),
            ToolResultKind::NotFound,
        )
    }
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    /// 给模型看的说明。
    ///
    /// **「什么时候该用这个工具」写在这里，不写进 harness。**
    /// 这是上一版最大的错误被改掉的地方之一：原来 harness 拿着一个 `Action` 枚举
    /// 替模型决定「这一步要读仓库」；现在只是把工具连同说明摆出来，用不用模型自己定。
    fn description(&self) -> &str;
    fn concurrency(&self) -> Concurrency;
    fn run<'a>(
        &'a self,
        call: Call,
        token: CancellationToken,
    ) -> crate::model::BoxFuture<'a, ToolResult>;
}

#[derive(Default)]
pub struct Registry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, t: Arc<dyn Tool>) -> Self {
        self.tools.insert(t.name().to_string(), t);
        self
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn concurrency(&self, name: &str) -> Concurrency {
        self.tools.get(name).map(|t| t.concurrency()).unwrap_or(Concurrency::Parallel)
    }

    /// 本轮**暴露**给模型的工具清单。场景只决定这个子集，不决定调用哪个、调几次。
    pub fn specs(&self, names: &[String]) -> Vec<crate::model::ToolSpec> {
        names
            .iter()
            .filter_map(|n| self.tools.get(n))
            .map(|t| crate::model::ToolSpec {
                name: t.name().to_string(),
                description: t.description().to_string(),
            })
            .collect()
    }
}

/// 内置工具：把一个带候选项的问题呈现给用户。
///
/// # 它是工具，不是 harness 动作
///
/// 上一版这是 `Action::AskUser`：判断段选中它，harness 就掐断回答段、
/// 把问题当成本轮全部输出交回用户。那等于 harness 替模型决定了「这一轮要提问」。
///
/// 现在它只是回答段可以调用的一个工具。模型可以先讲两段再问，可以不问，
/// 也可以问完接着说。调用之后 turn **不会**被 harness 中止 —— 工具返回一句
/// 「已呈现，等用户回复」，模型自己决定要不要收尾。
///
/// UI 侧收到 [`UiEvent::Choice`] 把它渲染成选择题；用户选了什么，
/// 会作为下一轮的普通用户输入回来。
pub struct AskUser;

pub const ASK_USER: &str = "ask_user";

impl Tool for AskUser {
    fn name(&self) -> &str {
        ASK_USER
    }

    fn description(&self) -> &str {
        "向用户提一个带候选项的问题。args: {question: string, options: [string]}。
         新手答不上开放式问题，所以只在你能给出具体候选项时用它；想不出候选项就在正文里直接问。
         问完通常应该结束本轮等用户回答。"
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Sequential
    }

    fn run<'a>(&'a self, call: Call, _token: CancellationToken) -> crate::model::BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let q = call.args.get("question").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let opts: Vec<String> = call
                .args
                .get("options")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default();
            if q.is_empty() {
                return ToolResult::failed(&call, "缺少 question");
            }
            // 注意：这里不阻塞等待用户。阻塞会让 turn 挂在一个人类时间尺度的等待上，
            // 打断、插话、落盘全都得排队。用户的回答走下一轮的正常输入路径。
            //
            // 提问本身已经由 turn 在跑这个工具**之前**落成一条 `Asked` 事件了，
            // 所以即使这一轮就此结束、界面刷新、进程重启，那道选择题也还在。
            ToolResult::ok(
                &call,
                format!(
                    "已把问题和 {} 个候选项呈现给用户，等他回复。现在可以结束本轮了。",
                    opts.len()
                ),
            )
        })
    }
}

impl AskUser {
    /// 从一次调用里取出问题与选项，供 turn 侧推 UI 事件。
    pub fn parse(call: &Call) -> Option<(String, Vec<String>)> {
        let q = call.args.get("question")?.as_str()?.to_string();
        let opts = call
            .args
            .get("options")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        Some((q, opts))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ToolConfig {
    /// 心跳周期。语义是「仍在进行」，不是「快好了」。
    pub heartbeat: Duration,
    /// 硬上限。到点把未完成的填成 timeout 结果**继续**。
    pub hard_limit: Duration,
}

impl Default for ToolConfig {
    fn default() -> Self {
        Self { heartbeat: Duration::from_secs(30), hard_limit: Duration::from_secs(300) }
    }
}

/// `run_tools` 需要的最小上下文。刻意不直接吃 `TurnCtx`，
/// 这样这个文件不依赖 turn 的控制流，可以单独测。
pub struct ToolCtx {
    pub token: CancellationToken,
    pub ui: broadcast::Sender<UiEvent>,
    pub registry: Arc<Registry>,
    /// 与 Core 共享的 TaskId 分配器。turn 侧直接取号，省一次往返。
    pub next_task: Arc<AtomicU64>,
    pub config: ToolConfig,
}

#[derive(Debug, Clone, Default)]
pub struct ToolRunStats {
    pub run: u32,
    pub timeout: u32,
    pub interrupted: u32,
    pub failed: u32,
    /// 模型主动调用 `ask_user` 的次数 —— 验收方案里的副指标「干预次数」。
    pub asked_user: u32,
    pub heartbeats: u32,
    pub elapsed_ms: u64,
}

async fn run_one(
    idx: usize,
    tool: Arc<dyn Tool>,
    call: Call,
    token: CancellationToken,
) -> (usize, ToolResult) {
    let r = tool.run(call, token).await;
    (idx, r)
}

/// 跑一批工具调用，**按调用顺序**返回结果。
///
/// 用 `Vec<Option<_>>` 占位而不是 `push`：OpenAI 风格接口要求 assistant 的
/// `tool_calls` 后面跟着 id 匹配的 tool 消息，两个工具乱序返回时必须按
/// **原始调用顺序**重组再发下一轮，否则请求会被 API 直接拒掉。
pub async fn run_tools(calls: Vec<Call>, ctx: &ToolCtx) -> (Vec<ToolResult>, ToolRunStats) {
    let n = calls.len();
    let mut stats = ToolRunStats::default();
    if n == 0 {
        return (vec![], stats);
    }

    let mut out: Vec<Option<ToolResult>> = vec![None; n];
    let mut running = FuturesUnordered::new();
    let mut serial_queue: VecDeque<usize> = VecDeque::new();
    let mut serial_idx: HashSet<usize> = HashSet::new();

    for (i, c) in calls.iter().enumerate() {
        // 提问不在这里推给 UI：它是主时间线上的一条 `Asked` 事件，
        // turn 在跑工具**之前**就已经落盘了（见 turn.rs）。这样界面刷新、
        // turn 结束、进程重启，那道选择题都还在。
        if c.name == ASK_USER {
            stats.asked_user += 1;
        }
        match ctx.registry.get(&c.name) {
            None => out[i] = Some(ToolResult::not_found(c)),
            Some(tool) => match tool.concurrency() {
                Concurrency::Parallel => {
                    stats.run += 1;
                    running.push(run_one(i, tool, c.clone(), ctx.token.child_token()));
                }
                Concurrency::Sequential => {
                    stats.run += 1;
                    serial_queue.push_back(i);
                    serial_idx.insert(i);
                }
            },
        }
    }
    // 串行队列同一时刻只放一个在跑。
    if let Some(i) = serial_queue.pop_front()
        && let Some(tool) = ctx.registry.get(&calls[i].name) {
            running.push(run_one(i, tool, calls[i].clone(), ctx.token.child_token()));
        }

    let start = Instant::now();
    let mut tick = tokio::time::interval(ctx.config.heartbeat);
    // interval 的第一次 tick 立即就绪，先吃掉，否则一进来就发一次无意义的心跳。
    tick.tick().await;
    // 硬上限是**独立的绝对期限**，不搭在心跳上。
    // 上一版只在心跳分支里判 `elapsed > hard_limit`，于是心跳周期一旦大于硬上限
    // （比如心跳 250ms、硬上限 20ms），硬上限就完全失效 —— 实测要等满 250ms。
    let deadline = tokio::time::Instant::now() + ctx.config.hard_limit;

    while out.iter().any(Option::is_none) {
        tokio::select! {
            _ = ctx.token.cancelled() => break,

            Some((idx, res)) = running.next() => {
                // 完成的是串行任务 ⇒ 放下一个进去
                if serial_idx.contains(&idx)
                    && let Some(j) = serial_queue.pop_front()
                        && let Some(tool) = ctx.registry.get(&calls[j].name) {
                            running.push(run_one(j, tool, calls[j].clone(), ctx.token.child_token()));
                        }
                if res.kind == ToolResultKind::Failed {
                    stats.failed += 1;
                }
                let _ = ctx.ui.send(UiEvent::TaskDone { idx, name: res.name.clone() });
                out[idx] = Some(res);
            }

            _ = tick.tick() => {
                stats.heartbeats += 1;
                let pending: Vec<String> = out
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.is_none())
                    .map(|(i, _)| calls[i].name.clone())
                    .collect();
                let _ = ctx.ui.send(UiEvent::StillRunning {
                    pending,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                });
            }

            _ = tokio::time::sleep_until(deadline) => {
                // 到点把未完成的填成 timeout 结果**继续**，不是整轮失败。
                for (i, slot) in out.iter_mut().enumerate() {
                    if slot.is_none() {
                        *slot = Some(ToolResult::timeout(&calls[i]));
                        stats.timeout += 1;
                    }
                }
                break;
            }
        }
    }

    stats.elapsed_ms = start.elapsed().as_millis() as u64;
    let results: Vec<ToolResult> = out
        .into_iter()
        .enumerate()
        .map(|(i, o)| {
            let mut r = o.unwrap_or_else(|| {
                stats.interrupted += 1;
                ToolResult::interrupted(&calls[i])
            });
            // 每次调用（含失败与超时）都发一个号：失败的调用也花了时间和钱，
            // 分账里少了它，「工具占多少开销」这个数就是假的。
            r.task = TaskId(ctx.next_task.fetch_add(1, Ordering::Relaxed));
            r
        })
        .collect();
    (results, stats)
}
