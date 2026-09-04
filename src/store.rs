//! 主时间线的存储。**三张表：会话、事件、checkpoint。**
//!
//! # 为什么没有派生表
//!
//! 会话规模是「一个人聊几十上百轮」，全表扫是微秒级；而每多一张派生表就多一处
//! 「写事件时忘了同步」的机会。**由时间线组装的难度不大，那就每次组装。**
//! 表上只有一个从 [`Body::tag`] 抄出来的 `kind` 列，纯粹当索引键用，不是第二份真相。
//!
//! # 会话是一等公民
//!
//! 回滚**不是截断**，是从某一轮分叉出一个新会话。原会话一条事件都不动 ——
//! 那条路走过就走过了，它是「试过没成立的方法」的原始记录。
//!
//! 读一个会话 = 沿 `parent` 链往上走，每段取 `seq <= 该段的 forked_at`，
//! 从根往叶拼（[`Store::load_chain`]）。每段的 seq 区间首尾相接，拼出来天然按 seq 升序。
//!
//! 所以事件主键是 `(session, seq)`：两个从同一点分叉出去的会话，各自的下一条
//! 都是 `forked_at + 1`，它们是同一段历史的两种续法，本来就不该有先后。
//!
//! # 同步 trait，由 writer 在阻塞线程上跑
//!
//! rusqlite 是同步 API，而写连接本来就独占给 writer。把它包成 async trait
//! 只会为一次本地磁盘写引入一层 boxing 和一个 runtime 往返。
//!
//! # checkpoint 不参与决定 history
//!
//! 纯粹是启动加速：`最近 checkpoint + 重放其后的事件`。删光 checkpoint 表，
//! 程序照常工作，只是启动慢一点（S22 验证）。

use crate::event::{Body, Event};
use crate::ids::{Seq, Session, SessionId};
use crate::state::Workspace;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path as FsPath;
use std::sync::{Arc, Mutex};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("存储错误: {0}")]
    Db(String),
    #[error("编码错误: {0}")]
    Codec(String),
    #[error("没有这个会话: {0}")]
    NoSession(String),
    #[error("注入的故障: {0}")]
    Injected(String),
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Db(e.to_string())
    }
}
impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        StoreError::Codec(e.to_string())
    }
}

/// 存储接口。SQLite 是一个实现，[`MemStore`] 是另一个，[`FaultStore`] 是注入层。
pub trait Store: Send + Sync {
    /// 原子追加一批事件。**要么全进要么全不进** —— 一次 turn 收尾会同时写正文、
    /// 状态改动和 TurnClosed，中间断开会留下「开了没关」的轮次。
    fn append(&self, evs: &[Event]) -> Result<(), StoreError>;

    fn create_session(&self, s: &Session) -> Result<(), StoreError>;
    fn get_session(&self, id: &SessionId) -> Result<Option<Session>, StoreError>;
    fn list_sessions(&self) -> Result<Vec<Session>, StoreError>;

    /// 读一条会话链的全部事件，按 seq 升序。
    fn load_chain(&self, id: &SessionId) -> Result<Vec<Event>, StoreError>;

    fn latest_checkpoint(&self, id: &SessionId) -> Result<Option<(Seq, Workspace)>, StoreError>;
    fn put_checkpoint(&self, id: &SessionId, seq: Seq, ws: &Workspace) -> Result<(), StoreError>;
}

/// 沿 parent 链求出 `(会话, 该段的 seq 上界)`，从根到叶。
fn chain_of(store: &dyn Store, id: &SessionId) -> Result<Vec<(SessionId, Seq)>, StoreError> {
    let mut links: Vec<(SessionId, Seq)> = Vec::new();
    let mut cur = Some(id.clone());
    // 叶子自己没有上界：它一直写到最后
    let mut bound = Seq(u64::MAX);
    while let Some(s) = cur {
        let row = store.get_session(&s)?;
        links.push((s.clone(), bound));
        match row {
            Some(r) => {
                bound = r.forked_at;
                cur = r.parent;
            }
            // 会话行不存在（老库、或者根会话还没写元信息）⇒ 到此为止
            None => cur = None,
        }
        if links.len() > 64 {
            return Err(StoreError::Db("会话链过深，疑似成环".into()));
        }
    }
    links.reverse();
    Ok(links)
}

