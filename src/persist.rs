//! 单一 writer task 与启动恢复。
//!
//! # writer 跑在阻塞线程上
//!
//! [`crate::store::Store`] 是同步 trait（rusqlite 就是同步的），所以 writer 用
//! `spawn_blocking` 起一条专用线程，靠 `blocking_recv` 从 channel 取活。
//! Core 依然一次磁盘都不碰。
//!
//! # 两级落盘
//!
//! - **must-flush**：用户输入、用户 apply、turn 收尾、shutdown。这些是**用户付出过动作**
//!   的数据，写入事务提交后才回执，UI 在此之前把气泡显示成「发送中」。
//! - **best-effort**：正文、工具事件、成本、场景判定。攒到 [`BATCH`] 条或
//!   [`LINGER`] 之后一并写。掉电最多丢最后这一小段模型产物，重生成即可。
//!
//! 全部 must-flush 会让每个 delta 都开一次事务；全部 best-effort 就回到了
//! 「用户输入还躺在内存里」的老问题。所以是两级。
//!
//! # 写失败不回滚内存
//!
//! Core 已经把事件当成生效的了（分配了 seq、更新了视图、推了 UI）。落盘失败时
//! writer 无限重试，并在连续失败后发 [`UiEvent::PersistDegraded`]。
//! **不回滚** —— 让用户刚打的字从屏幕上消失，比「暂时还没落盘」糟得多。

use crate::event::{Body, Draft, Event, crashed_turns, now_ms, open_questions, unclosed_calls};
use crate::ids::{Seq, SessionId};
use crate::msg::{OpenQuestion, UiEvent, WriteJob};
use crate::state::Workspace;
use crate::store::{Store, StoreError};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};

/// 攒批上限：条数。
const BATCH: usize = 64;
/// 攒批上限：时间。
const LINGER: Duration = Duration::from_millis(200);
/// 连续失败几次之后告诉用户。前几次多半是瞬时的（杀毒软件扫文件、盘忙）。
const DEGRADE_AFTER: u32 = 3;

pub fn spawn_writer(
    store: Arc<dyn Store>,
    ui: broadcast::Sender<UiEvent>,
) -> (mpsc::UnboundedSender<WriteJob>, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let join = tokio::spawn(run_writer(rx, store, ui));
    (tx, join)
}

