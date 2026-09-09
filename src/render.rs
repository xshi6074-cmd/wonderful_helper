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
use crate::state::{FlowView, Graph, Lang, Node, Origin, Workspace};
use serde::Serialize;
use std::fmt::Write as _;

/// 浏览器渲染 Mermaid 时所需的全部派生数据。`Workspace.flow` 仍是唯一真源；
/// 这里没有任何可写回字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GraphRenderPayload {
    pub source: String,
    pub bindings: Vec<GraphBinding>,
    pub layout: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GraphBinding {
    pub marker: String,
    pub kind: GraphBindingKind,
    pub id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub classes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphBindingKind {
    Node,
    Edge,
    Group,
}

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
    graph_render(g, style)
        .map(|p| p.source)
        .unwrap_or_else(|| "flowchart TD\n".into())
}

/// Built 图的源码、稳定身份映射与安全回退报告。
pub fn graph_render(g: &Graph, style: &GraphStyle) -> Option<GraphRenderPayload> {
    if g.view != FlowView::Built || g.nodes.is_empty() {
        return None;
    }

    let mut warnings = Vec::new();
    let dialect = allowed(
        g.render.get("dialect"),
        &["flowchart", "graph"],
        "flowchart",
        "dialect",
        &mut warnings,
    );
    let dir = allowed(
        g.render.get("dir"),
        &["TD", "TB", "BT", "LR", "RL"],
        "TD",
        "dir",
        &mut warnings,
    );
    let layout = allowed(
        g.render.get("layout"),
        &["elk", "dagre"],
        "elk",
        "layout",
        &mut warnings,
    )
    .to_string();
    for key in g.render.keys().filter(|k| k.starts_with("classdef.")) {
        warnings.push(format!(
            "已忽略旧的自由样式 {key}；请改用受限的 visual.* 属性"
        ));
    }

    let mut out = format!("{dialect} {dir}\n");
    let mut bindings = Vec::new();
    emit_class_defs(&mut out);

    // 节点从顶层递归写入 subgraph；BTreeMap 让源码逐字节确定。
    for n in g.nodes.values().filter(|n| n.parent.is_none()) {
        emit_node(g, style, n, 1, &mut out, &mut bindings, &mut warnings);
    }

    // 边使用 Mermaid 的显式 edge id。业务 id 只通过 marker/binding 回到应用，
    // 前端不拆 Mermaid 自己生成的 DOM id。
    for e in g.edges.values() {
        let from = diagram_id("n", &e.from.0);
        let to = diagram_id("n", &e.to.0);
        let eid = diagram_id("e", &e.id.0);
        // ELK 会把显式 edge id 同时放在 path id 与 label 的 data-id 上；
        // 这个值由 binding 直接给前端，不靠 DOM 顺序或拆自动 id 猜业务身份。
        let marker = eid.clone();
        let conn = safe_connector(style.edge_for(&e.kind), &e.kind, &mut warnings);
        if e.label.is_empty() {
            let _ = writeln!(out, "  {from} {eid}@{conn} {to}");
        } else {
            let _ = writeln!(out, "  {from} {eid}@{conn}|\"{}\"| {to}", esc(&e.label));
        }
        emit_class(&mut out, &eid, &marker);
        emit_class(&mut out, &eid, edge_class(&e.kind));
        if e.prov.is_dashed() {
            emit_class(&mut out, &eid, "pm_guess");
        }
        if e.prov.origin == Origin::User {
            emit_class(&mut out, &eid, "pm_user");
        }
        let mut classes = vec![edge_class(&e.kind).to_string()];
        if e.prov.is_dashed() {
            classes.push("pm_guess".into());
        }
        if e.prov.origin == Origin::User {
            classes.push("pm_user".into());
        }
        bindings.push(GraphBinding {
            marker,
            kind: GraphBindingKind::Edge,
            id: e.id.0.clone(),
            classes,
        });
    }

    Some(GraphRenderPayload {
        source: out,
        bindings,
        layout,
        warnings,
    })
}