// ───────────────────────────── SQLite ─────────────────────────────

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS session (
  id         TEXT PRIMARY KEY,
  parent     TEXT,
  forked_at  INTEGER NOT NULL DEFAULT 0,
  title      TEXT NOT NULL DEFAULT '',
  created_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS history (
  session   TEXT    NOT NULL,
  seq       INTEGER NOT NULL,
  turn      INTEGER,
  at_ms     INTEGER NOT NULL,
  corr      INTEGER,
  kind      TEXT    NOT NULL,
  client_id TEXT,
  body      TEXT    NOT NULL,
  PRIMARY KEY(session, seq)
);
CREATE UNIQUE INDEX IF NOT EXISTS history_client ON history(session, client_id) WHERE client_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS history_kind ON history(session, kind, seq);
CREATE INDEX IF NOT EXISTS history_corr ON history(session, corr) WHERE corr IS NOT NULL;
CREATE INDEX IF NOT EXISTS history_turn ON history(session, turn, seq);
CREATE TABLE IF NOT EXISTS checkpoint (
  session TEXT    NOT NULL,
  seq     INTEGER NOT NULL,
  ws      TEXT    NOT NULL,
  at_ms   INTEGER NOT NULL,
  PRIMARY KEY(session, seq)
);
";

pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    pub fn open(path: &FsPath) -> Result<SqliteStore, StoreError> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        Self::init(Connection::open(path)?)
    }

    /// 内存库。链路测试用它跑真实 SQL，不碰磁盘。
    pub fn memory() -> Result<SqliteStore, StoreError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<SqliteStore, StoreError> {
        // WAL：UI 的只读查询和 writer 的追加互不阻塞，这是引入 SQLite 最直接的收益。
        // NORMAL：WAL 下仍然崩溃安全，掉电最多丢最后一个未 checkpoint 的事务。
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(SqliteStore { conn: Mutex::new(conn) })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, StoreError> {
        self.conn.lock().map_err(|_| StoreError::Db("连接锁中毒".into()))
    }

    fn segment(&self, sid: &SessionId, upto: Seq) -> Result<Vec<Event>, StoreError> {
        let c = self.lock()?;
        let mut st = c.prepare_cached(
            "SELECT seq,turn,at_ms,corr,body FROM history \
             WHERE session = ?1 AND seq <= ?2 ORDER BY seq",
        )?;
        // SQLite 的整数是有符号的：`u64::MAX as i64` 会变成 -1，
        // `seq <= -1` 一行都匹配不到 —— 叶子段的「没有上界」必须夹到 i64::MAX。
        let bound = upto.0.min(i64::MAX as u64) as i64;
        let rows = st.query_map(params![sid.0, bound], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (s, t, a, co, b) = r?;
            out.push(Event {
                session: sid.clone(),
                seq: Seq(s as u64),
                turn: t.map(|x| crate::ids::TurnId(x as u64)),
                at_ms: a as u64,
                corr: co.map(|x| Seq(x as u64)),
                body: serde_json::from_str(&b)?,
            });
        }
        Ok(out)
    }
}

