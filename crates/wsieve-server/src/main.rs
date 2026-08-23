//! bin 入口：配置 → AppState → axum::serve。TLS 在 Task 14。

use std::collections::HashSet;
use std::net::SocketAddr;

use anyhow::{bail, Context, Result};
use wsieve_proto::hello::MuxId;
use wsieve_server::{
    AppState, KeepaliveRange, ServerKeys, SEEN_CACHE_CAPACITY,
};

/// 最小配置（CLI 参数，Task 14 扩展为完整文件配置 + TLS）：
/// wsieve-server [--listen 0.0.0.0:443] [--key-file PATH] [--whitelist-file PATH]
struct Config {
    listen: SocketAddr,
    /// 服务端静态私钥文件（32 字节裸二进制）。缺省 → 报错（私钥必须显式提供）。
    key_file: String,
    /// 白名单文件：每行一个客户端静态公钥（64 位 hex 或 base64url）。
    whitelist_file: String,
}

fn parse_args() -> Result<Config> {
    let mut args = std::env::args().skip(1);
    let mut listen = None;
    let mut key_file = None;
    let mut whitelist_file = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--listen" => listen = Some(args.next().context("--listen 需要值")?),
            "--key-file" => key_file = Some(args.next().context("--key-file 需要值")?),
            "--whitelist-file" => {
                whitelist_file = Some(args.next().context("--whitelist-file 需要值")?)
            }
            other => bail!("未知参数: {other}"),
        }
    }
    Ok(Config {
        listen: listen
            .unwrap_or_else(|| "0.0.0.0:8080".into())
            .parse()
            .context("listen 地址解析失败")?,
        key_file: key_file.context("缺少 --key-file")?,
        whitelist_file: whitelist_file.context("缺少 --whitelist-file")?,
    })
}

/// 读取 32 字节裸私钥。
fn load_key(path: &str) -> Result<[u8; 32]> {
    let bytes = std::fs::read(path).with_context(|| format!("读取私钥 {path}"))?;
    bytes
        .try_into()
        .map_err(|v: Vec<u8>| anyhow::anyhow!("私钥长度 {} ≠ 32", v.len()))
}

/// 每行一个客户端静态公钥（hex 或 base64url），忽略空行与 # 注释。
fn load_whitelist(path: &str) -> Result<HashSet<[u8; 32]>> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let text = std::fs::read_to_string(path).with_context(|| format!("读取白名单 {path}"))?;
    let mut set = HashSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let key = if line.len() == 64 {
            let mut bytes = [0u8; 32];
            for i in 0..32 {
                bytes[i] =
                    u8::from_str_radix(&line[i * 2..i * 2 + 2], 16).context("hex 公钥解析失败")?;
            }
            bytes
        } else {
            URL_SAFE_NO_PAD
                .decode(line)
                .ok()
                .and_then(|v| v.try_into().ok())
                .context("公钥格式不支持（需 64 hex 或 base64url）")?
        };
        set.insert(key);
    }
    if set.is_empty() {
        bail!("白名单为空：无人能完成握手");
    }
    Ok(set)
}

fn enabled_mux() -> Vec<MuxId> {
    // 全家桶启用（picomux 实现存在但保守起见按 spec §7.3 主力集启用）。
    vec![MuxId::Yamux, MuxId::Smux, MuxId::Muxado, MuxId::H2mux]
}

fn main() -> Result<()> {
    let cfg = parse_args()?;
    let priv_key = load_key(&cfg.key_file)?;
    let whitelist = load_whitelist(&cfg.whitelist_file)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let state = AppState::new(
            ServerKeys { priv_key, whitelist },
            enabled_mux(),
            KeepaliveRange::default(),
            SEEN_CACHE_CAPACITY,
        );
        let listener = tokio::net::TcpListener::bind(cfg.listen).await?;
        eprintln!("wsieve-server listening on {}", cfg.listen);
        axum::serve(listener, state.router()).await?;
        anyhow::Ok(())
    })?;
    Ok(())
}
