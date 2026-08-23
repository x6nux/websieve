//! HttpTransport：协议层与「谁来发 HTTP」之间的唯一边界。spec §5.1。

use bytes::Bytes;

#[derive(Debug)]
pub struct PostReply {
    pub status: u16,
    pub body: Bytes,
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
        Ok(PostReply { status, body })
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
