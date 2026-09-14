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

/// TLS 握手的上限。超时的连接直接丢弃。
///
/// **必须大于客户端预建池的 TTL**（`src-tauri` 的 `shard::PREWARM_TTL`，
/// 当前 45s）。预建池的全部意义就是提前把 TCP 建好静置着，等真有请求时省掉
/// 一次跨境 RTT；服务端若比那个 TTL 更早掐掉静默连接，预建出来的连接会在被
/// 用掉之前就死掉，于是：
///   - 省 RTT 的收益归零（取用时探活发现已死，只能现连）
///   - 还平白多出一轮连接抖动（每个端口每 `HANDSHAKE_TIMEOUT` 死一批、补一批）
///
/// 这个坑很隐蔽：两个常量分属两个 crate，编译器管不到，症状只是「开了多会话
/// 反而更慢」。2026-09-11 实测就是先取了 10s，把多会话组的测量整个带偏。
///
/// 60s 同时也在常见 HTTPS 服务端的量级内（nginx 的 `client_header_timeout`
/// 默认就是 60s），不构成可被动识别的异常特征。真正挡住「连上不说话」那种
/// DoS 的是**把握手挪出 accept 循环**，不是这个上限——上限只负责不让静默
/// 连接无限堆积。
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// 同时进行的 TLS 握手上限，见 accept 循环里的说明。
///
/// 512 远高于任何真实客户端的需求（一个客户端最多开十几条会话），却足以把
/// 半开连接的内存与 fd 占用框在一个可预期的量级：512 × (一个 fd + 一份 rustls
/// 缓冲) 是几 MB，而无上限时它只由攻击者的发包速率决定。
const MAX_CONCURRENT_HANDSHAKES: usize = 512;

