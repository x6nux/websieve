//! bin 入口：配置 → AppState → 监听（TLS 部署模式见 tls.rs）。
//!
//! 环境变量/CLI：
//!   --listen 0.0.0.0:443            监听地址
//!   --key-file PATH                  服务端静态私钥（32B 裸二进制，必须）
//!   --whitelist-file PATH            客户端静态公钥白名单（必须）
//!   --deployment direct|cdn-flexible|cdn-full-self-signed   部署模式
//!   --cert-file/--key-pem-file       direct 模式的 PEM 证书/私钥
//!   --upstream URL                   伪装反代上游（可选）
//!   --alt-svc-port N                 广播 Alt-Svc h3（可选）

use std::collections::HashSet;
use std::net::SocketAddr;

use anyhow::{bail, Context, Result};
use wsieve_proto::hello::MuxId;
use wsieve_server::tls::Deployment;
use wsieve_server::{AppState, DisguiseCfg, KeepaliveRange, ServerKeys, SEEN_CACHE_CAPACITY};

struct Config {
    listen: SocketAddr,
    key_file: String,
    whitelist_file: String,
    deployment: Deployment,
    upstream: Option<String>,
    alt_svc_port: Option<u16>,
}

fn parse_args() -> Result<Config> {
    let mut args = std::env::args().skip(1);
    macro_rules! get {
        ($flag:expr) => {
            args.next().with_context(|| format!("{} 需要值", $flag))
        };
    }
    let mut listen = None;
    let mut key_file = None;
    let mut whitelist_file = None;
    let mut deployment = None;
    let mut cert_file = None;
    let mut key_pem_file = None;
    let mut upstream = None;
    let mut alt_svc_port = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--listen" => listen = Some(get!("--listen")?),
            "--key-file" => key_file = Some(get!("--key-file")?),
            "--whitelist-file" => {
                whitelist_file = Some(get!("--whitelist-file")?)
            }
            "--deployment" => deployment = Some(get!("--deployment")?),
            "--cert-file" => cert_file = Some(get!("--cert-file")?),
            "--key-pem-file" => {
                key_pem_file = Some(get!("--key-pem-file")?)
            }
            "--upstream" => upstream = Some(get!("--upstream")?),
            "--alt-svc-port" => {
                let v = get!("--alt-svc-port")?;
                alt_svc_port = Some(v.parse().context("--alt-svc-port 需数字")?);
            }
            other => bail!("未知参数: {other}"),
        }
    }
    let deployment = match deployment.as_deref() {
        None => Deployment::CdnFlexible,
        Some("cdn-flexible") => Deployment::CdnFlexible,
        Some("cdn-full-self-signed") => Deployment::CdnFullSelfSigned,
        Some("direct") => Deployment::Direct {
            cert_path: cert_file.context("direct 模式需要 --cert-file")?,
            key_path: key_pem_file.context("direct 模式需要 --key-pem-file")?,
        },
        Some(other) => bail!("未知部署模式: {other}"),
    };
    Ok(Config {
        listen: listen
            .unwrap_or_else(|| "0.0.0.0:8080".into())
            .parse()
            .context("listen 地址解析失败")?,
        key_file: key_file.context("缺少 --key-file")?,
        whitelist_file: whitelist_file.context("缺少 --whitelist-file")?,
        deployment,
        upstream,
        alt_svc_port,
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
    // 只剩一种 mux：wsmux。
    vec![
        MuxId::Wsmux,
        MuxId::Wsmux,
        MuxId::Wsmux,
        MuxId::Wsmux,
        MuxId::Wsmux,
    ]
}

fn main() -> Result<()> {
    let cfg = parse_args()?;
    let priv_key = load_key(&cfg.key_file)?;
    let whitelist = load_whitelist(&cfg.whitelist_file)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let state = AppState::with_disguise(
            ServerKeys { priv_key, whitelist },
            enabled_mux(),
            KeepaliveRange::default(),
            SEEN_CACHE_CAPACITY,
            DisguiseCfg {
                upstream: cfg.upstream,
                alt_svc_port: cfg.alt_svc_port,
            },
        );
        wsieve_server::tls::serve(&cfg.deployment, cfg.listen, state.router()).await?;
        eprintln!("wsieve-server listening on {} ({:?})", cfg.listen, cfg.deployment);
        tokio::signal::ctrl_c().await.ok();
        anyhow::Ok(())
    })?;
    Ok(())
}