fn allowed<'a>(
    value: Option<&'a String>,
    choices: &[&str],
    fallback: &'a str,
    key: &str,
    warnings: &mut Vec<String>,
) -> &'a str {
    match value.map(String::as_str) {
        Some(v) if choices.contains(&v) => v,
        Some(v) => {
            warnings.push(format!("未知 {key}={v}，已回退为 {fallback}"));
            fallback
        }
        None => fallback,
    }
}

fn emit_class_defs(out: &mut String) {
    // 这些 classDef 在 Mermaid 量字与布局前生效。这里的颜色是 SVG 独立使用时的
    // 安全回退；应用 CSS 只按同名 class 覆盖明暗主题颜色，不在事后改变字号。
    const DEFS: &[(&str, &str)] = &[
        (
            "pm_kind_module",
            "fill:#eef0ff,stroke:#6459c7,color:#1b2433,stroke-width:1.6px,font-size:15px",
        ),
        (
            "pm_kind_data",
            "fill:#e7f4fb,stroke:#277da1,color:#1b2433,stroke-width:1.6px,font-size:15px",
        ),
        (
            "pm_kind_op",
            "fill:#e7f6f2,stroke:#23816f,color:#1b2433,stroke-width:1.5px,font-size:14px",
        ),
        (
            "pm_kind_loss",
            "fill:#fbeceb,stroke:#b85a54,color:#1b2433,stroke-width:1.8px,font-size:15px",
        ),
        (
            "pm_kind_metric",
            "fill:#fff4d8,stroke:#a96f16,color:#1b2433,stroke-width:1.5px,font-size:14px",
        ),
        (
            "pm_kind_branch",
            "fill:#f0edf9,stroke:#7867a6,color:#1b2433,stroke-width:1.4px,font-size:14px",
        ),
        (
            "pm_kind_note",
            "fill:#eef1f5,stroke:#7b8494,color:#1b2433,stroke-width:1.2px,font-size:13px",
        ),
        (
            "pm_tone_indigo",
            "fill:#eef0ff,stroke:#6459c7,color:#202044",
        ),
        ("pm_tone_blue", "fill:#e7f4fb,stroke:#277da1,color:#153447"),
        ("pm_tone_teal", "fill:#e7f6f2,stroke:#23816f,color:#173b34"),
        ("pm_tone_amber", "fill:#fff4d8,stroke:#a96f16,color:#49300c"),
        ("pm_tone_rose", "fill:#fbeceb,stroke:#b85a54,color:#4b2220"),
        ("pm_tone_slate", "fill:#eef1f5,stroke:#687386,color:#252c38"),
        ("pm_emphasis_primary", "stroke-width:3px,font-weight:700"),
        ("pm_emphasis_secondary", "stroke-width:2px,font-weight:600"),
        ("pm_emphasis_muted", "opacity:.68"),
        ("pm_text_sm", "font-size:12px"),
        ("pm_text_md", "font-size:15px"),
        ("pm_text_lg", "font-size:18px,font-weight:650"),
        (
            "pm_group_module",
            "fill:#f4f3fb,stroke:#8578c4,color:#1b2433,stroke-width:2px,font-size:16px,font-weight:650",
        ),
        (
            "pm_group_section",
            "fill:#f8f9fc,stroke:#aab1be,color:#697386,stroke-width:1px,stroke-dasharray:2 4,font-size:13px",
        ),
        (
            "pm_edge_flow",
            "stroke:#5a6678,stroke-width:1.8px,color:#5a6678",
        ),
        (
            "pm_edge_supervises",
            "stroke:#a64f61,stroke-width:2.8px,color:#a64f61",
        ),
        (
            "pm_edge_compares",
            "stroke:#8b6a19,stroke-width:1.6px,color:#8b6a19",
        ),
        (
            "pm_edge_depends",
            "stroke:#35766c,stroke-width:1.5px,color:#35766c",
        ),
        ("pm_guess", "stroke-dasharray:7 5"),
        ("pm_user", "stroke:#6f57cf"),
    ];
    for (name, def) in DEFS {
        let _ = writeln!(out, "  classDef {name} {def}");
    }
}

