//! 单一 writer task：所有磁盘 IO 的唯一出口。
//!
//! # 为什么必须是「单一」
//!
//! Core 不能 await 磁盘（会卡住打断响应），所以写盘必须异步出去。
//! 但一旦有两个写者，落盘顺序就不再确定，而这套设计的崩溃安全**完全依赖顺序**：
//!
//! ```text
//! turn 结束：① AppendHistory → ② 提交 pending → ③ WriteSnapshot
//! 用户编辑：立即 apply + 立即 WriteSnapshot，不等 turn
//! 恢复    ：读最新 snapshot + 重放 history 里 version 更大的记录
//! ```
//!
//! 先写日志再改状态，崩溃点的最坏情况是「历史里有但 state 没应用」，重放即可；
//! 反过来就会丢。单一 writer + 有序 channel 是这个顺序的唯一保证。
//!
//! # 快照是原子替换
//!
//! 写 `snapshot.json.tmp` 再 `rename`。直接覆写的话，崩在写一半就得到一个
//! 语法都不完整的 json，连「回到上一个版本」都做不到。

use crate::ids::{TurnId, Version};
use crate::model::Message;
use crate::msg::WriteJob;
use crate::state::{Op, Workspace};
use serde::{Deserialize, Serialize};
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

const SNAPSHOT: &str = "snapshot.json";
const SNAPSHOT_TMP: &str = "snapshot.json.tmp";
const HISTORY: &str = "history.jsonl";

/// history.jsonl 的一行。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryRecord {
    pub turn: TurnId,
    /// 写这条记录时 Core 的版本（**提交之前**的版本）。
    pub version: Version,
    pub msgs: Vec<Message>,
    /// 这一轮即将提交的状态改动。恢复时对 version 更大的记录重放它。
    #[serde(default)]
    pub commits: Vec<Op>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotFile {
    pub version: Version,
    pub ws: Workspace,
}

/// writer 的配置。`dir` 为 None 表示只记审计、不落盘（链路测试用）。
#[derive(Clone, Default)]
pub struct WriterCfg {
    pub dir: Option<PathBuf>,
    /// 按顺序记下每一次写操作的标签。链路测试靠它断言 ①②③ 的顺序。
    pub audit: Option<Arc<Mutex<Vec<String>>>>,
}

pub fn spawn_writer(cfg: WriterCfg) -> (mpsc::UnboundedSender<WriteJob>, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let join = tokio::spawn(run_writer(rx, cfg));
    (tx, join)
}

async fn run_writer(mut rx: mpsc::UnboundedReceiver<WriteJob>, cfg: WriterCfg) {
    if let Some(dir) = &cfg.dir {
        let _ = tokio::fs::create_dir_all(dir).await;
    }
    while let Some(job) = rx.recv().await {
        match job {
            WriteJob::AppendHistory { turn, version, msgs, commits } => {
                note(&cfg, format!("history(turn={turn},{}msgs,{}ops)", msgs.len(), commits.len()));
                if let Some(dir) = &cfg.dir {
                    let rec = HistoryRecord { turn, version, msgs, commits };
                    if let Err(e) = append_line(dir, &rec).await {
                        eprintln!("[writer] 写历史失败: {e}");
                    }
                }
            }
            WriteJob::WriteSnapshot { version, ws } => {
                note(&cfg, format!("snapshot({version})"));
                if let Some(dir) = &cfg.dir {
                    let snap = SnapshotFile { version, ws: *ws };
                    if let Err(e) = write_snapshot(dir, &snap).await {
                        eprintln!("[writer] 写快照失败: {e}");
                    }
                }
            }
            WriteJob::Shutdown => {
                note(&cfg, "shutdown".to_string());
                break;
            }
        }
    }
}

fn note(cfg: &WriterCfg, s: String) {
    if let Some(a) = &cfg.audit {
        if let Ok(mut v) = a.lock() {
            v.push(s);
        }
    }
}

async fn append_line(dir: &FsPath, rec: &HistoryRecord) -> std::io::Result<()> {
    let line = serde_json::to_string(rec).map_err(std::io::Error::other)?;
    let mut f = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(HISTORY))
        .await?;
    f.write_all(line.as_bytes()).await?;
    f.write_all(b"\n").await?;
    // 历史是重放的依据，必须真的落到盘上，不能只躺在页缓存里
    f.sync_data().await?;
    Ok(())
}

async fn write_snapshot(dir: &FsPath, snap: &SnapshotFile) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(snap).map_err(std::io::Error::other)?;
    let tmp = dir.join(SNAPSHOT_TMP);
    {
        let mut f = tokio::fs::File::create(&tmp).await?;
        f.write_all(&bytes).await?;
        f.sync_data().await?;
    }
    tokio::fs::rename(&tmp, dir.join(SNAPSHOT)).await
}

/// 恢复结果。
pub struct Restored {
    pub ws: Workspace,
    pub version: Version,
    /// snapshot 之后的历史记录（消息用于重建对话，commits 已经重放进 ws）。
    pub replayed: Vec<HistoryRecord>,
}

/// 读最新 snapshot + 重放 history 里 version 更大的记录。
///
/// 没有 snapshot 就返回 None，调用方开一个新会话。
pub async fn restore(dir: &FsPath) -> Option<Restored> {
    let raw = tokio::fs::read(dir.join(SNAPSHOT)).await.ok()?;
    let snap: SnapshotFile = serde_json::from_slice(&raw).ok()?;
    let mut ws = snap.ws;
    let mut version = snap.version;

    let mut replayed = Vec::new();
    if let Ok(text) = tokio::fs::read_to_string(dir.join(HISTORY)).await {
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let Ok(rec) = serde_json::from_str::<HistoryRecord>(line) else { continue };
            // 只重放快照之后的：崩溃点的最坏情况就是这几条「历史里有但 state 没应用」
            if rec.version < version {
                continue;
            }
            version.bump();
            for op in &rec.commits {
                match op {
                    Op::Set { path, value, source, confidence } => ws.write(
                        path.clone(),
                        value.clone(),
                        crate::state::Origin::Model,
                        source.clone(),
                        *confidence,
                        version,
                    ),
                    Op::Remove { path } => ws.erase(path),
                }
            }
            replayed.push(rec);
        }
    }
    Some(Restored { ws, version, replayed })
}
