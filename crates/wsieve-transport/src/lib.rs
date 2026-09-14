//! HttpTransport：协议层与「谁来发 HTTP」之间的唯一边界。spec §5.1。

use bytes::Bytes;

/// 服务端随响应回传的链路观测，来自 `Server-Timing` 头。
///
/// 两样客户端自己拿不到的东西：
///   - `server_us`：服务端自报的处理耗时。从往返里扣掉才是纯网络 RTT，
///     否则服务端的一次抖动（锁等待、GC）会被整个算成"链路变差了"。
///   - `gaps`/`dups`：服务端看到的上行 seq 空洞数与重复数。客户端只知道
///     "我重试了"，分不清是**请求**没到（空洞）还是**响应**丢了（重复）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerObservation {
    pub server_us: u32,
    pub gaps: u8,
    pub dups: u8,
}

#[derive(Debug)]
pub struct PostReply {
    pub status: u16,
    pub body: Bytes,
    /// 服务端侧观测。`None` = 这条传输通路不带回它（例如 reqwest 直连的
    /// 测试装置），不是"服务端没问题"——两者必须能区分，否则画像会把
    /// "没数据"当成"零延迟零丢包"。
    pub peer: Option<PeerObservation>,
}

#[async_trait::async_trait]
pub trait HttpTransport: Send + Sync {
    /// 上行 POST。返回状态码与响应体（n=0 时 body = msg2 的 TU）。
    async fn post(&self, path: &str, body: Bytes) -> anyhow::Result<PostReply>;
    /// 下行：开一条流式 GET。非 2xx 返回 Err。
    async fn get_stream(
        &self,
        path: &str,
    ) -> anyhow::Result<futures::stream::BoxStream<'static, anyhow::Result<Bytes>>>;
}

pub struct ReqwestTransport {
    client: reqwest::Client,
    base: reqwest::Url,
}

impl ReqwestTransport {
    pub fn new(base_url: String) -> anyhow::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                // 自签/无证书模式（spec §8 部署表）：测试与源站直连场景。
                // 生产客户端走 CDN（有效证书）经 WebView，不经过这里。
                .danger_accept_invalid_certs(true)
                .build()?,
            base: base_url.parse()?,
        })
    }
}

#[async_trait::async_trait]
impl HttpTransport for ReqwestTransport {
    async fn post(&self, path: &str, body: Bytes) -> anyhow::Result<PostReply> {
        let url = self.base.join(path)?;
        let resp = self.client.post(url).body(body).send().await?;
        let status = resp.status().as_u16();
        let body = resp.bytes().await?;
        Ok(PostReply { status, body, peer: None })
    }

    async fn get_stream(
        &self,
        path: &str,
    ) -> anyhow::Result<futures::stream::BoxStream<'static, anyhow::Result<Bytes>>> {
        let url = self.base.join(path)?;
        let resp = self.client.get(url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("GET {path} -> {}", status.as_u16());
        }
        use futures::StreamExt;
        let s = resp.bytes_stream().map(|r| r.map_err(anyhow::Error::new));
        Ok(s.boxed())
    }
}

/// 链路观测在回帧里的打包格式：`srv_us+1 : u16 | gaps : u8 | dups : u8`。
///
/// 为什么塞进那 4 个字节而不是新开一个帧或一个 IPC 命令：
///   - 回帧头的第 11..15 字节是下行分片序号，**POST 结果帧从不使用它**，
///     所以这是既有格式里现成的空位，帧大小一个字节都不用动。
///   - 新加 Tauri 命令要改 `capabilities/transport.json` 的授权面，而那个
///     文件的纪律是能不动就不动（授权面越小，承载形态变更的代价越小）。
///   - 多发一帧会让每个 POST 的 IPC 次数翻倍，而回帧是串行的。
///
/// `srv_us` 存实际值 +1，于是全零可以明确表示"没有观测数据"——这与
/// "服务端零耗时零丢包"是两回事，混同会让画像把缺数据当成完美链路。
/// u16 在 65 ms 处饱和，饱和本身就是"服务端非常慢"的信号，不丢信息。
pub fn pack_peer_observation(o: Option<PeerObservation>) -> u32 {
    match o {
        None => 0,
        Some(o) => {
            let us = o.server_us.min(65_534) + 1;
            (us << 16) | ((o.gaps as u32) << 8) | o.dups as u32
        }
    }
}

/// [`pack_peer_observation`] 的逆。
pub fn unpack_peer_observation(v: u32) -> Option<PeerObservation> {
    let us = v >> 16;
    if us == 0 {
        return None;
    }
    Some(PeerObservation {
        server_us: us - 1,
        gaps: ((v >> 8) & 0xff) as u8,
        dups: (v & 0xff) as u8,
    })
}

#[cfg(test)]
mod peer_obs_tests {
    use super::*;

    /// 打包/解包必须是恒等的，否则服务端的观测会被静默改写成别的数字。
    #[test]
    fn packing_a_peer_observation_round_trips() {
        for o in [
            PeerObservation { server_us: 0, gaps: 0, dups: 0 },
            PeerObservation { server_us: 1234, gaps: 3, dups: 7 },
            PeerObservation { server_us: 65_534, gaps: 255, dups: 255 },
        ] {
            assert_eq!(unpack_peer_observation(pack_peer_observation(Some(o))), Some(o));
        }
    }

    /// **"没数据"与"零耗时零丢包"必须可区分。**
    ///
    /// 这两者混同的后果是单向的、且正好是最坏的方向：任何不带 `Server-Timing`
    /// 的传输通路都会被读成"服务端零延迟、零丢包"，画像据此把 RTT 全额算作
    /// 网络往返、把链路判得比实际更好。
    #[test]
    fn absent_observations_are_distinguishable_from_all_zero_ones() {
        assert_eq!(unpack_peer_observation(pack_peer_observation(None)), None);
        let zero = PeerObservation { server_us: 0, gaps: 0, dups: 0 };
        assert_ne!(pack_peer_observation(Some(zero)), pack_peer_observation(None));
    }

    /// 超过 u16 的服务端耗时饱和而不是回绕——回绕会把"非常慢"读成"非常快"。
    #[test]
    fn an_absurdly_slow_server_saturates_instead_of_wrapping() {
        let slow = PeerObservation { server_us: 5_000_000, gaps: 0, dups: 0 };
        let got = unpack_peer_observation(pack_peer_observation(Some(slow))).unwrap();
        assert_eq!(got.server_us, 65_534);
    }
}
