//! TLS 部署模式（spec §8 部署表 + §6.8）：
//!
//! - `direct`：rustls 监听 + 真实证书文件，TLS 1.3 only，session ticket
//!   （rustls 默认启用），`max_early_data_size = 16384` 开启 0-RTT（§6.8 第 2 层；
//!   重放面由 msg1 防重放 + seq 去重兜底）；
//! - `cdn-flexible`：明文 HTTP 监听（CDN↔源站跑的仍是 Noise 密文）；
//! - `cdn-full-self-signed`：rcgen 启动时自签证书。
//!
//! Alt-Svc 头：`--alt-svc-port` 配置后广播 `h3:port`（§6.8 第 3 层“铺路”——
//! h3 监听本身不在本仓库范围，CDN 模式由 CDN 控制台负责）。

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use axum::Router;
use tokio::net::TcpListener;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::version::TLS13;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

/// 部署模式配置。
#[derive(Debug, Clone)]
pub enum Deployment {
    /// 直连：真实证书（cert PEM + key PEM）。
    Direct {
        cert_path: String,
        key_path: String,
    },
    /// CDN Flexible：明文 HTTP。
    CdnFlexible,
    /// CDN Full：自签证书（rcgen 启动时生成）。
    CdnFullSelfSigned,
}

/// Alt-Svc 广播端口（None = 不广播）。
#[derive(Debug, Clone, Copy, Default)]
pub struct AltSvc(pub Option<u16>);

impl AltSvc {
    pub fn header_value(&self) -> Option<String> {
        self.0.map(|p| format!("h3=\":{p}\"; ma=86400"))
    }
}

/// 服务监听器：封装 TLS 与明文两种 accept 形态。
pub enum ServeMode {
    Plain,
    Tls(TlsAcceptor),
}

/// 读取 PEM 证书 + 私钥 → rustls ServerConfig（TLS 1.3 only + early data）。
fn rustls_config(certs: Vec<CertificateDer<'static>>, key: PrivateKeyDer<'static>) -> Result<ServerConfig> {
    let mut cfg = ServerConfig::builder_with_protocol_versions(&[&TLS13])
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("证书/私钥加载失败")?;
    // §6.8 第 2 层：开 early data 让浏览器 0-RTT；重放面由
    // msg1 防重放（§6.3）+ seq 去重（§6.4）全额兜底。
    cfg.max_early_data_size = 16_384;
    Ok(cfg)
}

/// 按部署模式构建 ServeMode。
pub async fn build(deployment: &Deployment) -> Result<ServeMode> {
    match deployment {
        Deployment::CdnFlexible => Ok(ServeMode::Plain),
        Deployment::CdnFullSelfSigned => {
            let (cert, key) = self_signed()?;
            Ok(ServeMode::Tls(TlsAcceptor::from(Arc::new(
                rustls_config(vec![cert], key)?,
            ))))
        }
        Deployment::Direct {
            cert_path,
            key_path,
        } => {
            let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(
                &mut std::io::BufReader::new(std::fs::File::open(cert_path)?),
            )
            .collect::<std::io::Result<_>>()
            .with_context(|| format!("证书解析失败: {cert_path}"))?;
            if certs.is_empty() {
                bail!("证书文件不含 PEM 证书: {cert_path}");
            }
            let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(
                std::fs::File::open(key_path)?,
            ))
            .with_context(|| format!("私钥解析失败: {key_path}"))?
            .context("私钥文件不含 PEM 私钥")?;
            Ok(ServeMode::Tls(TlsAcceptor::from(Arc::new(
                rustls_config(certs, key)?,
            ))))
        }
    }
}

/// rcgen 自签证书（CN/NS 均为占位身份；CDN Full 模式下 CDN 不校验源站证书）。
fn self_signed() -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    let ka = rcgen::KeyPair::generate()?;
    let cert = rcgen::CertificateParams::new(vec!["localhost".to_string()])?
        .self_signed(&ka)?;
    let key = PrivateKeyDer::try_from(ka.serialize_der()).map_err(|_| anyhow::anyhow!("私钥序列化失败"))?;
    Ok((cert.der().clone(), key))
}

/// 在给定监听地址上跑 Router（按模式包 TLS）。
/// 返回实际绑定地址（127.0.0.1:0 测试用）。
pub async fn serve(
    deployment: &Deployment,
    listen: SocketAddr,
    router: Router,
) -> Result<SocketAddr> {
    let tcp = TcpListener::bind(listen).await.with_context(|| listen.to_string())?;
    let addr = tcp.local_addr()?;
    match build(deployment).await? {
        ServeMode::Plain => {
            tokio::spawn(async move {
                if let Err(e) = axum::serve(tcp, router).await {
                    eprintln!("serve error: {e}");
                }
            });
        }
        ServeMode::Tls(acceptor) => {
            tokio::spawn(async move {
                let mk_builder = || {
                    hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                };
                // TLS 监听：手动 accept → hyper-util auto conn（h1/h2 自适应）
                // 逐连接服务。axum::serve 只吃 TcpListener，TLS 流需走 hyper。
                loop {
                    let Ok((stream, _)) = tcp.accept().await else {
                        return;
                    };
                    let Ok(tls) = acceptor.accept(stream).await else {
                        continue;
                    };
                    let io = hyper_util::rt::TokioIo::new(tls);
                    let service = hyper_util::service::TowerToHyperService::new(router.clone());
                    let builder = mk_builder();
                    tokio::spawn(async move {
                        let conn = builder.serve_connection_with_upgrades(io, service);
                        if let Err(e) = conn.await {
                            eprintln!("tls conn error: {e}");
                        }
                    });
                }
            });
        }
    }
    Ok(addr)
}
