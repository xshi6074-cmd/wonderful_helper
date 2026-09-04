//! 起 UI。`cargo run --bin serve -- [目录] [端口]`
//!
//! 默认监听 127.0.0.1，**只绑本地回环**：这个进程手上有模型密钥、有对整个工作
//! 目录的读取权，绑到 0.0.0.0 等于把它开放给同网段的所有人。要远程访问就自己
//! 前面套一层带认证的反向代理，那是运维的事，不该由这里默认打开。

use premortem::server::{App, serve};

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().unwrap_or_else(|| ".".into());
    let port: u16 = args.next().and_then(|p| p.parse().ok()).unwrap_or(7878);

    let app = match App::new(dir.into()).await {
        Ok(a) => a,
        Err(e) => {
            eprintln!("起不来：{e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = serve(app, port).await {
        eprintln!("服务退出：{e}");
        std::process::exit(1);
    }
}
