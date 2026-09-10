//! 环回 TCP 转发（基准测量专用，不参与产品构建）。
//!
//! `fwd 127.0.0.1:8443 10.0.5.2:8443`
//!
//! **为什么需要它**：承载 WebView 里 Tauri 的 IPC 引导只在**安全上下文**下
//! 可用。`https://` 需要受信证书（内网自签装不进系统信任库，写信任设置要求
//! 交互授权），而裸 `http://<内网IP>` 不是安全上下文——页面能加载、脚本能注入，
//! 但 `invoke` 不可用，表现为「心跳停摆」且没有任何网络层错误，极难判读。
//!
//! `http://127.0.0.1:*` 落在 localhost 例外里，**是**安全上下文。于是把节点
//! 转发到环回即可，无需任何证书或系统权限。
//!
//! 环回带宽是数 GB/s 量级，对 1Gbps 的被测链路不构成瓶颈；但它确实多一跳，
//! 因此基准里**直连对照也走同一条转发**，两边同增同减。
//!
//! 只用 std：一连接两线程，各方向一个，`copy` 到 EOF。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

fn main() {
    let mut args = std::env::args().skip(1);
    let listen = args.next().expect("用法: fwd <监听addr> <目标addr>");
    let target = args.next().expect("用法: fwd <监听addr> <目标addr>");
    let listener = TcpListener::bind(&listen).expect("bind 失败");
    eprintln!("fwd {listen} -> {target}");
    for c in listener.incoming() {
        let Ok(inbound) = c else { continue };
        let target = target.clone();
        std::thread::spawn(move || {
            let Ok(outbound) = TcpStream::connect(&target) else {
                return;
            };
            let _ = inbound.set_nodelay(true);
            let _ = outbound.set_nodelay(true);
            let (Ok(i2), Ok(o2)) = (inbound.try_clone(), outbound.try_clone()) else {
                return;
            };
            let up = std::thread::spawn(move || pump(i2, o2));
            pump(outbound, inbound);
            let _ = up.join();
        });
    }
}

/// 单向搬运到 EOF，然后**半关对端写侧**——不这么做的话，一方读完了另一方
/// 还在傻等，连接要等到 TCP 超时才释放。
fn pump(mut from: TcpStream, mut to: TcpStream) {
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        match from.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if to.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
    let _ = to.shutdown(std::net::Shutdown::Write);
}
