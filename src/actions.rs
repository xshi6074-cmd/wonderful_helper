//! **动作工具**：模型把推断写回主时间线的两个入口。
//!
//! # 为什么是工具，不是判断段的副产品
//!
//! 上一版把「顺带做出的推断」挂在判断段的输出里（`JudgeOut.ops`）。那有两个错：
//!
//! 1. **判断和推断是两回事。** 判断段只回答「这一轮该进哪个场景」，它跑在回答段
//!    之前、在模型读任何东西之前。而推断是读完论文/仓库之后才形成的东西 ——
//!    挂在判断段等于要求模型在还没看材料的时候就把结论写下来。
//! 2. 那条路**从来没通过**：判断段工具的 `ops` 字段 schema 是个空对象，
//!    没有任何地方告诉过模型 op 长什么样，于是真实模型跑出来永远是 `inferred_ops: 0`。
//!
//! 现在它们是回答段可以调用的两个普通工具，**调用时机写在 description 里**
//! （和 `ask_user` 一样，这是这个项目对「不用 harness 替模型决定流程」的一贯做法）。
//!
//! # 为什么是两个工具而不是一个
//!
//! - **参数形状根本不同**：图是 `node/edge/drop` 这套有严格 schema、要铸 id、
//!   要过仲裁、要 sanitize 的结构；图外是自由文本的键值 + 待落定/搁置。
//!   塞进一个工具会得到一个「二选一」的 schema，模型经常两边各填一半。
//! - **调用时机不同**：图是拓扑真的变了才写；关键信息是随时想补一条口径、
//!   一条否决理由就写。合成一个，模型为了记一句话得把整张图重提一遍。
//! - **不多花钱**：两个调用可以在同一条 assistant 消息里并发发出，一次往返。
//!
//! # 一批调用 = 一次提交 = 一个别名作用域
//!
//! 同一条 assistant 消息里的动作调用由 [`crate::turn`] 合并成**一次**
//! `Emit::Infer`。所以 `record_note` 里的 `anchor: "$enc"` 能指到同一批
//! `record_graph` 刚建的那个节点 —— 拆成两次提交就指不到了，那正是把它们
//! 分成两个工具时最容易踩的坑。
//!
//! # 它们为什么不在自己的 `run` 里干活
//!
//! 写主时间线要 `CoreHandle`，而 `Tool::run` 只拿得到 call 和 token。
//! 所以 turn 在跑工具**之前**把它们摘出来自己提交（和 `ask_user` 落
//! `Asked` 事件是同一个位置）。这里的 `run` 只在漏摘时才会被调到，
//! 它返回一条显式的内部错误 —— 宁可吵，也不要再来一次静默断链。

use crate::model::ToolSpec;
use crate::state::Op;
use crate::tools::{Concurrency, Tool, ToolResult};
use crate::model::Call;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

pub const RECORD_GRAPH: &str = "record_graph";
pub const RECORD_NOTE: &str = "record_note";

/// 这个调用要不要由 turn 就地提交。
pub fn is_action(name: &str) -> bool {
    name == RECORD_GRAPH || name == RECORD_NOTE
}

/// 从一次动作调用里取出 op 列表。
///
/// 解析不了的**单条**丢掉并记名，不让一条坏 op 废掉整批 —— 但要在返回里说清楚
/// 丢了几条，否则模型会以为都写进去了。
pub fn parse(call: &Call) -> (Vec<Op>, Vec<String>) {
    let mut ops = Vec::new();
    let mut bad = Vec::new();
    let Some(arr) = call.args.get("ops").and_then(Value::as_array) else {
        bad.push("缺少 ops 数组".to_string());
        return (ops, bad);
    };
    for item in arr {
        match serde_json::from_value::<Op>(item.clone()) {
            Ok(o) => ops.push(o),
            Err(e) => bad.push(format!("{e} — {}", clip(&item.to_string(), 160))),
        }
    }
    (ops, bad)
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n).collect::<String>() + "…"
}

/// 一条 op 是图内的还是图外的。用来在结果里告诉模型「你放错工具了」——
/// **不拒绝执行**：两个工具收到的 op 都照样提交，分工是给模型看的，不是闸门。
pub fn is_graph_op(op: &Op) -> bool {
    !matches!(op, Op::Set { .. } | Op::Remove { .. })
}

// ───────────────────────── record_graph ─────────────────────────

pub struct RecordGraph;

impl Tool for RecordGraph {
    fn name(&self) -> &str {
        RECORD_GRAPH
    }

