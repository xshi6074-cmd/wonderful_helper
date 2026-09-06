//! Independent flow-acceptance contracts from the architecture re-audit.
//!
//! These tests assert user-visible ordering, durability, replay, and timeout
//! semantics. They intentionally do not mirror the current implementation.
//! If a contract is disputed, settle the design first; do not weaken a failing
//! assertion merely to make the suite green.

use premortem::context::{ContextLimit, split_at_recent};
use premortem::core::{self, CoreDeps};
use premortem::event::{Body, Event, assemble};
use premortem::ids::{Seq, SessionId, TurnId};
use premortem::memory::Memory;
use premortem::mock::{MockModel, SlowTool, call, default_judge};
use premortem::model::{Mode, Models, StreamEvent, Usage};
use premortem::msg::{SendMode, UiEvent};
use premortem::persist::{Restored, restore, spawn_writer};
use premortem::scene::Playbook;
use premortem::state::Op;
use premortem::state::Workspace;
use premortem::store::{SqliteStore, Store, StoreError};
use premortem::tools::{Registry, ToolConfig, ToolCtx, run_tools};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// 这些用例都在一条根会话上跑。适配 session 化 API 时补的，未改动任何断言。
fn sid() -> SessionId {
    SessionId::from("probe")
}

struct Rig {
    handle: premortem::CoreHandle,
    ui: broadcast::Receiver<UiEvent>,
    core_join: tokio::task::JoinHandle<core::CoreSummary>,
    writer_join: tokio::task::JoinHandle<()>,
}

/// Blocks the transaction containing TurnClosed so the test can observe whether
/// the UI publishes a durable-looking close before the store commit completes.
struct CloseGate {
    entered: AtomicBool,
    released: Mutex<bool>,
    cv: Condvar,
}

impl CloseGate {
    fn new() -> Self {
        Self {
            entered: AtomicBool::new(false),
            released: Mutex::new(false),
            cv: Condvar::new(),
        }
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.cv.notify_all();
    }
}

struct BlockingCloseStore {
    inner: Arc<dyn Store>,
    gate: Arc<CloseGate>,
}

impl Store for BlockingCloseStore {
    fn append(&self, events: &[Event]) -> Result<(), StoreError> {
        if events
            .iter()
            .any(|e| matches!(e.body, Body::TurnClosed { .. }))
        {
            self.gate.entered.store(true, Ordering::SeqCst);
            let mut released = self.gate.released.lock().unwrap();
            while !*released {
                released = self.gate.cv.wait(released).unwrap();
            }
        }
        self.inner.append(events)
    }

    fn create_session(&self, s: &premortem::ids::Session) -> Result<(), StoreError> {
        self.inner.create_session(s)
    }

    fn get_session(&self, id: &SessionId) -> Result<Option<premortem::ids::Session>, StoreError> {
        self.inner.get_session(id)
    }

    fn list_sessions(&self) -> Result<Vec<premortem::ids::Session>, StoreError> {
        self.inner.list_sessions()
    }

    fn load_chain(&self, id: &SessionId) -> Result<Vec<Event>, StoreError> {
        self.inner.load_chain(id)
    }

    fn latest_checkpoint(&self, id: &SessionId) -> Result<Option<(Seq, Workspace)>, StoreError> {
        self.inner.latest_checkpoint(id)
    }

    fn put_checkpoint(&self, id: &SessionId, seq: Seq, ws: &Workspace) -> Result<(), StoreError> {
        self.inner.put_checkpoint(id, seq, ws)
    }
}

fn answer(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::Chunk(text.to_string()),
        StreamEvent::Done(Usage::default()),
    ]
}

fn build(
    model: Arc<MockModel>,
    store: Arc<dyn Store>,
    restored: Option<Restored>,
    checkpoint_every: u64,
) -> Rig {
    let _ = &store;
    let (ui_tx, _) = broadcast::channel(128);
    let (writer, writer_join) = spawn_writer(store.clone(), ui_tx.clone());
    let mut deps = CoreDeps::new(
        Arc::new(Models::uniform(model)),
        Arc::new(Registry::new()),
        sid(),
        Arc::new(Memory::empty()),
        writer,
        Mode::Explore,
    );
    deps.ui = Some(ui_tx);
    deps.restored = restored;
    deps.checkpoint_every = checkpoint_every;
    deps.context_limit = ContextLimit::default();
    let started = core::start(deps);
    Rig {
        handle: started.handle,
        ui: started.ui,
        core_join: started.join,
        writer_join,
    }
}

async fn wait_closed(ui: &mut broadcast::Receiver<UiEvent>) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if matches!(ui.recv().await, Ok(UiEvent::TurnClosed { .. })) {
                break;
            }
        }
    })
    .await
    .expect("turn did not close");
}

