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

/// 选定进程级 crypto provider。
///
/// rustls 0.23 在编译进多个 provider 时拒绝自动选择，构建 ServerConfig 时
/// 直接 panic。本 workspace 显式启用了 ring，但 tokio-rustls 会顺带带进
/// aws-lc-rs，于是两个都在 ⇒ 必须显式装一个。少了这一步，`direct` 与
/// `cdn-full-self-signed` 两种部署一启动就崩，而 CI/E2E 全用 `cdn-flexible`
/// （明文）跑，永远碰不到这条路径。
///
/// 重复调用返回 Err（已装过），忽略即可——多个测试并发进来是正常的。
fn ensure_crypto_provider() {
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
}

/// TCP 监听面的 ALPN：h2 优先，客户端不支持时回落 http/1.1。
///
/// 顺序即服务端偏好。`hyper_util` 的 `auto::Builder`（见 `serve`）本就能
/// h1/h2 自适应，所以宣告 h2 不需要任何额外的协议实现——此前缺的只是这
/// 一句宣告本身。
pub(crate) const ALPN_TCP: [&[u8]; 2] = [b"h2", b"http/1.1"];

/// QUIC 监听面的 ALPN：只有 h3。
///
/// 与 `ALPN_TCP` 并排放在这里，是因为「两个监听面的 ALPN 与 early_data 取值
/// 互斥」这条约束属于 `rustls_config` 的契约本身，写在契约旁边才不会在将来
/// 被当成可有可无的细节。使用方见 `crate::http3::bind`。
pub(crate) const ALPN_QUIC: [&[u8]; 1] = [b"h3"];

pub(crate) fn alpn_vec(items: &[&[u8]]) -> Vec<Vec<u8>> {
    items.iter().map(|s| s.to_vec()).collect()
}

/// 读取 PEM 证书 + 私钥 → rustls ServerConfig（TLS 1.3 only）。
///
/// `alpn` 与 `early_data` **由调用方给出，不设默认值**：两个监听面在这两项
/// 上的取值是互斥的，给了默认值就等于把其中一面焊进函数里，另一面每次都
/// 得记得覆盖——那正是将来只改一处便产生静默分歧的地方。
///
///   TCP 443：`ALPN_TCP`，early_data = 16384（§6.8 第 2 层）
///   UDP 443：`ALPN_QUIC`，early_data = `u32::MAX`（要 0-RTT）或 0
///
/// QUIC 只接受 0 或 `u32::MAX`；拿 16384 去 `QuicServerConfig::try_from`
/// 会直接失败。
pub(crate) fn rustls_config(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    alpn: Vec<Vec<u8>>,
    early_data: u32,
) -> Result<ServerConfig> {
    ensure_crypto_provider();
    let mut cfg = ServerConfig::builder_with_protocol_versions(&[&TLS13])
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("证书/私钥加载失败")?;
    cfg.alpn_protocols = alpn;
    // §6.8 第 2 层：开 early data 让浏览器 0-RTT；重放面由
    // msg1 防重放（§6.3）+ seq 去重（§6.4）全额兜底。
    cfg.max_early_data_size = early_data;
    Ok(cfg)
}

/// 该部署模式下的证书链与私钥；明文模式（CDN Flexible）返回 `None`。
///
/// 单独抽出来是因为 TCP 与 UDP 两个监听面都要用同一份材料。各读各的会在
/// 证书轮换的那一瞬间读到两个不同版本——两个面于是拿着不同证书对外服务，
/// 而且不会报错，只在客户端零星出现校验失败。
pub fn tls_material(
    deployment: &Deployment,
) -> Result<Option<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)>> {
    match deployment {
        Deployment::CdnFlexible => Ok(None),
        Deployment::CdnFullSelfSigned => {
            let (cert, key) = self_signed()?;
            Ok(Some((vec![cert], key)))
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
            Ok(Some((certs, key)))
        }
    }
}

/// 由已读出的证书材料构建 ServeMode（TCP 面：ALPN h2/http1.1）。
pub fn mode_from_material(
    material: Option<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)>,
) -> Result<ServeMode> {
    match material {
        None => Ok(ServeMode::Plain),
        Some((certs, key)) => Ok(ServeMode::Tls(TlsAcceptor::from(Arc::new(
            rustls_config(certs, key, alpn_vec(&ALPN_TCP), 16_384)?,
        )))),
    }
}