/// 连续多少次 accept 失败才真的放弃监听。
const MAX_ACCEPT_ERRS: u32 = 64;

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
                //
                // **握手必须在 spawn 之后做**，绝不能 await 在 accept 循环里。
                // 曾经是 `let Ok(tls) = acceptor.accept(stream).await else`——
                // 一条连上来却不发 ClientHello 的连接会把整个循环卡死，服务端
                // 从此不再接受任何新连接。两种现实触发方式：
                //   - 客户端的预建 TCP 池（`src-tauri` 的 shard 转发器）提前
                //     建好连接静置，省一次跨境 RTT；2026-09-11 实测 8 条预建
                //     把服务端 accept 队列顶到 29 条全部饿死
                //   - 任何人 `nc host 443` 然后什么都不发，就是一次完整的 DoS
                // 并发握手上限。
                //
                // 把握手挪进任务解决了"一条静默连接堵死所有人"，但也拿掉了
                // 原先那条 `acceptor.accept(stream).await` 隐式提供的串行化：
                // 此后每个到达的连接都无条件 spawn 一个任务，内存与 fd 只由
                // 攻击者的发包速率决定。`for i in $(seq 1 50000); do nc -w0
                // host 443 & done` 或一个慢速 ClientHello，都能让成千上万个
                // 半开握手各自占住一个 fd、一份 rustls 缓冲和一个 hyper builder
                // 达 60 秒，直到 EMFILE 或 OOM。
                //
                // 信号量把它重新框住，又不重新引入队头阻塞：许可满了只是让
                // accept 停一下，已经在握手的那些照常各自推进。
                let gate = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_HANDSHAKES));
                let mut accept_errs = 0u32;
                loop {
                    // 先拿许可再 accept：反过来的话连接已经从内核队列里取出来
                    // 了，却要在用户态排队等许可，等于把积压从内核搬到自己身上。
                    let Ok(permit) = gate.clone().acquire_owned().await else {
                        return;
                    };
                    let stream = match tcp.accept().await {
                        Ok((s, _)) => {
                            accept_errs = 0;
                            s
                        }
                        Err(e) => {
                            // accept 出错**不能**当成"监听结束"。EMFILE 是瞬时的
                            // （别处释放 fd 就好了），而原先的 `let Ok(..) else
                            // { return }` 会让一次 fd 压力永久杀死监听，且零日志。
                            accept_errs += 1;
                            if accept_errs > MAX_ACCEPT_ERRS {
                                eprintln!("tls: 连续 {accept_errs} 次 accept 失败，停止监听: {e}");
                                return;
                            }
                            eprintln!("tls: accept 失败（{e}），退避后继续");
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            continue;
                        }
                    };
                    let acceptor = acceptor.clone();
                    let service = hyper_util::service::TowerToHyperService::new(router.clone());
                    let builder = mk_builder();
                    tokio::spawn(async move {
                        // 握手超时：光把握手挪进任务只解决了「堵住别人」，
                        // 堆积本身还在——不说话的连接会一直占着 fd 与内存。
                        let tls = match tokio::time::timeout(
                            HANDSHAKE_TIMEOUT,
                            acceptor.accept(stream),
                        )
                        .await
                        {
                            Ok(Ok(tls)) => tls,
                            // 握手失败是常态噪声（端口扫描、探测、客户端放弃），
                            // 不值得每条都刷一行；真正的配置错误会在别处显形。
                            Ok(Err(_)) => return,
                            Err(_) => return,
                        };
                        // 握手完成就交还许可：后面是正常的长连接服务，不该占
                        // 着握手配额——那个配额防的是半开连接堆积。
                        drop(permit);
                        let io = hyper_util::rt::TokioIo::new(tls);
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

    /// **一条不发 ClientHello 的连接不得挡住其他连接。**
    ///
    /// 曾经的实现把 `acceptor.accept(stream).await` 直接写在 accept 循环里，
    /// 于是任何一条「连上就不说话」的 TCP 都会把整个循环卡死——服务端从此
    /// 不再接受任何新连接，且全程没有一条错误日志。
    ///
    /// 2026-09-11 真机实测：客户端条带的预建 TCP 池（提前建好连接静置以省
    /// 一次跨境 RTT）8 条就把服务端的 accept 队列顶到 29 条全部饿死，表现为
    /// 「所有会话 60 秒超时」，排查了很久才落到这一行上。任何人 `nc host 443`
    /// 之后什么都不发，也是同一次完整的 DoS。
    ///
    /// 断言用「哑连接先到、正常请求后到」的顺序：反过来的话，正常请求在哑
    /// 连接产生影响之前就已经服务完了，测试会在坏实现上照样变绿。
    #[tokio::test]
    async fn a_silent_connection_does_not_block_the_accept_loop() {
        let (cert, key) = self_signed().unwrap();
        let cfg = rustls_config(vec![cert.clone()], key, alpn_vec(&ALPN_TCP), 16_384).unwrap();
        let mode = ServeMode::Tls(tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(cfg)));
        let router = Router::new().route("/ping", axum::routing::get(|| async { "pong" }));
        let addr = serve_mode(mode, "127.0.0.1:0".parse().unwrap(), router)
            .await
            .unwrap();

        // 哑连接：连上，一个字节都不发，并且**一直持有**到断言之后。
        // 坏实现下第一条就够卡死，多开几条是为了排除「恰好被调度绕开」。
        let _silent = tokio::net::TcpStream::connect(addr).await.unwrap();
        let _silent2 = tokio::net::TcpStream::connect(addr).await.unwrap();
        let _silent3 = tokio::net::TcpStream::connect(addr).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // 判据用**真握手**：能协商出 ALPN，就证明服务端确实在服务这条连接。
        // 不用「发半截 TLS 记录看会不会很快报错」——rustls 会等后续字节而
        // 不是立刻拒绝，那种探针只会撞上 HANDSHAKE_TIMEOUT，好坏实现都变红。
        let mut roots = tokio_rustls::rustls::RootCertStore::empty();
        roots.add(cert).unwrap();
        let mut cc = tokio_rustls::rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        cc.alpn_protocols = alpn_vec(&ALPN_TCP);
        let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(cc));

        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let tls = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            connector.connect("localhost".try_into().unwrap(), tcp),
        )
        .await
        .expect("哑连接把 accept 循环卡死了：握手必须在 spawn 之后做，不能 await 在循环里")
        .expect("握手本身应当成功");
        assert_eq!(
            tls.get_ref().1.alpn_protocol(),
            Some(&b"h2"[..]),
            "握手完成了但没协商出 ALPN，说明服务的不是我们这份配置"
        );
    }

    /// **TLS1.3 0-RTT 真的能落地**：重连时请求随 ClientHello 一起发出，
    /// 不必再等一轮握手。
    ///
    /// `max_early_data_size` 设了不等于 0-RTT 成立——它只是「接受 early data
    /// 的上限」，前提是客户端手里得有会话票据，而票据要服务端先发。rustls
    /// 在没配 ticketer 时走**有状态票据**（默认的 `session_storage` 内存
    /// 缓存），所以今天是发得出的；但这依赖的是 rustls 的默认值，哪天默认值
    /// 变了或者有人显式塞了个 `NeverProducesTickets`，0-RTT 会**静默失效**：
    /// 握手照常成功，只是每次都多一轮 RTT，没有任何报错。这条测试就是为了
    /// 让那种变化当场变红。
    ///
    /// 判据是 `is_early_data_accepted()`——不是「第二次连接更快」那种会因
    /// 机器负载而抖动的间接指标。
    /// 把两端的 TLS 记录来回搬到双方都无话可说为止（内存握手，不走网络）。
    fn pump(
        c: &mut tokio_rustls::rustls::ClientConnection,
        s: &mut tokio_rustls::rustls::ServerConnection,
    ) {
        for _ in 0..32 {
            let mut moved = false;
            let mut up = Vec::new();
            c.write_tls(&mut up).unwrap();
            if !up.is_empty() {
                s.read_tls(&mut up.as_slice()).unwrap();
                s.process_new_packets().unwrap();
                moved = true;
            }
            let mut down = Vec::new();
            s.write_tls(&mut down).unwrap();
            if !down.is_empty() {
                c.read_tls(&mut down.as_slice()).unwrap();
                c.process_new_packets().unwrap();
                moved = true;
            }
            if !moved {
                return;
            }
        }
        panic!("握手没有收敛");
    }

    #[test]
    fn a_resumed_connection_really_sends_its_request_as_0rtt() {
        use std::io::{Read, Write};
        use tokio_rustls::rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConnection};

        let (cert, key) = self_signed().unwrap();
        let scfg = std::sync::Arc::new(
            rustls_config(vec![cert.clone()], key, alpn_vec(&ALPN_TCP), 16_384).unwrap(),
        );

        let mut roots = RootCertStore::empty();
        roots.add(cert).unwrap();
        let mut cc = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        // 只提 http/1.1：early data 里发的是一个完整 HTTP 请求，h2 的 SETTINGS
        // 前奏会把这条测试的重点从「0-RTT 成不成立」漂移到「h2 前奏怎么写」。
        cc.alpn_protocols = vec![b"http/1.1".to_vec()];
        cc.enable_early_data = true;
        // 同一份 config ⇒ 同一个 resumption 存储，第一次拿到的票据第二次能用。
        let cc = std::sync::Arc::new(cc);
        let name: tokio_rustls::rustls::pki_types::ServerName<'static> =
            "localhost".try_into().unwrap();

        // 第一次：完整握手。票据是握手**之后**才发的 NewSessionTicket，所以
        // 必须一直搬到双方都无话可说，中途停下就等于没拿到票。
        let mut c1 = ClientConnection::new(cc.clone(), name.clone()).unwrap();
        let mut s1 = ServerConnection::new(scfg.clone()).unwrap();
        pump(&mut c1, &mut s1);
        assert!(
            c1.early_data().is_none(),
            "第一次连接手里还没有票据，不该有 early data 通道——若这里成立，\
             说明前后对照失效了"
        );

        // 第二次：带着票据重连，请求作为 0-RTT 数据随 ClientHello 一起发出。
        let mut c2 = ClientConnection::new(cc, name).unwrap();
        {
            let mut ed = c2.early_data().expect(
                "客户端拿不到 early data 通道：服务端没发出会话票据。\
                 rustls 在没配 ticketer 时走有状态票据（默认的 session_storage），\
                 若那个默认值被改成 NeverProducesTickets，0-RTT 会静默失效",
            );
            ed.write_all(b"GET /ping HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .unwrap();
        }
        let mut s2 = ServerConnection::new(scfg).unwrap();
        pump(&mut c2, &mut s2);

        assert!(
            c2.is_early_data_accepted(),
            "重连没有走 0-RTT：请求仍要等一轮握手。检查 max_early_data_size \
             与服务端是否真的发出了会话票据"
        );
        // 服务端这侧要真的**读到**那些字节——只有客户端说「被接受了」而服务端
        // 手上没有数据的话，0-RTT 就只是个空壳。
        let mut got = Vec::new();
        if let Some(mut ed) = s2.early_data() {

            ed.read_to_end(&mut got).unwrap();
        }
        assert!(
            String::from_utf8_lossy(&got).starts_with("GET /ping"),
            "服务端没读到 0-RTT 数据，实收 {} 字节",
            got.len()
        );
    }

    /// 握手超时既要有上限，又**必须容得下客户端的预建池 TTL**。
    ///
    /// 只把握手挪进 spawn 解决的是「堵住别人」，堆积本身还在——一个慢速
    /// 攻击者可以用几万条静默连接把进程拖垮，所以要有上限。
    ///
    /// 下限那条才是容易踩的：客户端的预建池（`src-tauri` 的
    /// `shard::PREWARM_TTL = 45s`）会提前建好 TCP 静置着，服务端比它更早掐
    /// 连接的话，预建连接在被用掉之前就死了——省 RTT 的收益归零，还多出
    /// 一轮连接抖动。两个常量分属两个 crate，编译器管不到，症状只是「开了
    /// 多会话反而更慢」，所以只能由这条测试守着。
    #[test]
    fn handshake_timeout_outlives_the_client_prewarm_ttl() {
        // 对端常量：src-tauri `shard::PREWARM_TTL`。改那边就要同步改这里。
        const CLIENT_PREWARM_TTL: std::time::Duration = std::time::Duration::from_secs(45);
        assert!(
            HANDSHAKE_TIMEOUT > CLIENT_PREWARM_TTL,
            "握手上限 {HANDSHAKE_TIMEOUT:?} ≤ 客户端预建 TTL {CLIENT_PREWARM_TTL:?}：\
             预建连接会在被用掉前就被服务端掐死，预建等于白做还多出连接抖动"
        );
        assert!(
            HANDSHAKE_TIMEOUT <= std::time::Duration::from_secs(120),
            "太长等于没有上限，静默连接照样堆积"
        );
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