async fn stop(rig: Rig) {
    rig.handle.session_shutdown().await;
    let _ = rig.core_join.await;
    let _ = rig.writer_join.await;
}

#[tokio::test]
async fn normal_turn_round_trips_through_a_reopened_sqlite_file() {
    let path = std::env::temp_dir().join(format!(
        "premortem-flow-acceptance-{}.db",
        uuid::Uuid::new_v4()
    ));
    let sqlite = Arc::new(SqliteStore::open(&path).unwrap());
    let store: Arc<dyn Store> = sqlite.clone();
    let model = Arc::new(
        MockModel::new()
            .on_judge(default_judge())
            .on_answer(answer("persisted")),
    );
    let mut rig = build(model, store, None, 200);
    let ack = rig
        .handle
        .session_send("round-trip", SendMode::Queue)
        .await
        .unwrap();
    assert!(ack.durable, "accepted user input was not durable");
    wait_closed(&mut rig.ui).await;
    stop(rig).await;
    drop(sqlite);

    let reopened = SqliteStore::open(&path).unwrap();
    let restored = restore(&reopened, &sid()).unwrap();
    assert!(
        restored
            .events
            .iter()
            .any(|e| matches!(&e.body, Body::Said { text, .. } if text == "round-trip"))
    );
    assert!(
        restored
            .events
            .iter()
            .any(|e| matches!(&e.body, Body::Wrote { text, .. } if text == "persisted"))
    );
    assert!(
        restored
            .events
            .iter()
            .any(|e| matches!(e.body, Body::TurnClosed { .. }))
    );
    drop(reopened);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn accepted_inference_is_visible_to_same_turn_answer() {
    // 推断由回答段的动作工具写，所以看的是**下一次**回答调用的 prompt。
    let model = Arc::new(
        MockModel::new()
            .on_judge(default_judge())
            .on_ops(&[Op::set("audit.fact", "new-value")])
            .on_answer(answer("ok")),
    );
    let store: Arc<dyn Store> = Arc::new(SqliteStore::memory().unwrap());
    let mut rig = build(model.clone(), store, None, 200);
    rig.handle.session_send("infer it", SendMode::Queue).await;
    wait_closed(&mut rig.ui).await;
    let prompt = model.answer_prompt(1);
    stop(rig).await;
    assert!(
        prompt.contains("audit.fact") && prompt.contains("new-value"),
        "accepted inference missing from same-turn answer prompt: {prompt}"
    );
}

#[tokio::test]
async fn triggering_user_input_appears_once_in_each_model_prompt() {
    let marker = "only-once-trigger-6c2e";
    let model = Arc::new(
        MockModel::new()
            .on_judge(default_judge())
            .on_answer(answer("ok")),
    );
    let store: Arc<dyn Store> = Arc::new(SqliteStore::memory().unwrap());
    let mut rig = build(model.clone(), store, None, 200);
    rig.handle.session_send(marker, SendMode::Queue).await;
    wait_closed(&mut rig.ui).await;
    let judge = model.judge_prompt(0);
    let answer = model.answer_prompt(0);
    stop(rig).await;
    assert_eq!(
        judge.matches(marker).count(),
        1,
        "trigger duplicated in judge prompt: {judge}"
    );
    assert_eq!(
        answer.matches(marker).count(),
        1,
        "trigger duplicated in answer prompt: {answer}"
    );
}

/// # 已裁决：UI 的 `TurnClosed` **有意**领先于落盘回执
///
/// 审计原版要求「这批事务提交之后才推给 UI」。它与一条更强的、已经验证过的性质冲突
/// （`src/main.rs` 的 S14：落盘失败不阻断对话）：盘写不进去时 writer 会**无限重试**，
/// 若 UI 要等提交，一块坏盘会让界面永远停在「正在停止」，用户连新的一轮都开不了。
///
/// 取舍已定：**显示可以领先于磁盘，对话不能被磁盘卡住。** 同一取舍还落在别处 ——
/// `Wrote`（正文本身）也是先推 UI 后落盘；只把 `TurnClosed` 单独卡住并不能消除
/// 这一类不一致，只会让它更难解释。
///
/// 耐久度是**另一路信号**，不混进 UI 事件：用户输入用 `Ack.durable`（提交后才回），
/// 出问题用 `UiEvent::PersistDegraded`。
///
/// 所以断言反过来写 —— 守的是这个决定本身，以及它换来的那件事（Core 不被磁盘拖住）。
#[tokio::test]
async fn ui_close_leads_the_store_commit_and_the_disk_never_blocks_the_core() {
    let base: Arc<dyn Store> = Arc::new(SqliteStore::memory().unwrap());
    let gate = Arc::new(CloseGate::new());
    let store: Arc<dyn Store> = Arc::new(BlockingCloseStore {
        inner: base.clone(),
        gate: gate.clone(),
    });
    let model = Arc::new(
        MockModel::new()
            .on_judge(default_judge())
            .on_answer(answer("ok")),
    );
    let mut rig = build(model, store, None, 200);
    rig.handle
        .session_send("close ordering", SendMode::Queue)
        .await;

    tokio::time::timeout(Duration::from_secs(2), async {
        while !gate.entered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("writer never reached the TurnClosed transaction");

    // (1) 显示领先于磁盘：事务还卡在 append 里，UI 已经收到收尾。
    let mut close_seen_before_commit = false;
    while let Ok(event) = rig.ui.try_recv() {
        if matches!(event, UiEvent::TurnClosed { .. }) {
            close_seen_before_commit = true;
        }
    }
    assert!(
        close_seen_before_commit,
        "TurnClosed 没能在落盘之前推给 UI —— 坏盘会把界面冻在「正在停止」"
    );

    // (2) 这才是 (1) 换来的东西：盘卡死时 Core 仍然应答。
    tokio::time::timeout(Duration::from_secs(1), rig.handle.session_snapshot())
        .await
        .expect("盘卡住时 Core 不再应答 —— 落盘失败阻断了对话");

    gate.release();
    stop(rig).await;

    // (3) 领先不等于丢失：闸门放开后，那条 TurnClosed 确实落到了盘上。
    let events = base.load_chain(&sid()).unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e.body, Body::TurnClosed { .. })),
        "TurnClosed 推给了 UI 却始终没有落盘"
    );
}

