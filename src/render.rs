//! 把 [`Graph`] 渲染成源码。**纯函数，Core 与 UI 调的是同一个。**
//!
//! # 为什么渲染不由模型做
//!
//! 用户改一个节点的名字，图变了，渲染必须**立刻**变 —— 不能为此发一次模型调用。
//! 而且重启之后必须逐字节相同，否则「模型以为的图」和「用户看到的图」会分叉，
//! 那是这个项目最不该有的东西。所以渲染是 `Graph -> String` 的纯函数（不变量 I10）。
//!
//! # 两种画法都从这里出去
//!
//! - [`FlowView::Built`]：结构化 nodes/edges → mermaid。形状与配色查
//!   [`GraphStyle`]（在 `memory/prompts.toml` 里，用户随时可改），
//!   **未知 kind 落到默认形状，不报错**。
//! - [`FlowView::Sketch`]：模型自己写的源码，原样出去。
//!
//! 两边产出都是「源码」，所以 UI 侧是同一个「源码 + 预览」双栏组件。
//!
//! # 虚线是这张图最重要的一笔
//!
//! 没标来源、或标了 `Source::Guess` 的节点与边画成虚线。图上一眼看过去哪些是
//! 模型自己猜的，那正是要给用户看的盲区 —— 比图本身更有价值。

use crate::ids::Seq;
use crate::memory::GraphStyle;
use crate::state::{FlowView, Graph, Lang, Node, Workspace};

/// 当前视图的源码。`None` = 图是空的，prompt 与 UI 都不该出现这一段。
pub fn source(g: &Graph, style: &GraphStyle) -> Option<(Lang, String)> {
    match g.view {
        FlowView::Sketch => g.sketch.as_ref().map(|s| (s.lang, s.src.clone())),
        FlowView::Built => {
            if g.nodes.is_empty() {
                None
            } else {
                Some((Lang::Mermaid, mermaid(g, style)))
            }
        }
    }
}

/// 结构化图 → mermaid。
pub fn mermaid(g: &Graph, style: &GraphStyle) -> String {
    let dialect = g.render.get("dialect").map(String::as_str).unwrap_or("flowchart");
    let dir = g.render.get("dir").map(String::as_str).unwrap_or("TD");
    let mut out = format!("{dialect} {dir}\n");

    // 节点：从顶层往下递归，子节点写进 subgraph。BTreeMap 保证顺序确定。
    for n in g.nodes.values().filter(|n| n.parent.is_none()) {
        emit_node(g, style, n, 1, &mut out);
    }

    // 边。虚线由 prov 决定，实线的连接符查 kind。
    for e in g.edges.values() {
        let conn = if e.prov.is_dashed() {
            "-.->"
        } else {
            style.edge_for(&e.kind)
        };
        if e.label.is_empty() {
            out.push_str(&format!("  {} {} {}\n", e.from.0, conn, e.to.0));
        } else {
            out.push_str(&format!(
                "  {} {}|\"{}\"| {}\n",
                e.from.0,
                conn,
                esc(&e.label),
                e.to.0
            ));
        }
    }

    // classDef：先是用户在 prompts.toml 里给各 kind 定的，再是模型临时塞的。
    let mut kinds: Vec<&str> = g.nodes.values().map(|n| n.kind.as_str()).collect();
    kinds.sort_unstable();
    kinds.dedup();
    for k in &kinds {
        if let Some(def) = style.class.get(*k) {
            out.push_str(&format!("  classDef {k} {def}\n"));
            let members: Vec<&str> = g
                .nodes
                .values()
                .filter(|n| n.kind == *k)
                .map(|n| n.id.0.as_str())
                .collect();
            out.push_str(&format!("  class {} {k}\n", members.join(",")));
        }
    }
    for (key, val) in &g.render {
        if let Some(name) = key.strip_prefix("classdef.") {
            out.push_str(&format!("  classDef {name} {val}\n"));
        }
    }

    // 虚线节点。放最后，让它盖过 kind 的配色 —— 「这是猜的」比「这是个损失函数」重要。
    let guessed: Vec<&str> = g
        .nodes
        .values()
        .filter(|n| n.prov.is_dashed())
        .map(|n| n.id.0.as_str())
        .collect();
    if !guessed.is_empty() {
        // 模型自己定过 classdef.guess 的话，上面那轮 render 循环已经写出去了，
        // 这里只补 class 绑定，别写第二遍。
        if !g.render.contains_key("classdef.guess") {
            out.push_str(&format!("  classDef guess {GUESS_CLASS}\n"));
        }
        out.push_str(&format!("  class {} guess\n", guessed.join(",")));
    }
    out
}