fn emit_node(
    g: &Graph,
    style: &GraphStyle,
    n: &Node,
    depth: usize,
    out: &mut String,
    bindings: &mut Vec<GraphBinding>,
    warnings: &mut Vec<String>,
) {
    let pad = "  ".repeat(depth);
    let kids = g.children(&n.id);
    let nid = diagram_id("n", &n.id.0);
    let marker = marker(&n.id.0);
    if kids.is_empty() {
        let shape = node_shape(n, style, warnings);
        out.push_str(&format!(
            "{pad}{nid}{}\n",
            shape.replace("%s", &quoted(&node_label(n)))
        ));
        emit_class(out, &nid, &marker);
        emit_class(out, &nid, node_kind_class(&n.kind));
        emit_visual_classes(out, &nid, n, warnings);
        if n.prov.is_dashed() {
            emit_class(out, &nid, "pm_guess");
        }
        if n.prov.origin == Origin::User {
            emit_class(out, &nid, "pm_user");
        }
        if let Some(def) = style.class.get(&n.kind) {
            match sanitize_class_def(def) {
                Some(def) => {
                    let custom = diagram_id("custom", &n.kind);
                    let _ = writeln!(out, "  classDef {custom} {def}");
                    emit_class(out, &nid, &custom);
                }
                None => warnings.push(format!(
                    "kind={} 的 graph.class 含不支持的样式，已忽略",
                    n.kind
                )),
            }
        }
        bindings.push(GraphBinding {
            marker,
            kind: GraphBindingKind::Node,
            id: n.id.0.clone(),
            classes: vec![],
        });
        return;
    }
    out.push_str(&format!(
        "{pad}subgraph {nid}[{}]\n",
        quoted(&node_label(n))
    ));
    for k in kids {
        emit_node(g, style, k, depth + 1, out, bindings, warnings);
    }
    out.push_str(&format!("{pad}end\n"));
    emit_class(out, &nid, &marker);
    let container = visual(
        n,
        "visual.container",
        &["module", "section"],
        "module",
        warnings,
    );
    emit_class(
        out,
        &nid,
        if container == "section" {
            "pm_group_section"
        } else {
            "pm_group_module"
        },
    );
    emit_visual_classes(out, &nid, n, warnings);
    if n.prov.is_dashed() {
        emit_class(out, &nid, "pm_guess");
    }
    if n.prov.origin == Origin::User {
        emit_class(out, &nid, "pm_user");
    }
    bindings.push(GraphBinding {
        marker,
        kind: GraphBindingKind::Group,
        id: n.id.0.clone(),
        classes: vec![],
    });
}

fn emit_class(out: &mut String, id: &str, class: &str) {
    let _ = writeln!(out, "  class {id} {class}");
}

fn emit_visual_classes(out: &mut String, id: &str, n: &Node, warnings: &mut Vec<String>) {
    if let Some(v) = visual_opt(
        n,
        "visual.tone",
        &["indigo", "blue", "teal", "amber", "rose", "slate"],
        warnings,
    ) {
        emit_class(out, id, &format!("pm_tone_{v}"));
    }
    if let Some(v) = visual_opt(
        n,
        "visual.emphasis",
        &["primary", "secondary", "muted"],
        warnings,
    ) {
        emit_class(out, id, &format!("pm_emphasis_{v}"));
    }
    if let Some(v) = visual_opt(n, "visual.text", &["sm", "md", "lg"], warnings) {
        emit_class(out, id, &format!("pm_text_{v}"));
    }
}

fn visual<'a>(
    n: &'a Node,
    key: &str,
    values: &[&str],
    fallback: &'a str,
    warnings: &mut Vec<String>,
) -> &'a str {
    visual_opt(n, key, values, warnings).unwrap_or(fallback)
}

fn visual_opt<'a>(
    n: &'a Node,
    key: &str,
    values: &[&str],
    warnings: &mut Vec<String>,
) -> Option<&'a str> {
    let value = n.attrs.get(key)?;
    if values.contains(&value.as_str()) {
        Some(value)
    } else {
        warnings.push(format!(
            "节点 {} 的 {key}={} 不受支持，已使用默认样式",
            n.id.0, value
        ));
        None
    }
}