#[test]
fn folded_event_replaces_covered_messages() {
    let events = vec![
        Event {
            session: sid(),
            seq: Seq(1),
            turn: None,
            at_ms: 1,
            corr: None,
            body: Body::Said {
                client_id: "c1".into(),
                text: "old-user".into(),
            },
        },
        Event {
            session: sid(),
            seq: Seq(2),
            turn: Some(TurnId(1)),
            at_ms: 2,
            corr: None,
            body: Body::Wrote {
                text: "old-answer".into(),
                interrupted: false,
            },
        },
        Event {
            session: sid(),
            seq: Seq(3),
            turn: Some(TurnId(2)),
            at_ms: 3,
            corr: None,
            body: Body::Folded {
                from: Seq(1),
                to: Seq(2),
                summary: "summary-only".into(),
                folded: 2,
            },
        },
    ];
    let messages = assemble(&events);
    let text = messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("|");
    assert_eq!(
        messages.len(),
        1,
        "covered originals were not replaced: {text}"
    );
    assert!(text.contains("summary-only"));
}

#[test]
fn recent_turn_keeps_its_triggering_user_event_during_compaction() {
    let events = vec![
        Event {
            session: sid(),
            seq: Seq(1),
            turn: None,
            at_ms: 1,
            corr: None,
            body: Body::Said {
                client_id: "old".into(),
                text: "old trigger".into(),
            },
        },
        Event {
            session: sid(),
            seq: Seq(2),
            turn: Some(TurnId(1)),
            at_ms: 2,
            corr: None,
            body: Body::TurnOpened {
                mode: Mode::Explore,
            },
        },
        Event {
            session: sid(),
            seq: Seq(3),
            turn: Some(TurnId(1)),
            at_ms: 3,
            corr: None,
            body: Body::Wrote {
                text: "old answer".into(),
                interrupted: false,
            },
        },
        Event {
            session: sid(),
            seq: Seq(4),
            turn: None,
            at_ms: 4,
            corr: None,
            body: Body::Said {
                client_id: "new".into(),
                text: "latest trigger".into(),
            },
        },
        Event {
            session: sid(),
            seq: Seq(5),
            turn: Some(TurnId(2)),
            at_ms: 5,
            corr: None,
            body: Body::TurnOpened {
                mode: Mode::Explore,
            },
        },
        Event {
            session: sid(),
            seq: Seq(6),
            turn: Some(TurnId(2)),
            at_ms: 6,
            corr: None,
            body: Body::Wrote {
                text: "latest answer".into(),
                interrupted: false,
            },
        },
    ];

    let split = split_at_recent(&events, 1);
    assert!(
        events[split..]
            .iter()
            .any(|e| matches!(&e.body, Body::Said { text, .. } if text == "latest trigger")),
        "recent-turn retention orphaned the answer from its triggering user event"
    );
}

