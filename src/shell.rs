//! 受控的外部命令执行。**模型碰不到它** —— 它是工具的实现手段，不是工具。
//!
//! # 只走 argv，永远不拼命令行
//!
//! 全项目没有一处 `sh -c` / `cmd /c`。命令是 `(二进制名, 参数数组)`，参数直接交给
//! `exec`，中间没有任何一层会去解释 `;`、`|`、`$(...)`、反引号。**注入在这里不是
//! 「过滤干净了」，是「根本没有可注入的语法」** —— 前者要不停打补丁，后者一劳永逸。
//!
//! 二进制只收裸名字（`rg`、`curl`），带路径分隔符的一律拒绝：允许 `./x` 就等于
//! 允许运行授权目录里任何一个可执行文件，而那些文件的内容可能是抓回来的。
//!
//! # 为什么用 `spawn_blocking` 而不是 `tokio::process`
//!
//! `tokio::process` 要给 tokio 开 `process` feature，在 unix 上会连带拉进
//! 信号处理那一串依赖。这个项目到目前为止一个网络/进程依赖都没有，为了跑几个
//! 只读命令引入它不划算。**代价是自己实现取消**：把一个 abort 标志传进阻塞任务，
//! 轮询 `try_wait` 的同时看它，置位就 kill 子进程。取消延迟是一个轮询周期。
//!
//! # 超时一定要 kill
//!
//! 只是不再等它，进程会留着继续跑、继续占资源、还可能继续往网络上发东西。
//! 一次抓取超时留一个 curl 在后台，跑一天下来就是几百个。

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
pub enum ShellError {
    #[error("命令 {0} 不在允许列表里")]
    NotAllowed(String),
    #[error("找不到命令 {0}")]
    NotFound(String),
    #[error("命令超时（{0:?}）已终止")]
    Timeout(Duration),
    #[error("已取消")]
    Cancelled,
    #[error("启动失败: {0}")]
    Io(String),
}

#[derive(Debug, Clone)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    /// 输出超过上限被砍掉过。
    pub truncated: bool,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.code == 0
    }

    /// 出错时给模型看的一句话。stderr 往往几百行，只取头两行。
    pub fn brief_error(&self) -> String {
        let head: Vec<&str> = self.stderr.lines().filter(|l| !l.trim().is_empty()).take(2).collect();
        if head.is_empty() {
            format!("退出码 {}", self.code)
        } else {
            format!("退出码 {}：{}", self.code, head.join(" / "))
        }
    }
}

/// 允许跑哪些命令、跑多久、最多收多少输出。
#[derive(Debug, Clone)]
pub struct Shell {
    allow: Vec<String>,
    pub timeout: Duration,
    pub max_output: usize,
}

impl Shell {
    pub fn new(allow: Vec<String>) -> Shell {
        Shell { allow, timeout: Duration::from_secs(60), max_output: 4 * 1024 * 1024 }
    }

    pub fn timeout(mut self, d: Duration) -> Shell {
        self.timeout = d;
        self
    }

    /// 这个命令允许跑吗（在白名单里、且装了）。
    pub fn have(&self, bin: &str) -> bool {
        self.allow.iter().any(|a| a == bin) && which(bin).is_some()
    }

    /// 跑一条命令。
    ///
    /// `args` 是**参数数组**，不是命令行字符串 —— 这个签名本身就是那条安全性质。
    pub async fn run(
        &self,
        bin: &str,
        args: &[String],
        token: &CancellationToken,
    ) -> Result<Output, ShellError> {
        if bin.contains('/') || bin.contains('\\') {
            return Err(ShellError::NotAllowed(bin.into()));
        }
        if !self.allow.iter().any(|a| a == bin) {
            return Err(ShellError::NotAllowed(bin.into()));
        }
        let path = which(bin).ok_or_else(|| ShellError::NotFound(bin.into()))?;

        let abort = Arc::new(AtomicBool::new(false));
        let deadline = self.timeout;
        let cap = self.max_output;
        let args: Vec<String> = args.to_vec();

        let flag = abort.clone();
        let mut job =
            tokio::task::spawn_blocking(move || run_blocking(path, args, deadline, cap, flag));

        tokio::select! {
            biased;
            _ = token.cancelled() => {
                abort.store(true, Ordering::SeqCst);
                // 等它自己收尾（kill 之后很快），拿不到就算了 —— 取消路径不阻塞调用方
                let _ = (&mut job).await;
                Err(ShellError::Cancelled)
            }
            r = &mut job => match r {
                Ok(v) => v,
                Err(e) => Err(ShellError::Io(format!("子任务失败: {e}"))),
            },
        }
    }
}

/// 阻塞线程里的那一段：起进程、轮询、到点或被叫停就 kill。
fn run_blocking(
    path: PathBuf,
    args: Vec<String>,
    deadline: Duration,
    cap: usize,
    abort: Arc<AtomicBool>,
) -> Result<Output, ShellError> {
    let mut child = Command::new(&path)
        .args(&args)
        // stdin 直接给 null：这些命令都不该等输入，给了 inherit 会在没有 tty 时挂住
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ShellError::Io(e.to_string()))?;

    let started = Instant::now();
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(e) => return Err(ShellError::Io(e.to_string())),
        }
        if abort.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ShellError::Cancelled);
        }
        if started.elapsed() >= deadline {
            // 只是不等它是不够的：进程会留着继续跑、继续发网络请求。
            let _ = child.kill();
            let _ = child.wait();
            timed_out = true;
            break;
        }
        std::thread::sleep(POLL);
    }

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = o.by_ref().take(cap as u64).read_to_end(&mut stdout);
    }
    if let Some(mut e) = child.stderr.take() {
        let _ = e.by_ref().take(64 * 1024).read_to_end(&mut stderr);
    }
    if timed_out {
        return Err(ShellError::Timeout(deadline));
    }
    let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
    let truncated = stdout.len() >= cap;
    Ok(Output {
        code,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        truncated,
    })
}

const POLL: Duration = Duration::from_millis(20);

/// PATH 里找一个裸命令名。Windows 上补 `PATHEXT` 里的后缀。
pub fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
            .split(';')
            .map(|s| s.to_ascii_lowercase())
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path) {
        for ext in &exts {
            let cand = dir.join(format!("{bin}{ext}"));
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}
