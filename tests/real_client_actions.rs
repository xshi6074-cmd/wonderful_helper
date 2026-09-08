//! 真实 HTTP 客户端 → 动作工具的**端到端**一条链，对着一个本地假服务跑。
//!
//! # 为什么值得单独测这条
//!
//! 这一整套改动里，唯一没有被 mock 覆盖到的接缝是「真实客户端解出来的 `Call`，
//! `actions::parse` 收不收得下」。之前正是这一段断了：判断段工具的 `ops` 字段
//! schema 是个空对象，模型给什么都不对，而 harness 一声不吭 ——
//! 指标上只是 `inferred_ops: 0`，看起来像模型不爱写推断。
//!
//! 假服务同时验两件事：
//! 1. 判断段的请求体带的是 `tool_choice: "required"` 而不是点名某个工具
//!    （点名在开了 thinking 的模型上会 400）；
//! 2. 回答段流式吐出来的 `record_graph` / `record_note` 调用，
//!    参数原样能被 `actions::parse` 解成 op。

use axum::Router;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::post;
use premortem::actions;
use premortem::config::{Api, ModelCfg, ProviderCfg};
use premortem::client::HttpClient;
use premortem::model::{AnswerReq, JudgeReq, Message, ModelClient, StreamEvent};
use premortem::state::{Op, Source};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// 假服务收到的请求体，测试拿它检查我们发出去的形状。
type Seen = Arc<Mutex<Vec<Value>>>;

async fn chat(State(seen): State<Seen>, body: String) -> impl IntoResponse {
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let streaming = v["stream"].as_bool().unwrap_or(false);
    seen.lock().unwrap().push(v);

    if !streaming {
        // 判断段：一次强制工具调用，参数是 JSON 字符串
        return (
            [("content-type", "application/json")],
            json!({
                "choices": [{ "message": { "tool_calls": [{
                    "id": "j1", "type": "function",
                    "function": {
                        "name": "record_judgement",
                        "arguments": "{\"scenes\":[\"trace_code\",\"cheap_first\"],\"rationale\":\"他要改模块，而且预算对不上\"}"
                    }
                }]}}],
                "usage": { "prompt_tokens": 100, "completion_tokens": 20 }
            })
            .to_string(),
        );
    }

    // 回答段：流式吐两个动作调用，参数分片到达（真实服务就是这么发的）
    let graph = r#"{"ops":[{"op":"node","id":"$enc","kind":"module","label":"编码器","source":{"repo":"src/m.py:1"}},{"op":"node","id":"$l","kind":"loss","label":"对比损失"},{"op":"edge","id":"$e","from":"$enc","to":"$l","kind":"supervises"}]}"#;
    let note = r#"{"ops":[{"op":"set","path":"spec.temp","value":"0.07","anchor":"$l","source":"user"}]}"#;
    let (g1, g2) = graph.split_at(40);
    let sse = format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"g1","function":{"name":"record_graph","arguments":g1}}]}}]}),
        json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"arguments":g2}}]}}]}),
        json!({"choices":[{"delta":{"tool_calls":[
            {"index":1,"id":"n1","function":{"name":"record_note","arguments":note}}]}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":200,"completion_tokens":40}}),
    );
    ([("content-type", "text/event-stream")], sse)
}

async fn stub() -> (String, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new().route("/chat/completions", post(chat)).with_state(seen.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });
    (format!("http://{addr}"), seen)
}

fn client(base: &str) -> HttpClient {
    let p = ProviderCfg {
        api: Api::OpenAiCompat,
        base_url: base.to_string(),
        key_env: "X".into(),
        caps: None,
    };
    let m = ModelCfg {
        provider: "stub".into(),
        model: "stub-1".into(),
        temperature: 0.0,
        max_tokens: 256,
    };
    HttpClient::new(&p, &m, "k".into()).unwrap()
}

#[tokio::test]
async fn judge_asks_for_a_scene_without_naming_the_tool() {
    let (base, seen) = stub().await;
    let out = client(&base)
        .judge(JudgeReq {
            turn: premortem::ids::TurnId(1),
            mode: premortem::model::Mode::Go,
            msgs: vec![Message::system("规则"), Message::user("改一下解码器")],
            token: CancellationToken::new(),
        })
        .await
        .expect("判断段应当成功");
    assert_eq!(out.scenes, vec!["trace_code".to_string(), "cheap_first".to_string()]);
    assert_eq!(out.usage.prompt, 100);

    let body = seen.lock().unwrap()[0].clone();
    // 点名工具在开了 thinking 的模型上会 400，所以必须是 required
    assert_eq!(body["tool_choice"], json!("required"), "判断段不该点名工具：{body}");
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1, "只给一个工具，required 才等价于点名");
    // 判断段只判场景 —— 不该再有 ops 那个字段
    let props = &tools[0]["function"]["parameters"]["properties"];
    assert_eq!(props["scenes"]["type"], "array", "场景是一组，不是一个：{props}");
    assert!(props.get("ops").is_none(), "判断段不写推断：{props}");
}

#[tokio::test]
async fn streamed_action_calls_parse_into_ops() {
    let (base, _seen) = stub().await;
    use futures_util::StreamExt;
    let mut st = client(&base)
        .stream(AnswerReq {
            turn: premortem::ids::TurnId(1),
            mode: premortem::model::Mode::Go,
            msgs: vec![Message::user("看看")],
            tools: actions::specs(),
            token: CancellationToken::new(),
        })
        .await
        .expect("开流");

    let mut calls = Vec::new();
    while let Some(ev) = st.next().await {
        if let StreamEvent::ToolCalls(cs) = ev {
            calls = cs;
        }
    }
    assert_eq!(calls.len(), 2, "两个动作调用都解出来了");
    assert_eq!(calls[0].name, actions::RECORD_GRAPH);
    assert_eq!(calls[1].name, actions::RECORD_NOTE);

    // ★ 接缝：真实客户端解出来的参数，actions::parse 收得下
    let (g, bad) = actions::parse(&calls[0]);
    assert!(bad.is_empty(), "分片拼起来的参数应当能解析：{bad:?}");
    assert_eq!(g.len(), 3);
    assert!(g.iter().all(actions::is_graph_op));
    match &g[0] {
        Op::Node { source, label, .. } => {
            assert_eq!(label.as_deref(), Some("编码器"));
            assert_eq!(source.as_ref().unwrap(), &Source::Repo("src/m.py:1".into()));
        }
        o => panic!("{o:?}"),
    }

    let (n, bad) = actions::parse(&calls[1]);
    assert!(bad.is_empty(), "{bad:?}");
    assert_eq!(n.len(), 1);
    assert!(!actions::is_graph_op(&n[0]), "set 是图外 op");
}