#[tokio::test]
async fn hard_limit_is_independent_of_heartbeat() {
    let registry =
        Registry::new().with(Arc::new(SlowTool::parallel("slow", Duration::from_secs(5))));
    let (ui, _) = broadcast::channel(8);
    let ctx = ToolCtx {
        token: tokio_util::sync::CancellationToken::new(),
        ui,
        registry: Arc::new(registry),
        next_task: Arc::new(AtomicU64::new(1)),
        config: ToolConfig {
            heartbeat: Duration::from_millis(250),
            hard_limit: Duration::from_millis(20),
        },
    };
    let started = Instant::now();
    let _ = run_tools(vec![call("slow-1", "slow")], &ctx).await;
    assert!(
        started.elapsed() < Duration::from_millis(120),
        "20ms hard limit waited {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn shutdown_during_stream_persists_partial_and_terminal_state() {
    let model = Arc::new(
        MockModel::new()
            .on_judge(default_judge())
            .on_answer(vec![
                StreamEvent::Chunk("half".into()),
                StreamEvent::Chunk("tail".into()),
                StreamEvent::Done(Usage::default()),
            ])
            .chunk_delay(Duration::from_millis(80)),
    );
    let store: Arc<dyn Store> = Arc::new(SqliteStore::memory().unwrap());
    let mut rig = build(model, store.clone(), None, 200);
    rig.handle.session_send("start", SendMode::Queue).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if matches!(rig.ui.recv().await, Ok(UiEvent::Delta { .. })) {
                break;
            }
        }
    })
    .await
    .unwrap();
    stop(rig).await;
    let events = store.load_chain(&sid()).unwrap();
    assert!(
        events.iter().any(
            |e| matches!(&e.body, Body::Wrote { text, interrupted: true } if text.contains("half"))
        ),
        "partial stream was lost on graceful shutdown"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e.body, Body::TurnClosed { .. })),
        "graceful shutdown left the turn open"
    );
}

#[tokio::test]
async fn old_command_id_remains_idempotent_across_restart() {
    let sqlite = Arc::new(SqliteStore::memory().unwrap());
    let first = uuid::Uuid::new_v4();
    let mut events = Vec::new();
    for i in 0..257u64 {
        let id = if i == 0 { first } else { uuid::Uuid::new_v4() };
        events.push(Event {
            session: sid(),
            seq: Seq(i + 1),
            turn: None,
            at_ms: i,
            corr: None,
            body: Body::Said {
                client_id: id.to_string(),
                text: format!("m{i}"),
            },
        });
    }
    sqlite.append(&events).unwrap();
    let restored = restore(sqlite.as_ref(), &sid()).unwrap();
    let model = Arc::new(MockModel::new().judge_delay(Duration::from_secs(1)));
    let store: Arc<dyn Store> = sqlite;
    let rig = build(model, store, Some(restored), 200);
    let ack = rig
        .handle
        .session_resend(first, "m0", SendMode::Queue)
        .await;
    stop(rig).await;
    assert!(
        ack.is_none(),
        "old duplicate was accepted instead of deduplicated: {ack:?}"
    );
}

#[tokio::test]
async fn scene_override_survives_restart_until_consumed() {
    let sqlite = Arc::new(SqliteStore::memory().unwrap());
    sqlite
        .append(&[Event {
            session: sid(),
            seq: Seq(1),
            turn: None,
            at_ms: 1,
            corr: None,
            body: Body::SceneOverridden {
                from: "none".into(),
                to: "trace_code".into(),
            },
        }])
        .unwrap();
    let restored = restore(sqlite.as_ref(), &sid()).unwrap();
    let model = Arc::new(
        MockModel::new()
            .on_judge(default_judge())
            .on_answer(answer("ok")),
    );
    let store: Arc<dyn Store> = sqlite;
    let mut rig = build(model.clone(), store, Some(restored), 200);
    rig.handle
        .session_send("after restart", SendMode::Queue)
        .await;
    wait_closed(&mut rig.ui).await;
    let prompt = model.answer_prompt(0);
    stop(rig).await;
    let guidance = Playbook::builtin()
        .get("trace_code")
        .unwrap()
        .guidance
        .lines()
        .next()
        .unwrap()
        .to_string();
    assert!(
        prompt.contains(&guidance),
        "persisted override was not consumed after restart"
    );
}

#[tokio::test]
async fn checkpoint_never_leads_durable_history() {
    let sqlite = Arc::new(SqliteStore::memory().unwrap());
    let model = Arc::new(MockModel::new().judge_delay(Duration::from_secs(1)));
    let store: Arc<dyn Store> = sqlite.clone();
    let rig = build(model, store, None, 1);
    rig.handle.session_send("start", SendMode::Queue).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    let max = sqlite
        .load_chain(&sid())
        .unwrap()
        .last()
        .map(|e| e.seq)
        .unwrap_or(Seq::ZERO);
    let checkpoint = sqlite
        .latest_checkpoint(&sid())
        .unwrap()
        .map(|x| x.0)
        .unwrap_or(Seq::ZERO);
    stop(rig).await;
    assert!(
        checkpoint <= max,
        "checkpoint {checkpoint} contains state beyond durable event log {max}"
    );
}
