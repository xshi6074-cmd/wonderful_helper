//! 真实链路探针。**填好密钥之后跑它**，不进常规测试套件。
//!
//! ```text
//! export ANTHROPIC_API_KEY=sk-...        # 或写进 <目录>/secrets.json
//! cargo run --bin live -- [目录] [要问的话]
//! ```
//!
//! # 它跟 39 个链路场景的分工
//!
//! 那些场景用假模型、假网络，验的是**控制流**：仲裁、恢复、截断、闸门。
//! 这个探针用真模型、真网络，验的是那批场景**永远验不到**的东西：
//! 密钥对不对、模型名存不存在、消息映射合不合 API 的规矩、SSE 解得对不对、
//! 工具 schema 模型认不认。这几样全是「本地全绿、一上线全错」的重灾区。
//!
//! 分五步，**每一步单独报结果**：任何一步挂了都能立刻知道是哪一段的问题，
//! 而不是笼统的「跑不起来」。

use premortem::config::{CONFIG_FILE, SECRETS_FILE, Secrets, Settings, real_env};
use premortem::core::{CoreDeps, start};
use premortem::event::Body;
use premortem::ids::SessionId;
use premortem::memory::Memory;
use premortem::model::Mode;
use premortem::msg::{SendMode, UiEvent};
use premortem::persist::spawn_writer;
use premortem::store::{SqliteStore, Store};
use premortem::tools::Registry;
use premortem::{toolkit, web};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let mut args = std::env::args().skip(1);
    let dir: PathBuf = args.next().unwrap_or_else(|| ".".into()).into();
    let question: String = {
        let rest: Vec<String> = args.collect();
        if rest.is_empty() {
            "我想复现 CLIP 的零样本分类，先从哪一步开始比较稳？".into()
        } else {
            rest.join(" ")
        }
    };
    let dir = dir.canonicalize().unwrap_or(dir);
    std::fs::create_dir_all(&dir).ok();

    println!("premortem 真实链路探针");
    println!("目录：{}", dir.display());
    println!("{}", "═".repeat(64));

    // ── 1. 配置 ──
    step(1, "读配置");
    let settings = Settings::load(&dir, &real_env);
    let (secrets, warn) = Secrets::load(&dir);
    print!("{}", settings.describe(&secrets, &real_env));
    for w in warn {
        println!("  ! {w}");
    }
    let missing = settings.missing_keys(&secrets, &real_env);
    if !missing.is_empty() {
        bad(&format!("这些 provider 还没有密钥：{}", missing.join(" / ")));
        println!(
            "\n填法二选一：\n  · 环境变量：{}\n  · 或写进 {}：\n      {{ \"keys\": {{ \"{}\": \"sk-...\" }} }}\n\
             \n模型花名册在 {}/{}，改完直接重跑。",
            missing
                .iter()
                .filter_map(|n| settings.providers.get(n).map(|p| format!("export {}=sk-...", p.key_env)))
                .collect::<Vec<_>>()
                .join("  或  "),
            SECRETS_FILE,
            missing[0],
            dir.display(),
            CONFIG_FILE,
        );
        std::process::exit(1);
    }
    good("密钥齐了");

    // ── 2. 建模型客户端 ──
    step(2, "建模型客户端");
    let models = match settings.build_models(&secrets, &real_env) {
        Ok(m) => {
            good("三个角色都建起来了");
            Arc::new(m)
        }
        Err(e) => {
            bad(&e);
            std::process::exit(1);
        }
    };

    // ── 3. 工具链 ──
    step(3, "工具链");
    let key = settings.web_key(&secrets, &real_env);
    let backend = web::build(&settings.web, &settings.tools, key);
    match &backend {
        None => println!("  · 没有联网后端（config.json 的 web 段），只有本地文件工具"),
        Some(b) => {
            let t = CancellationToken::new();
            match b.probe(&t).await {
                Ok(m) => {
                    for l in m.lines() {
                        println!("  · {l}");
                    }
                }
                Err(e) => warn_(&format!("探活失败：{e}")),
            }
        }
    }
    let deps = match toolkit::from_settings(&settings, &dir, backend) {
        Ok(d) => d,
        Err(e) => {
            bad(&format!("工具链起不来：{e}"));
            std::process::exit(1);
        }
    };
    let registry = toolkit::register(Registry::new(), &deps);
    let names = registry.names();
    println!("  · 工具：{}", names.join(", "));
    println!("  · 可读目录：{}", deps.policy.roots().iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "));
    good("工具链就绪");

    // ── 4. 起 Core ──
    step(4, "起会话");
    let db = dir.join("premortem.db");
    let store: Arc<dyn Store> = match SqliteStore::open(&db) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            bad(&format!("打不开 {}：{e}", db.display()));
            std::process::exit(1);
        }
    };
    let memory = Arc::new(Memory::load_or_bootstrap(&dir.join("memory")).await);
    for (f, e) in &memory.warnings {
        warn_(&format!("{f}：{e}"));
    }
    let session = SessionId::new();
    let (ui_tx, mut ui) = tokio::sync::broadcast::channel(8192);
    let (writer, writer_join) = spawn_writer(store.clone(), ui_tx.clone());
    let mut cd = CoreDeps::new(
        models,
        Arc::new(registry),
        session.clone(),
        memory,
        writer,
        Mode::Explore,
    );
    cd.memory_dir = Some(dir.join("memory"));
    cd.ui = Some(ui_tx);
    let started = start(cd);
    good(&format!("会话 {}", session.0));

    // ── 5. 真跑一轮 ──
    step(5, "跑一轮");
    println!("  问：{question}\n");
    let began = Instant::now();
    started.handle.session_send(&question, SendMode::Queue).await;

    let mut streamed = 0usize;
    let mut failed: Option<String> = None;
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            failed = Some("180 秒没跑完，放弃".into());
            break;
        }
        let ev = match tokio::time::timeout(left, ui.recv()).await {
            Ok(Ok(e)) => e,
            Ok(Err(_)) => break,
            Err(_) => {
                failed = Some("超时".into());
                break;
            }
        };
        match ev {
            UiEvent::Delta { text, .. } => {
                streamed += text.chars().count();
                print!("{text}");
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
            UiEvent::Appended { body, .. } => match *body {
                Body::Judged { scene, rationale } => {
                    println!("\n  [判断段] 场景 {scene} —— {rationale}");
                }
                Body::Called { calls, .. } => {
                    for c in calls {
                        println!("\n  [工具] {} {}", c.name, compact(&c.args.to_string()));
                    }
                }
                Body::Returned { name, content, outcome, .. } => {
                    println!("  [返回] {name} · {outcome} · {}", compact(&content));
                }
                Body::Inferred { ops, dropped } => {
                    println!("\n  [推断] 生效 {} 条，丢弃 {} 条", ops.len(), dropped.len());
                }
                _ => {}
            },
            UiEvent::CostTick { role, usage, session_total } => {
                println!(
                    "\n  [计费] {role:?} 输入 {} / 输出 {}（本会话累计 {session_total}）",
                    usage.prompt, usage.completion
                );
            }
            UiEvent::PersistDegraded { why, .. } => warn_(&format!("落盘降级：{why}")),
            UiEvent::TurnClosed { aborted, .. } => {
                println!();
                if aborted {
                    failed = Some("轮次是中止收尾的".into());
                }
                break;
            }
            _ => {}
        }
    }

    // ── 收尾与结算 ──
    started.handle.session_shutdown().await;
    let summary = started.join.await.ok();
    let _ = writer_join.await;

    println!("\n{}", "═".repeat(64));
    match &failed {
        Some(e) => bad(e),
        None => good(&format!("一轮跑完，用时 {:.1}s，流式收到 {streamed} 字", began.elapsed().as_secs_f32())),
    }
    if let Some(s) = &summary {
        println!("  事件 {} 条 · 成本 {} token", s.events.len(), s.cost.total());
        let tags: Vec<String> = s.events.iter().map(|e| e.body.tag().to_string()).collect();
        println!("  时间线：{}", tags.join(" → "));
        if !s.ws.flow.nodes.is_empty() {
            println!(
                "  推断图：{} 个节点 / {} 条边",
                s.ws.flow.nodes.len(),
                s.ws.flow.edges.len()
            );
        }
        if !s.ws.fields.is_empty() {
            println!("  图外推断：{} 条", s.ws.fields.len());
        }
    }
    println!("  {}", deps.metrics.line());
    println!("  会话存在 {}（{}）", db.display(), session.0);
    if failed.is_some() {
        std::process::exit(1);
    }
}

fn step(n: u8, what: &str) {
    println!("\n[{n}/5] {what}");
}

fn good(m: &str) {
    println!("  ✓ {m}");
}

fn warn_(m: &str) {
    println!("  ! {m}");
}

fn bad(m: &str) {
    println!("  ✗ {m}");
}

/// 一行以内的摘要。工具返回可能几千字，这里只要看得出「回来了什么」。
fn compact(s: &str) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= 100 {
        one
    } else {
        format!("{}… ({} 字)", one.chars().take(100).collect::<String>(), s.chars().count())
    }
}
