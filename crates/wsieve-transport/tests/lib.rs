use axum::{routing::post, Router, body::Bytes as BodyBytes};
use wsieve_transport::{HttpTransport, ReqwestTransport};

#[tokio::test]
async fn post_roundtrip_and_status() {
    let app = Router::new().route(
        "/api/sync",
        post(|body: BodyBytes| async move {
            if body.is_empty() {
                (axum::http::StatusCode::NO_CONTENT, BodyBytes::new())
            } else {
                (axum::http::StatusCode::OK, body)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });

    let t = ReqwestTransport::new(format!("http://{addr}")).unwrap();
    let r = t.post("/api/sync?n=0", bytes::Bytes::from_static(b"hello")).await.unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(&r.body[..], b"hello");

    let r2 = t.post("/api/sync?n=1", bytes::Bytes::new()).await.unwrap();
    assert_eq!(r2.status, 204);
    assert!(r2.body.is_empty());
}

#[tokio::test]
async fn get_stream_non_2xx_is_err() {
    // route /api/events doesn't exist → 404 → get_stream must Err
    let app = Router::new().route("/", post(|| async { "x" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
    let t = ReqwestTransport::new(format!("http://{addr}")).unwrap();
    assert!(t.get_stream("/api/events").await.is_err());
}

#[tokio::test]
async fn get_stream_yields_chunks() {
    use futures::StreamExt;
    use axum::response::sse::{Sse, Event};
    // a streaming route: send 3 events then end
    let stream = futures::stream::iter((0..3).map(|i| {
        Ok::<_, std::convert::Infallible>(Event::default().data(format!("chunk{i}")))
    }));
    let app = Router::new().route(
        "/api/events",
        axum::routing::get(|| async move { Sse::new(stream) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
    let t = ReqwestTransport::new(format!("http://{addr}")).unwrap();
    let mut s = t.get_stream("/api/events").await.unwrap();
    let mut collected = Vec::new();
    while let Some(chunk) = s.next().await {
        collected.extend_from_slice(&chunk.unwrap());
    }
    let text = String::from_utf8(collected).unwrap();
    assert!(text.contains("chunk0") && text.contains("chunk2"));
}
