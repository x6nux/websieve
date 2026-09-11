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
        // 证书材料只读一次，TCP 与 UDP 两面共用。各读各的会在证书轮换的那
        // 一瞬间读到两个版本，两个面于是拿着不同证书对外服务——不报错，只在
        // 客户端零星出现校验失败。
        let material = wsieve_server::tls::tls_material(&cfg.deployment)?;

        // UDP 面**先于 AppState** 绑定：Alt-Svc 要宣告的端口取决于它成不成功，
        // 而 AppState 构造时就需要那个端口（端口 → AppState → router → 服务
        // 是一条链）。失败只警告不中止——h3 是叠加能力，TCP 面必须照常服务，
        // 与 shard_setup 里「条带禁用则退回单会话」同源的纪律。
        let h3_endpoint = match &material {
            Some((certs, key)) => {
                match wsieve_server::http3::bind_endpoint(
                    certs.clone(),
                    key.clone_key(),
                    cfg.listen,
                ) {
                    Ok(ep) => Some(ep),
                    Err(e) => {
                        eprintln!("HTTP/3 未启用（{e:#}）——继续以 h1/h2 提供服务");
                        None
                    }
                }
            }
            // 明文部署（cdn-flexible）没有证书，QUIC 无从谈起
            None => None,
        };
        let h3_port = h3_endpoint
            .as_ref()
            .and_then(|ep| ep.local_addr().ok())
            .map(|a| a.port());

        let state = AppState::with_disguise(
            ServerKeys { priv_key, whitelist },
            enabled_mux(),
            KeepaliveRange::default(),
            SEEN_CACHE_CAPACITY,
            DisguiseCfg {
                upstream: cfg.upstream,
                // 只在 h3 真的起来了才宣告。宣告一个连不上的 QUIC 端点，会让
                // 客户端此后每次连接都先试 QUIC 超时再回落，比不宣告更糟。
                // 命令行的 --alt-svc-port 保留为手工覆盖（CDN 模式下 h3 由
                // CDN 提供，本进程并不监听 UDP）。
                alt_svc_port: h3_port.or(cfg.alt_svc_port),
            },
        );

        if let Some(ep) = h3_endpoint {
            wsieve_server::http3::serve(ep, state.clone().router());
            eprintln!("HTTP/3 就绪: udp://{}", cfg.listen);
        }

        let mode = wsieve_server::tls::mode_from_material(material)?;
        wsieve_server::tls::serve_mode(mode, cfg.listen, state.router()).await?;
        eprintln!("wsieve-server listening on {} ({:?})", cfg.listen, cfg.deployment);
        tokio::signal::ctrl_c().await.ok();
        anyhow::Ok(())
    })?;
    Ok(())
}