/// # 为什么是 async 循环 + 每批一次 `spawn_blocking`
///
/// 攒批必须有个**兜底的时限**：只在「下一次有活来」时才检查攒够没有的话，
/// 一批 best-effort 事件之后没有后续活动，它就永远躺在内存里 ——
/// 而那正是「用户看着正文吐完、关掉应用、回来一片空白」的成因。
/// async 循环拿得到 `sleep`，阻塞线程拿不到。
///
/// 每次真正写库时才 `spawn_blocking` 一下（≤5 次/秒），
/// rusqlite 的同步调用不会卡住 runtime。
async fn run_writer(
    mut rx: mpsc::UnboundedReceiver<WriteJob>,
    store: Arc<dyn Store>,
    ui: broadcast::Sender<UiEvent>,
) {
    // 还没成功写进去的。**永远不丢**，只会越攒越多然后一起重试。
    let mut pending: Vec<Event> = Vec::new();
    let mut fails: u32 = 0;
    let mut degraded = false;

    loop {
        let job = if pending.is_empty() {
            // 没东西攒着就一直等，不用空转。
            match rx.recv().await {
                Some(j) => j,
                None => break,
            }
        } else {
            // 攒着东西 ⇒ 最多再等 LINGER 就落盘，不管有没有新活来。
            match tokio::time::timeout(LINGER, rx.recv()).await {
                Ok(Some(j)) => j,
                Ok(None) => {
                    flush(&store, &mut pending, &mut fails, &mut degraded, &ui).await;
                    break;
                }
                Err(_) => {
                    flush(&store, &mut pending, &mut fails, &mut degraded, &ui).await;
                    continue;
                }
            }
        };

        match job {
            WriteJob::Append { evs, ack } => {
                pending.extend(evs);
                // must-flush：用户付出过动作的数据，落盘确认之后才 ack。
                let must = ack.is_some();
                let ok = if must || pending.len() >= BATCH {
                    flush(&store, &mut pending, &mut fails, &mut degraded, &ui).await
                } else {
                    true
                };
                if let Some(a) = ack {
                    let _ = a.send(ok && pending.is_empty());
                }
            }
            WriteJob::Checkpoint { session, seq, ws } => {
                // ★ 先把攒着的事件写掉再打 checkpoint。
                //
                // Append 是攒批的、Checkpoint 是立即写的，不先冲一遍的话
                // checkpoint 会记到一个盘上还不存在的 seq。重启时 restore 从那个
                // checkpoint 起跳，却找不到它之后（其实是之前）的事件，
                // 于是 Core 会从一个比 checkpoint 还小的位置继续分配 seq —— 直接撞号。
                flush(&store, &mut pending, &mut fails, &mut degraded, &ui).await;
                // 纯功能性：失败只记一行，不影响正确性，也不触发降级提示。
                let s = store.clone();
                let r =
                    tokio::task::spawn_blocking(move || s.put_checkpoint(&session, seq, &ws)).await;
                if let Ok(Err(e)) = r {
                    eprintln!("[writer] checkpoint({seq}) 失败，忽略: {e}");
                }
            }
            WriteJob::NewSession { session } => {
                let s = store.clone();
                let r = tokio::task::spawn_blocking(move || s.create_session(&session)).await;
                if let Ok(Err(e)) = r {
                    eprintln!("[writer] 建会话失败: {e}");
                }
            }
            WriteJob::Shutdown { ack } => {
                // 退出前把攒着的全写掉。**这一步是「优雅退出」与「被 kill」的全部差别。**
                let mut tries = 0;
                while !pending.is_empty() && tries < 10 {
                    flush(&store, &mut pending, &mut fails, &mut degraded, &ui).await;
                    tries += 1;
                    if !pending.is_empty() {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
                if !pending.is_empty() {
                    eprintln!("[writer] 退出时仍有 {} 条未落盘", pending.len());
                }
                let _ = ack.send(());
                break;
            }
        }
    }
}

/// 返回 true 表示这一批真的进库了。
async fn flush(
    store: &Arc<dyn Store>,
    pending: &mut Vec<Event>,
    fails: &mut u32,
    degraded: &mut bool,
    ui: &broadcast::Sender<UiEvent>,
) -> bool {
    if pending.is_empty() {
        return true;
    }
    let s = store.clone();
    let batch = pending.clone();
    let r = tokio::task::spawn_blocking(move || s.append(&batch)).await;
    match r {
        Ok(Ok(())) => {
            pending.clear();
            *fails = 0;
            if *degraded {
                *degraded = false;
                let _ = ui.send(UiEvent::PersistOk);
            }
            true
        }
        Ok(Err(e)) => {
            *fails += 1;
            // 前几次多半是瞬时的（杀毒软件扫文件、盘忙），不值得惊动用户。
            if *fails >= DEGRADE_AFTER && !*degraded {
                *degraded = true;
                let _ = ui.send(UiEvent::PersistDegraded {
                    why: e.to_string(),
                    pending: pending.len(),
                });
            }
            eprintln!("[writer] 落盘失败第 {} 次（{} 条待写）: {e}", *fails, pending.len());
            false
        }
        Err(e) => {
            eprintln!("[writer] 写盘任务 panic: {e}");
            false
        }
    }
}

// ───────────────────────────── 恢复 ─────────────────────────────

/// 启动恢复的结果。
pub struct Restored {
    pub session: SessionId,
    /// 物化好的推断图。
    pub ws: Workspace,
    /// 时间线上最后一条的位置。Core 接着往下分配。
    pub seq: Seq,
    /// 完整时间线。
    pub events: Vec<Event>,
    /// 需要补进时间线的修补事件：未闭合的工具调用、开了没关的轮次。
    ///
    /// **它们是真正的事件，要写进去**，不是内存里的一个标记 —— 否则下次启动
    /// 还要再判一次「上次是不是崩了」，而且中间任何一次组装 prompt 都会缺 tool 消息。
    pub repairs: Vec<Draft>,
    pub open_questions: Vec<OpenQuestion>,
    /// 本链上出现过的**全部** client_id。会话里的用户输入条数天然有界，
    /// 全量预热才能挡住「重发一条很旧的消息」。
    pub client_ids: Vec<String>,
    pub crashed: usize,
}

/// 读 checkpoint + 重放其后的事件，并算出要补的修补事件。
///
/// # 为什么物化走的是 `Workspace::apply`
///
/// 上一版恢复时另写了一段重放逻辑，里面把所有 op 一律记成 `Origin::Model` ——
/// 于是重启后用户填的字段全变成模型推断，`[用户设定]` 标记消失。
/// 现在恢复与运行时调的是同一个函数，不存在「两条路径实现不一致」的可能。
pub fn restore(store: &dyn Store, session: &SessionId) -> Result<Restored, StoreError> {
    // 读整条会话链：分叉出来的会话要把父会话分叉点之前的历史一起带上，
    // 否则新分支一开口就失忆。
    let events = store.load_chain(session)?;
    let forked_at = store.get_session(session)?.map(|s| s.forked_at).unwrap_or(Seq::ZERO);
    let seq = events.last().map(|e| e.seq).unwrap_or(forked_at);

    // checkpoint 只是加速：从它开始物化，而不是从 0 开始。丢了也只是慢一点。
    let (mut ws, from) = match store.latest_checkpoint(session)? {
        Some((cs, w)) => (w, cs),
        None => (Workspace::new(), Seq::ZERO),
    };
    for e in events.iter().filter(|e| e.seq > from) {
        ws.apply(e);
    }

    // 未闭合的工具调用 ⇒ 补 Aborted。与打断收尾 `close_open_calls` 是同一件事，
    // 只是时机不同：进程还活着时由它补，进程没了就由这里补。
    let mut repairs = Vec::new();
    for (called_seq, call_id, name) in unclosed_calls(&events) {
        // 只补本会话自己留下的烂摊子：父会话分叉点之前的未闭合调用是它自己的事，
        // 分支不该去改写它的历史（也确实改不动 —— 那些事件属于另一条 session）。
        if called_seq <= forked_at {
            continue;
        }
        let turn = events.iter().find(|e| e.seq == called_seq).and_then(|e| e.turn);
        repairs.push(Draft::reply_to(
            turn,
            called_seq,
            Body::Aborted {
                call_id,
                why: format!("[interrupted] {name} 在返回前进程结束"),
            },
        ));
    }
    // 开了没关的轮次 ⇒ 补 TurnClosed，标成异常。
    let crashed: Vec<_> = crashed_turns(&events)
        .into_iter()
        .filter(|t| {
            events
                .iter()
                .any(|e| e.turn == Some(*t) && e.seq > forked_at)
        })
        .collect();
    for t in &crashed {
        repairs.push(Draft::new(
            Some(*t),
            Body::TurnClosed { aborted: true, stats: "{\"crashed\":true}".into() },
        ));
    }

    let open_questions = open_questions(&events)
        .into_iter()
        .map(|(id, question, options)| OpenQuestion { id, question, options })
        .collect();

    let client_ids = events
        .iter()
        .filter_map(|e| match &e.body {
            Body::Said { client_id, .. } => Some(client_id.clone()),
            _ => None,
        })
        .collect();

    Ok(Restored {
        session: session.clone(),
        ws,
        seq,
        events,
        repairs,
        open_questions,
        client_ids,
        crashed: crashed.len(),
    })
}

/// 把 draft 补成完整事件（恢复路径专用；正常路径由 Core 的 `commit` 做）。
pub fn seal(session: &SessionId, drafts: Vec<Draft>, next: &mut Seq) -> Vec<Event> {
    drafts
        .into_iter()
        .map(|d| Event {
            session: session.clone(),
            seq: next.bump(),
            turn: d.turn,
            at_ms: now_ms(),
            corr: d.corr,
            body: d.body,
        })
        .collect()
}