fn node_shape<'a>(n: &'a Node, style: &'a GraphStyle, warnings: &mut Vec<String>) -> &'a str {
    if let Some(shape) = n.attrs.get("visual.shape") {
        let preset = match shape.as_str() {
            "rect" => Some("[%s]"),
            "rounded" => Some("(%s)"),
            "pill" => Some("([%s])"),
            "cylinder" => Some("[(%s)]"),
            "diamond" => Some("{%s}"),
            "hexagon" => Some("{{%s}}"),
            "subroutine" => Some("[[%s]]"),
            "circle" => Some("((%s))"),
            _ => None,
        };
        if let Some(v) = preset {
            return v;
        }
        warnings.push(format!(
            "节点 {} 的 visual.shape={} 不受支持，已按 kind 回退",
            n.id.0, shape
        ));
    }
    let shape = style.shape_for(&n.kind);
    if supported_shape(shape) {
        shape
    } else {
        warnings.push(format!(
            "kind={} 的 graph.shape 不安全或不受支持，已回退为 rect",
            n.kind
        ));
        "[%s]"
    }
}

fn supported_shape(s: &str) -> bool {
    matches!(
        s,
        "[%s]" | "(%s)" | "([%s])" | "[(%s)]" | "{%s}" | "{{%s}}" | "[[%s]]" | "((%s))" | "[/%s/]"
    )
}

fn safe_connector<'a>(s: &'a str, kind: &str, warnings: &mut Vec<String>) -> &'a str {
    if matches!(s, "-->" | "==>" | "---" | "--o" | "--x") {
        s
    } else {
        warnings.push(format!("边 kind={kind} 的连接符不受支持，已回退为 -->"));
        "-->"
    }
}

fn node_kind_class(kind: &str) -> &'static str {
    match kind {
        "data" => "pm_kind_data",
        "op" | "gate" => "pm_kind_op",
        "loss" => "pm_kind_loss",
        "metric" => "pm_kind_metric",
        "ablation" | "baseline" => "pm_kind_branch",
        "note" => "pm_kind_note",
        _ => "pm_kind_module",
    }
}

fn edge_class(kind: &str) -> &'static str {
    match kind {
        "supervises" => "pm_edge_supervises",
        "compares" => "pm_edge_compares",
        "depends" | "snapshot" | "copies" => "pm_edge_depends",
        _ => "pm_edge_flow",
    }
}

fn node_label(n: &Node) -> String {
    let mut label = wrap(&n.label, 28);
    if let Some(summary) = n.attrs.get("summary").filter(|s| !s.trim().is_empty()) {
        label.push_str("<br/><small>");
        label.push_str(&wrap(summary, 36));
        label.push_str("</small>");
    }
    label
}

fn wrap(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    for ch in s.chars() {
        if matches!(ch, '\n' | '\r') {
            out.push_str("<br/>");
            col = 0;
            continue;
        }
        let w = if ch.is_ascii() { 1 } else { 2 };
        if col > 0 && col + w > width {
            out.push_str("<br/>");
            col = 0;
        }
        out.push_str(&esc_char(ch));
        col += w;
    }
    out
}

fn sanitize_class_def(s: &str) -> Option<String> {
    let allowed = [
        "fill",
        "stroke",
        "color",
        "stroke-width",
        "font-size",
        "font-weight",
        "stroke-dasharray",
    ];
    let mut clean = Vec::new();
    for item in s.split(',') {
        let (key, value) = item.split_once(':')?;
        let key = key.trim();
        let value = value.trim();
        if !allowed.contains(&key)
            || value.is_empty()
            || !value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "#().% -_".contains(c))
        {
            return None;
        }
        clean.push(format!("{key}:{value}"));
    }
    (!clean.is_empty()).then(|| clean.join(","))
}