/// 没标来源的元素长什么样。放常量里是为了让「虚线」这件事只有一个定义。
const GUESS_CLASS: &str = "stroke-dasharray: 5 3";

fn emit_node(g: &Graph, style: &GraphStyle, n: &Node, depth: usize, out: &mut String) {
    let pad = "  ".repeat(depth);
    let kids = g.children(&n.id);
    if kids.is_empty() {
        let shape = style.shape_for(&n.kind);
        out.push_str(&format!("{pad}{}{}\n", n.id.0, shape.replace("%s", &quoted(&n.label))));
        return;
    }
    // 有子节点 ⇒ 画成 subgraph。id 仍然出现在源码里，所以点选与 I10 都成立。
    out.push_str(&format!("{pad}subgraph {}[{}]\n", n.id.0, quoted(&n.label)));
    for k in kids {
        emit_node(g, style, k, depth + 1, out);
    }
    out.push_str(&format!("{pad}end\n"));
}

fn quoted(s: &str) -> String {
    format!("\"{}\"", esc(s))
}

/// mermaid 的标签里不能出现裸引号与换行。
fn esc(s: &str) -> String {
    s.replace('"', "#quot;").replace(['\n', '\r'], " ")
}

/// TODO(html-built)：`Built` 视图的 HTML 渲染。
///
/// 逻辑和 [`mermaid`] 高度相似 —— 同样是按 id 序遍历 `nodes`、递归展开子图、
/// 按 `kind` 查形状、按 `prov.is_dashed()` 决定虚实、最后铺边；差别只在产出
/// `<div>` / `<svg>` 而不是 mermaid 文本，以及边要自己算路径（mermaid 那边是
/// 渲染器代劳的）。
///
/// **现在不写**：节点组件长什么样是 R2 的 UI 决定的，此刻写等于凭空猜 DOM 结构，
/// 猜错了要连着测试一起返工。`Sketch` 视图的 HTML 已经能用（模型自己写的源码
/// 原样出去），所以 HTML 这条路不是完全空白。
pub fn built_html(_g: &Graph, _style: &GraphStyle) -> Option<String> {
    None
}

/// prompt 里的图那一段：源码 + 节点明细。
///
/// 明细**只写 mermaid 装不下的东西**（body / attrs / 挂在节点上的图外推断），
/// 而且只写有内容的节点 —— 标题不重复第二遍，省 token。
pub fn flow_block(ws: &Workspace, style: &GraphStyle, turn_start: Seq) -> String {
    let Some((lang, src)) = source(&ws.flow, style) else {
        return String::new();
    };
    let mut s = String::from("== 实验流程图 ==\n");
    if ws.flow.view == FlowView::Sketch {
        s.push_str(
            "（这张是你自己画的。它没有节点 id，所以用户只能整段改源码、不能点选单个节点；\
             要让用户能就地改，把图搬回结构化视图。）\n",
        );
    }
    s.push_str(&format!("```{}\n{}\n```\n", lang.tag(), src.trim_end()));

    if ws.flow.view == FlowView::Built {
        let mut detail = String::new();
        for n in ws.flow.nodes.values() {
            let anchored: Vec<String> = ws
                .fields
                .iter()
                .filter(|(_, f)| f.anchor.as_ref() == Some(&n.id))
                .map(|(p, f)| format!("    ↳ {p} = {}  {}", f.value, f.prov.annotate()))
                .collect();
            if n.body.is_empty() && n.attrs.is_empty() && anchored.is_empty() {
                continue;
            }
            let fresh = if n.prov.seq > turn_start { " (本轮)" } else { "" };
            detail.push_str(&format!("{}  {}{fresh}\n", n.id.0, n.prov.annotate()));
            if !n.attrs.is_empty() {
                let kv: Vec<String> =
                    n.attrs.iter().map(|(k, v)| format!("{k} = {v}")).collect();
                detail.push_str(&format!("    {}\n", kv.join("   ")));
            }
            if !n.body.is_empty() {
                detail.push_str(&format!("    {}\n", n.body.trim()));
            }
            for a in anchored {
                detail.push_str(&a);
                detail.push('\n');
            }
        }
        if !detail.is_empty() {
            s.push_str("\n== 节点明细（只列有细节的）==\n");
            s.push_str(&detail);
        }
    }
    s
}