    fn description(&self) -> &str {
        "把你对实验流程 / 模型架构的推断落到推断图上。\n\
         **调用时机**：先用 fs_* / web_* 读到该读的，再调这个把拓扑写下来，最后才写正文回答。\n\
         图是给用户点选和就地修改的，所以每次只提交你**这一轮真想改的那几项**，\
         不要每轮把整张图重发一遍（所有字段都是 patch，不给就是不改）。\n\
         新建元素时 id 填 `$别名`（如 `$enc`），同一轮后续调用仍可引用已经成功创建的别名；\
         提交后由程序铸成稳定 id。改已有元素就直接填它现在的 id。\n\
         没有实证来源的节点会画成虚线 —— 那是给用户看的盲区，所以 source 要老实填。\
         source=\"user\" 只允许用户在界面上主动选择；你不能输出，输出了整条 op 会被丢弃。"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "ops": {
                "type": "array",
                "description": "一批图内改动，按顺序生效。",
                "items": {
                    "type": "object",
                    "properties": {
                        "op": {
                            "type": "string",
                            "enum": ["node", "edge", "drop", "render", "sketch", "view"],
                            "description": "node=建/改节点 edge=建/改边 drop=删 render=改整图渲染选项 sketch=改成你自己写的源码 view=切换用哪一份"
                        },
                        "id": { "type": "string", "description": "node/edge：新建填 $别名，改已有填它的 id（如 n12_0 / e12_1）" },
                        "kind": { "type": "string", "description": "节点：data/module/op/loss/metric/ablation/baseline/gate/note；边：flow/feeds/supervises/compares/depends。也可以自己造词，形状不认识就落到默认" },
                        "label": { "type": "string", "description": "图上显示的名字，短" },
                        "body": { "type": "string", "description": "装不进标签的细节，只在节点明细里出现" },
                        "parent": { "type": ["string", "null"], "description": "归到哪个节点下面（画成 subgraph）；null = 提到顶层" },
                        "attrs": { "type": "array", "description": "[[键, 值]]，值给 null 表示删这一项",
                                   "items": { "type": "array" } },
                        "from": { "type": "string", "description": "edge：起点节点 id 或 $别名" },
                        "to": { "type": "string", "description": "edge：终点节点 id 或 $别名。op=view 时填 built 或 sketch" },
                        "source": {
                            "description": "这条推断哪来的。你自己推测填 guess（画虚线）；有实证填 repo/paper。user 是用户界面专属值，模型不得输出，否则整条 op 丢弃。",
                            "anyOf": [
                                { "type": "string", "enum": ["guess"] },
                                { "type": "object", "properties": { "repo": { "type": "string" } }, "required": ["repo"] },
                                { "type": "object", "properties": { "paper": { "type": "string" } }, "required": ["paper"] }
                            ]
                        },
                        "confidence": { "type": "number", "description": "0–1" },
                        "why": { "type": "string", "description": "drop：为什么去掉" },
                        "key": { "type": "string", "description": "render：如 dialect / dir / classdef.<名>" },
                        "value": { "type": ["string", "null"], "description": "render 的值，给 null 表示删" },
                        "lang": { "type": "string", "enum": ["mermaid", "html"], "description": "sketch 的语言" },
                        "src": { "type": "string", "description": "sketch：你自己写的图源码。自由但没有节点 id，用户只能整段改、点不了单个节点" }
                    },
                    "required": ["op"],
                    "allOf": [
                        { "if": { "properties": { "op": { "const": "node" } } }, "then": { "required": ["id"] } },
                        { "if": { "properties": { "op": { "const": "edge" } } }, "then": { "required": ["id"] } },
                        { "if": { "properties": { "op": { "const": "drop" } } }, "then": { "required": ["id"] } },
                        { "if": { "properties": { "op": { "const": "render" } } }, "then": { "required": ["key"] } },
                        { "if": { "properties": { "op": { "const": "sketch" } } }, "then": { "required": ["lang", "src"] } },
                        { "if": { "properties": { "op": { "const": "view" } } }, "then": { "required": ["to"] } }
                    ]
                }
            }},
            "required": ["ops"]
        })
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Sequential
    }

    fn run<'a>(&'a self, call: Call, _t: CancellationToken) -> crate::model::BoxFuture<'a, ToolResult> {
        Box::pin(async move { ToolResult::failed(&call, NOT_INTERCEPTED) })
    }
}

// ───────────────────────── record_note ─────────────────────────

pub struct RecordNote;

impl Tool for RecordNote {
    fn name(&self) -> &str {
        RECORD_NOTE
    }