impl Store for SqliteStore {
    fn append(&self, evs: &[Event]) -> Result<(), StoreError> {
        if evs.is_empty() {
            return Ok(());
        }
        let mut c = self.lock()?;
        let tx = c.transaction()?;
        {
            let mut st = tx.prepare_cached(
                "INSERT INTO history(session,seq,turn,at_ms,corr,kind,client_id,body) \
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            )?;
            for e in evs {
                let client = match &e.body {
                    Body::Said { client_id, .. } => Some(client_id.clone()),
                    _ => None,
                };
                st.execute(params![
                    e.session.0,
                    e.seq.0 as i64,
                    e.turn.map(|t| t.0 as i64),
                    e.at_ms as i64,
                    e.corr.map(|s| s.0 as i64),
                    e.body.tag(),
                    client,
                    serde_json::to_string(&e.body)?,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn create_session(&self, s: &Session) -> Result<(), StoreError> {
        let c = self.lock()?;
        c.execute(
            "INSERT OR REPLACE INTO session(id,parent,forked_at,title,created_ms) \
             VALUES(?1,?2,?3,?4,?5)",
            params![
                s.id.0,
                s.parent.as_ref().map(|p| p.0.clone()),
                s.forked_at.0 as i64,
                s.title,
                s.created_ms as i64
            ],
        )?;
        Ok(())
    }

    fn get_session(&self, id: &SessionId) -> Result<Option<Session>, StoreError> {
        let c = self.lock()?;
        let row = c
            .query_row(
                "SELECT id,parent,forked_at,title,created_ms FROM session WHERE id = ?1",
                [id.0.clone()],
                |r| {
                    Ok(Session {
                        id: SessionId(r.get::<_, String>(0)?),
                        parent: r.get::<_, Option<String>>(1)?.map(SessionId),
                        forked_at: Seq(r.get::<_, i64>(2)? as u64),
                        title: r.get::<_, String>(3)?,
                        created_ms: r.get::<_, i64>(4)? as u64,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    fn list_sessions(&self) -> Result<Vec<Session>, StoreError> {
        let c = self.lock()?;
        let mut st = c.prepare(
            "SELECT id,parent,forked_at,title,created_ms FROM session ORDER BY created_ms",
        )?;
        let rows = st.query_map([], |r| {
            Ok(Session {
                id: SessionId(r.get::<_, String>(0)?),
                parent: r.get::<_, Option<String>>(1)?.map(SessionId),
                forked_at: Seq(r.get::<_, i64>(2)? as u64),
                title: r.get::<_, String>(3)?,
                created_ms: r.get::<_, i64>(4)? as u64,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    fn load_chain(&self, id: &SessionId) -> Result<Vec<Event>, StoreError> {
        let links = chain_of(self, id)?;
        let mut out = Vec::new();
        for (sid, bound) in links {
            out.extend(self.segment(&sid, bound)?);
        }
        Ok(out)
    }

    fn latest_checkpoint(&self, id: &SessionId) -> Result<Option<(Seq, Workspace)>, StoreError> {
        let c = self.lock()?;
        let row: Option<(i64, String)> = c
            .query_row(
                "SELECT seq,ws FROM checkpoint WHERE session = ?1 ORDER BY seq DESC LIMIT 1",
                [id.0.clone()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match row {
            None => Ok(None),
            Some((s, ws)) => Ok(Some((Seq(s as u64), serde_json::from_str(&ws)?))),
        }
    }

    fn put_checkpoint(&self, id: &SessionId, seq: Seq, ws: &Workspace) -> Result<(), StoreError> {
        let c = self.lock()?;
        c.execute(
            "INSERT OR REPLACE INTO checkpoint(session,seq,ws,at_ms) VALUES(?1,?2,?3,?4)",
            params![
                id.0,
                seq.0 as i64,
                serde_json::to_string(ws)?,
                crate::event::now_ms() as i64
            ],
        )?;
        // 每条会话只留最近 10 份。留多份是为了分叉时能就近起跳，不必从 0 重放。
        c.execute(
            "DELETE FROM checkpoint WHERE session = ?1 AND seq NOT IN \
             (SELECT seq FROM checkpoint WHERE session = ?1 ORDER BY seq DESC LIMIT 10)",
            [id.0.clone()],
        )?;
        Ok(())
    }
}

// ───────────────────────────── 内存实现 ─────────────────────────────

/// 不落盘的实现。用于不变量对拍：同一串事件喂给两个实现，物化结果必须逐字段相等。
#[derive(Default)]
pub struct MemStore {
    evs: Mutex<Vec<Event>>,
    sessions: Mutex<Vec<Session>>,
    ck: Mutex<Vec<(SessionId, Seq, Workspace)>>,
}

impl MemStore {
    pub fn new() -> MemStore {
        MemStore::default()
    }
}

impl Store for MemStore {
    fn append(&self, evs: &[Event]) -> Result<(), StoreError> {
        self.evs.lock().unwrap().extend_from_slice(evs);
        Ok(())
    }
    fn create_session(&self, s: &Session) -> Result<(), StoreError> {
        let mut g = self.sessions.lock().unwrap();
        g.retain(|x| x.id != s.id);
        g.push(s.clone());
        Ok(())
    }
    fn get_session(&self, id: &SessionId) -> Result<Option<Session>, StoreError> {
        Ok(self.sessions.lock().unwrap().iter().find(|s| &s.id == id).cloned())
    }
    fn list_sessions(&self) -> Result<Vec<Session>, StoreError> {
        Ok(self.sessions.lock().unwrap().clone())
    }
    fn load_chain(&self, id: &SessionId) -> Result<Vec<Event>, StoreError> {
        let links = chain_of(self, id)?;
        let g = self.evs.lock().unwrap();
        let mut out = Vec::new();
        for (sid, bound) in links {
            let mut seg: Vec<Event> =
                g.iter().filter(|e| e.session == sid && e.seq <= bound).cloned().collect();
            seg.sort_by_key(|e| e.seq);
            out.extend(seg);
        }
        Ok(out)
    }
    fn latest_checkpoint(&self, id: &SessionId) -> Result<Option<(Seq, Workspace)>, StoreError> {
        Ok(self
            .ck
            .lock()
            .unwrap()
            .iter()
            .filter(|(s, _, _)| s == id)
            .max_by_key(|(_, q, _)| *q)
            .map(|(_, q, w)| (*q, w.clone())))
    }
    fn put_checkpoint(&self, id: &SessionId, seq: Seq, ws: &Workspace) -> Result<(), StoreError> {
        self.ck.lock().unwrap().push((id.clone(), seq, ws.clone()));
        Ok(())
    }
}

// ───────────────────────────── 故障注入 ─────────────────────────────

/// 故障注入层。包住任意 [`Store`]，按计数在第 N 次写入时失败。
///
/// 存在的理由：正路测试测不出「落盘挂了会怎样」。有了它才能断言
/// 「盘写不进去，用户仍然能继续对话，且状态栏亮起降级提示」。
pub struct FaultStore {
    inner: Arc<dyn Store>,
    /// 第几次 `append` 开始失败（1 = 第一次就失败）。0 = 不注入。
    fail_from: Mutex<u32>,
    /// 还要失败几次。用完之后恢复正常，用来验证「失败后重试成功」。
    fail_times: Mutex<u32>,
    calls: Mutex<u32>,
}

impl FaultStore {
    pub fn new(inner: Arc<dyn Store>, fail_from: u32, fail_times: u32) -> FaultStore {
        FaultStore {
            inner,
            fail_from: Mutex::new(fail_from),
            fail_times: Mutex::new(fail_times),
            calls: Mutex::new(0),
        }
    }
    fn should_fail(&self) -> bool {
        let mut n = self.calls.lock().unwrap();
        *n += 1;
        let from = *self.fail_from.lock().unwrap();
        if from == 0 || *n < from {
            return false;
        }
        let mut left = self.fail_times.lock().unwrap();
        if *left == 0 {
            return false;
        }
        *left -= 1;
        true
    }
}

impl Store for FaultStore {
    fn append(&self, evs: &[Event]) -> Result<(), StoreError> {
        if self.should_fail() {
            return Err(StoreError::Injected("append".into()));
        }
        self.inner.append(evs)
    }
    fn create_session(&self, s: &Session) -> Result<(), StoreError> {
        self.inner.create_session(s)
    }
    fn get_session(&self, id: &SessionId) -> Result<Option<Session>, StoreError> {
        self.inner.get_session(id)
    }
    fn list_sessions(&self) -> Result<Vec<Session>, StoreError> {
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