/// 按部署模式构建 ServeMode（自己读证书；两面共用材料的场景请改用
/// [`tls_material`] + [`mode_from_material`]）。
pub async fn build(deployment: &Deployment) -> Result<ServeMode> {
    mode_from_material(tls_material(deployment)?)
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
    serve_mode(build(deployment).await?, listen, router).await
}

/// 同上，但 `ServeMode` 由调用方给出。
///
/// 启动编排走这个入口：TCP 与 UDP 两面必须共用同一份证书材料，各读各的会
/// 在轮换瞬间拿到两个版本（见 [`tls_material`]）。
pub async fn serve_mode(
    mode: ServeMode,
    listen: SocketAddr,
    router: Router,
) -> Result<SocketAddr> {
    let tcp = TcpListener::bind(listen).await.with_context(|| listen.to_string())?;
    let addr = tcp.local_addr()?;
    match mode {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 两种 TLS 部署模式必须能真正构建出 acceptor。
    ///
    /// 回归守卫：rustls 0.23 的 crypto provider 歧义会让这里 panic，而全部
    /// E2E/集成测试都跑 `cdn-flexible`（明文），这条路径此前零覆盖——服务端
    /// 带证书启动即崩，直到线上才发现。
    #[tokio::test]
    async fn tls_modes_build_acceptor() {
        assert!(matches!(
            build(&Deployment::CdnFullSelfSigned).await.unwrap(),
            ServeMode::Tls(_)
        ));

        // direct 模式读的是 PEM 文件，所以这里必须落地真的 PEM（rcgen 直接给）。
        let ka = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["localhost".to_string()])
            .unwrap()
            .self_signed(&ka)
            .unwrap();
        let dir = std::env::temp_dir().join(format!("wsieve-tls-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        std::fs::write(&cert_path, cert.pem()).unwrap();
        std::fs::write(&key_path, ka.serialize_pem()).unwrap();

        let mode = build(&Deployment::Direct {
            cert_path: cert_path.to_string_lossy().into_owned(),
            key_path: key_path.to_string_lossy().into_owned(),
        })
        .await
        .unwrap();
        assert!(matches!(mode, ServeMode::Tls(_)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn cdn_flexible_is_plain() {
        assert!(matches!(
            build(&Deployment::CdnFlexible).await.unwrap(),
            ServeMode::Plain
        ));
    }

    /// 自签一对证书供 ALPN 测试用（不落文件系统）。
    fn test_cert() -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        let (c, k) = self_signed().unwrap();
        (vec![c], k)
    }

    #[test]
    fn tcp_config_advertises_h2_then_http11() {
        // 顺序即服务端偏好：h2 在前表示优先 h2，客户端不支持时回落 http/1.1。
        // 此前一行都没设，握手结果是 `No ALPN negotiated`——一个 TLS1.3 服务端
        // 在客户端明明提供了 ALPN 的情况下不做任何选择，是可被动识别的异常
        // 特征，真实世界的 HTTPS 服务端几乎全部会协商 h2。
        let (certs, key) = test_cert();
        let cfg = rustls_config(certs, key, alpn_vec(&ALPN_TCP), 16_384).unwrap();
        assert_eq!(
            cfg.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()],
            "TCP 面必须按 h2 优先的顺序宣告 ALPN"
        );
        assert_eq!(
            cfg.max_early_data_size, 16_384,
            "TCP 面的 0-RTT 配置（§6.8 第 2 层）不得被 ALPN 改动波及"
        );
    }

    #[test]
    fn quic_config_advertises_only_h3_with_quic_legal_early_data() {
        // QUIC 只接受 max_early_data_size 为 0 或 u32::MAX。TCP 面那个 16384
        // 拿去 `QuicServerConfig::try_from` 会直接失败——这正是两个监听面
        // 不能共用同一份 config 的根本原因。
        let (certs, key) = test_cert();
        let cfg = rustls_config(certs, key, alpn_vec(&ALPN_QUIC), u32::MAX).unwrap();
        assert_eq!(cfg.alpn_protocols, vec![b"h3".to_vec()]);
        assert_eq!(cfg.max_early_data_size, u32::MAX);
    }
}