    fn description(&self) -> &str {
        "记下**图上装不下**的关键信息：口径与用户的原话约束、指标定义、\
         已经否决的路线和否决理由、还没落定的问题、暂时搁置的问题。\n\
         **调用时机**：和 record_graph 一样在正式回答之前；两个可以在同一次里一起调。\n\
         判断标准很简单：能画成节点或连线的用 record_graph，剩下的用这个。\n\
         anchor 填一个节点 id（或同一轮前面成功创建的 $别名），这条就挂在那个节点下面显示。\n\
         open = 还没落定的问题清单，parked = 搁置的。两者都是**整份替换**，\
         所以要删一条就把剩下的重发一遍；落定了就从 open 里去掉。\
         source=\"user\" 只允许用户在界面上主动选择；你不能输出，输出了整条 op 会被丢弃。"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "ops": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "op": { "type": "string", "enum": ["set", "remove"] },
                        "path": { "type": "string", "description": "点分路径，如 spec.claim / metric.bwt.定义。op=set 时若给了 open/parked，path 固定填 open 或 parked" },
                        "value": { "description": "字符串 / 数字 / 布尔" },
                        "anchor": { "type": "string", "description": "挂到哪个节点（id 或同批的 $别名）。挂错或指不到只会清掉 anchor，这条本身仍然生效" },
                        "open": { "type": "array", "items": { "type": "string" }, "description": "整份替换待落定清单" },
                        "parked": { "type": "array", "items": { "type": "string" }, "description": "整份替换搁置清单" },
                        "source": {
                            "anyOf": [
                                { "type": "string", "enum": ["guess"] },
                                { "type": "object", "properties": { "repo": { "type": "string" } }, "required": ["repo"] },
                                { "type": "object", "properties": { "paper": { "type": "string" } }, "required": ["paper"] }
                            ]
                        },
                        "confidence": { "type": "number" }
                    },
                    "required": ["op"],
                    "allOf": [
                        { "if": { "properties": { "op": { "const": "set" } } }, "then": { "required": ["path"] } },
                        { "if": { "properties": { "op": { "const": "remove" } } }, "then": { "required": ["path"] } }
                    ]
                }
            }},
            "required": ["ops"]
        })
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Sequential
    }

    fn run<'a>(&'a self, call: Call, _t: CancellationToken) -> crate::model::BoxFuture<'a, ToolResult> {
        Box::pin(async move { ToolResult::failed(&call, NOT_INTERCEPTED) })
    }
}

const NOT_INTERCEPTED: &str =
    "内部错误：动作工具必须由 turn 就地提交，这次走到了普通工具路径，改动没有生效。";

/// 注册这两个。**永远注册**，不看配置 —— 它们不依赖任何外部服务，
/// 而「模型能不能把推断写下来」不该是个可选项。
pub fn register(reg: crate::tools::Registry) -> crate::tools::Registry {
    reg.with(std::sync::Arc::new(RecordGraph)).with(std::sync::Arc::new(RecordNote))
}

/// 给启动日志用：这两个工具的 spec。
pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: RECORD_GRAPH.into(),
            description: RecordGraph.description().into(),
            schema: RecordGraph.schema(),
        },
        ToolSpec {
            name: RECORD_NOTE.into(),
            description: RecordNote.description().into(),
            schema: RecordNote.schema(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{FlowView, Source};

    fn call_of(name: &str, ops: Value) -> Call {
        Call { id: "c1".into(), name: name.into(), args: json!({ "ops": ops }) }
    }

    #[test]
    fn graph_ops_parse_from_the_shape_the_schema_advertises() {
        // 这一条防的是「schema 说的和 serde 收的对不上」——
        // 上一版 schema 是个空对象，模型怎么写都错，而且错得没有声音。
        let c = call_of(RECORD_GRAPH, json!([
            { "op": "node", "id": "$enc", "kind": "module", "label": "编码器", "source": "guess" },
            { "op": "node", "id": "$dec", "kind": "module", "label": "解码器",
              "source": { "repo": "src/model.py:42" }, "confidence": 0.9 },
            { "op": "edge", "id": "$e1", "from": "$enc", "to": "$dec", "kind": "flow" },
            { "op": "render", "key": "dir", "value": "LR" },
            { "op": "view", "to": "built" },
        ]));
        let (ops, bad) = parse(&c);
        assert!(bad.is_empty(), "{bad:?}");
        assert_eq!(ops.len(), 5);
        assert!(ops.iter().all(is_graph_op));
        match &ops[1] {
            Op::Node { source, confidence, .. } => {
                assert_eq!(source.as_ref().unwrap(), &Source::Repo("src/model.py:42".into()));
                assert_eq!(*confidence, Some(0.9));
            }
            o => panic!("{o:?}"),
        }
        assert!(matches!(ops[4], Op::View { to: FlowView::Built }));
    }

    #[test]
    fn note_ops_parse_and_are_not_graph_ops() {
        let c = call_of(RECORD_NOTE, json!([
            { "op": "set", "path": "spec.claim", "value": "对照组要同 seed", "source": "user" },
            { "op": "set", "path": "open", "open": ["用哪个指标测遗忘？"] },
            { "op": "remove", "path": "spec.old" },
        ]));
        let (ops, bad) = parse(&c);
        assert!(bad.is_empty(), "{bad:?}");
        assert_eq!(ops.len(), 3);
        assert!(!ops.iter().any(is_graph_op));
    }

    #[test]
    fn one_broken_op_does_not_kill_the_batch_but_is_reported() {
        let c = call_of(RECORD_GRAPH, json!([
            { "op": "node", "id": "$a", "label": "留下的" },
            { "op": "这不是个 op" },
        ]));
        let (ops, bad) = parse(&c);
        assert_eq!(ops.len(), 1);
        assert_eq!(bad.len(), 1);
    }

    #[test]
    fn missing_ops_array_is_an_error_not_an_empty_batch() {
        let c = Call { id: "c".into(), name: RECORD_NOTE.into(), args: json!({ "path": "x" }) };
        let (ops, bad) = parse(&c);
        assert!(ops.is_empty());
        assert_eq!(bad.len(), 1);
    }
}
