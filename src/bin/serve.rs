//! 起 UI。`cargo run --bin serve -- [目录] [端口] [--no-browser]`
//!
//! 起来之后默认用系统浏览器打开页面；`--no-browser` 或环境变量
//! `PREMORTEM_NO_BROWSER=1` 关掉（开发、测试脚本、没有桌面的机器）。
//!
//! 默认监听 127.0.0.1，**只绑本地回环**：这个进程手上有模型密钥、有对整个工作
//! 目录的读取权，绑到 0.0.0.0 等于把它开放给同网段的所有人。要远程访问就自己
//! 前面套一层带认证的反向代理，那是运维的事，不该由这里默认打开。

use premortem::config::real_env;
use premortem::server::{App, parse_serve_args, serve};

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let args = parse_serve_args(std::env::args().skip(1), &real_env);

    let app = match App::new(args.dir.into()).await {
        Ok(a) => a,
        Err(e) => {
            eprintln!("起不来：{e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = serve(app, args.port, args.open_browser).await {
        eprintln!("服务退出：{e}");
        std::process::exit(1);
    }
}