fn diagram_id(prefix: &str, value: &str) -> String {
    let mut out = format!("pm{prefix}_");
    for byte in value.as_bytes() {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn marker(value: &str) -> String {
    if value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        format!("pmref_{value}")
    } else {
        diagram_id("ref", value)
    }
}

fn quoted(s: &str) -> String {
    format!("\"{s}\"")
}

/// mermaid 的标签里不能出现裸引号与换行。
fn esc(s: &str) -> String {
    s.chars().map(esc_char).collect()
}

fn esc_char(c: char) -> String {
    match c {
        '"' => "#quot;".into(),
        '&' => "#38;".into(),
        '<' => "#60;".into(),
        '>' => "#62;".into(),
        '\n' | '\r' => " ".into(),
        c if c.is_control() => " ".into(),
        c => c.to_string(),
    }
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
            let fresh = if n.prov.seq > turn_start {
                " (本轮)"
            } else {
                ""
            };
            detail.push_str(&format!("{}  {}{fresh}\n", n.id.0, n.prov.annotate()));
            if !n.attrs.is_empty() {
                let kv: Vec<String> = n.attrs.iter().map(|(k, v)| format!("{k} = {v}")).collect();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{EdgeId, NodeId};
    use crate::state::{Edge, Prov, Source};
    use std::collections::BTreeMap;

    fn prov(source: Option<Source>) -> Prov {
        Prov {
            origin: Origin::Model,
            source,
            confidence: None,
            seq: Seq(7),
        }
    }

    fn node(id: &str, label: &str, parent: Option<&str>) -> Node {
        Node {
            id: NodeId(id.into()),
            kind: "module".into(),
            label: label.into(),
            body: String::new(),
            attrs: BTreeMap::new(),
            parent: parent.map(|p| NodeId(p.into())),
            prov: prov(Some(Source::Repo("src/model.rs:1".into()))),
        }
    }

    #[test]
    fn payload_maps_duplicate_labels_groups_and_explicit_edges_by_marker() {
        let mut g = Graph::default();
        g.nodes
            .insert(NodeId("g1".into()), node("g1", "Generator", None));
        g.nodes
            .insert(NodeId("n1".into()), node("n1", "同名节点", Some("g1")));
        g.nodes
            .insert(NodeId("n2".into()), node("n2", "同名节点", Some("g1")));
        g.edges.insert(
            EdgeId("e1".into()),
            Edge {
                id: EdgeId("e1".into()),
                from: NodeId("n1".into()),
                to: NodeId("n2".into()),
                kind: "supervises".into(),
                label: "teacher signal".into(),
                attrs: BTreeMap::new(),
                prov: prov(Some(Source::Paper("sec. 3".into()))),
            },
        );

        let p = graph_render(&g, &GraphStyle::default()).expect("built graph");
        assert_eq!(p.layout, "elk");
        assert!(
            p.source.contains("pme_6531@==>"),
            "显式 edge id 必须进 Mermaid：{}",
            p.source
        );
        assert!(p.source.contains("pmref_n1") && p.source.contains("pmref_n2"));
        assert_eq!(
            p.bindings
                .iter()
                .filter(|b| b.kind == GraphBindingKind::Node)
                .count(),
            2
        );
        assert_eq!(
            p.bindings
                .iter()
                .filter(|b| b.kind == GraphBindingKind::Group)
                .count(),
            1
        );
        assert_eq!(
            p.bindings
                .iter()
                .filter(|b| b.kind == GraphBindingKind::Edge)
                .count(),
            1
        );
        let n1 = p.bindings.iter().find(|b| b.id == "n1").unwrap();
        let n2 = p.bindings.iter().find(|b| b.id == "n2").unwrap();
        assert_ne!(n1.marker, n2.marker, "同名不影响身份映射");
    }

    #[test]
    fn labels_cannot_inject_mermaid_and_unknown_visual_values_only_warn() {
        let mut g = Graph::default();
        let mut n = node("n1", "标题\"]\nclass evil injected", None);
        n.attrs
            .insert("summary".into(), "完整保留的中文业务说明".into());
        n.attrs
            .insert("visual.tone".into(), "javascript:bad".into());
        g.nodes.insert(n.id.clone(), n);
        g.render.insert("layout".into(), "mystery".into());
        g.render.insert("classdef.evil".into(), "fill:red".into());

        let p = graph_render(&g, &GraphStyle::default()).unwrap();
        assert!(p.source.contains("#quot;]"));
        assert!(p.source.contains("<br/>class evil injected"));
        assert!(p.source.contains("<small>完整保留的中文业务说明</small>"));
        assert!(
            !p.source.contains("\nclass evil injected"),
            "用户换行不能开启新指令"
        );
        assert_eq!(p.layout, "elk");
        assert!(
            p.warnings.len() >= 3,
            "未知样式、布局和旧 classdef 都必须可见：{:?}",
            p.warnings
        );
    }
}
