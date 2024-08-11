use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioTimer};
use std::time::{Duration, Instant};

type Client = hyper_util::client::legacy::Client<
    HttpsConnector<super::Connector<HttpConnector>>,
    Empty<Bytes>,
>;

fn client(read_rate: Option<f64>, write_rate: Option<f64>) -> Client {
    let mut connector = HttpConnector::new();
    connector.enforce_http(false);
    let mut builder = super::Connector::builder(TokioTimer::new());
    if let Some(rate) = read_rate {
        builder = builder.read_rate(rate);
    }
    if let Some(rate) = write_rate {
        builder = builder.write_rate(rate);
    }
    let connector = builder.build(connector);
    let connector = HttpsConnectorBuilder::new()
        .with_native_roots()
        .unwrap()
        .https_only()
        .enable_http1()
        .enable_http2()
        .wrap_connector(connector);
    hyper_util::client::legacy::Client::builder(TokioExecutor::new()).build(connector)
}

async fn get(client: &Client, uri: &str) {
    let response = client.get(uri.parse().unwrap()).await.unwrap();
    assert!(response.status().is_success());
    response.into_body().collect().await.unwrap();
}

#[tokio::test]
async fn test_throttle() {
    let client = client(Some(4096.), Some(4096.));
    let now = Instant::now();
    get(&client, "https://www.rust-lang.org").await;
    get(&client, "https://www.rust-lang.org").await;
    assert!(now.elapsed() > Duration::from_secs(4));
}

#[tokio::test]
async fn test_passthrough() {
    let client = client(None, None);
    let now = Instant::now();
    get(&client, "https://www.rust-lang.org").await;
    get(&client, "https://www.rust-lang.org").await;
    assert!(now.elapsed() < Duration::from_secs(2));
}
