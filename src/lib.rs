//! ```rust
//! use bytes::Bytes;
//! use http_body_util::{BodyExt, Empty};
//! use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
//! use hyper_util::client::legacy::connect::HttpConnector;
//! use hyper_util::client::legacy::Client;
//! use hyper_util::rt::{TokioExecutor, TokioTimer};
//!
//! let mut connector = HttpConnector::new();
//! connector.enforce_http(false);
//! let connector = hyper_throttle::Connector::builder(TokioTimer::new())
//!     .read_rate(65536) // 64 KiB/s
//!     .build(connector);
//! let connector = HttpsConnectorBuilder::new()
//!     .with_native_roots()?
//!     .https_or_http()
//!     .enable_all_versions()
//!     .wrap_connector(connector);
//! let client = Client::builder(TokioExecutor::new()).build::<_, Empty<Bytes>>(connector);
//! # std::io::Result::Ok(())
//! ```

use http::Uri;
use hyper::rt::{Read, ReadBuf, ReadBufCursor, Sleep, Timer, Write};
use std::future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use std::time::Duration;
use tower_service::Service;

pub struct ConnectorBuilder {
    timer: Arc<dyn Timer + Send + Sync>,
    read_rate: Option<u64>,
    write_rate: Option<u64>,
}

impl ConnectorBuilder {
    pub fn build<C>(self, inner: C) -> Connector<C> {
        Connector {
            inner,
            timer: self.timer,
            read_rate: self.read_rate,
            write_rate: self.write_rate,
        }
    }

    #[must_use]
    pub fn read_rate(mut self, rate: u64) -> Self {
        self.read_rate = Some(rate);
        self
    }

    #[must_use]
    pub fn write_rate(mut self, rate: u64) -> Self {
        self.write_rate = Some(rate);
        self
    }
}

#[derive(Clone)]
pub struct Connector<C> {
    inner: C,
    timer: Arc<dyn Timer + Send + Sync>,
    read_rate: Option<u64>,
    write_rate: Option<u64>,
}

impl Connector<()> {
    pub fn builder<T>(timer: T) -> ConnectorBuilder
    where
        T: Timer + Send + Sync + 'static,
    {
        ConnectorBuilder {
            timer: Arc::new(timer),
            read_rate: None,
            write_rate: None,
        }
    }
}

impl<C> Service<Uri> for Connector<C>
where
    C: Service<Uri>,
{
    type Response = Stream<C::Response>;
    type Error = C::Error;
    type Future = Future<C::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Uri) -> Self::Future {
        Future {
            inner: self.inner.call(request),
            timer: self.timer.clone(),
            read_rate: self.read_rate,
            write_rate: self.write_rate,
        }
    }
}

#[pin_project::pin_project]
pub struct Stream<S> {
    #[pin]
    inner: S,
    timer: Arc<dyn Timer + Send + Sync>,
    read_rate: Option<u64>,
    write_rate: Option<u64>,
    read_sleep: Option<(usize, Pin<Box<dyn Sleep>>)>,
    write_sleep: Option<(usize, Pin<Box<dyn Sleep>>)>,
}

fn call<T, F>(
    cx: &mut Context<'_>,
    timer: &T,
    rate: Option<u64>,
    sleep: &mut Option<(usize, Pin<Box<dyn Sleep>>)>,
    mut f: F,
) -> Poll<Result<usize, io::Error>>
where
    T: Timer + ?Sized,
    F: FnMut(&mut Context<'_>) -> Poll<Result<usize, io::Error>>,
{
    loop {
        if let Some((len, ref mut f)) = *sleep {
            ready!(f.as_mut().poll(cx));
            *sleep = None;
            break Poll::Ready(Ok(len));
        }
        let len = ready!(f(cx)?);
        if let Some(rate) = rate {
            *sleep = Some((
                len,
                timer.sleep(Duration::from_nanos(len as u64 * 1_000_000_000 / rate)),
            ));
        } else {
            break Poll::Ready(Ok(len));
        }
    }
}

impl<S> Read for Stream<S>
where
    S: Read,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: ReadBufCursor<'_>,
    ) -> Poll<Result<(), io::Error>> {
        let mut this = self.project();
        let len = ready!(call(
            cx,
            this.timer.as_ref(),
            *this.read_rate,
            this.read_sleep,
            |cx| unsafe {
                let mut buf = ReadBuf::uninit(buf.as_mut());
                ready!(this.inner.as_mut().poll_read(cx, buf.unfilled())?);
                Poll::Ready(Ok(buf.filled().len()))
            }
        )?);
        unsafe { buf.advance(len) };
        Poll::Ready(Ok(()))
    }
}

impl<S> Write for Stream<S>
where
    S: Write,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let mut this = self.project();
        call(
            cx,
            this.timer.as_ref(),
            *this.write_rate,
            this.write_sleep,
            |cx| this.inner.as_mut().poll_write(cx, buf),
        )
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let this = self.project();
        this.inner.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let this = self.project();
        this.inner.poll_shutdown(cx)
    }
}

#[cfg(feature = "hyper-util")]
impl<S> hyper_util::client::legacy::connect::Connection for Stream<S>
where
    S: hyper_util::client::legacy::connect::Connection,
{
    fn connected(&self) -> hyper_util::client::legacy::connect::Connected {
        self.inner.connected()
    }
}

#[pin_project::pin_project]
pub struct Future<F> {
    #[pin]
    inner: F,
    timer: Arc<dyn Timer + Send + Sync>,
    read_rate: Option<u64>,
    write_rate: Option<u64>,
}

impl<F, T, E> future::Future for Future<F>
where
    F: future::Future<Output = Result<T, E>>,
{
    type Output = Result<Stream<T>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        this.inner.poll(cx).map_ok(|inner| Stream {
            inner,
            timer: this.timer.clone(),
            read_rate: *this.read_rate,
            write_rate: *this.write_rate,
            read_sleep: None,
            write_sleep: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use http_body_util::{BodyExt, Empty};
    use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
    use hyper_util::client::legacy::connect::HttpConnector;
    use hyper_util::client::legacy::Client;
    use hyper_util::rt::{TokioExecutor, TokioTimer};
    use std::time::{Duration, Instant};

    fn client(
        read_rate: Option<u64>,
        write_rate: Option<u64>,
    ) -> Client<HttpsConnector<super::Connector<HttpConnector>>, Empty<Bytes>> {
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
        Client::builder(TokioExecutor::new()).build(connector)
    }

    #[tokio::test]
    async fn test_thorottle() {
        let client = client(Some(4096), Some(4096));
        let now = Instant::now();
        let response = client
            .get("https://www.rust-lang.org".parse().unwrap())
            .await
            .unwrap();
        assert!(response.status().is_success());
        response.into_body().collect().await.unwrap();
        assert!(now.elapsed() > Duration::from_secs(4));
    }

    #[tokio::test]
    async fn test_passthrough() {
        let client = client(None, None);
        let now = Instant::now();
        let response = client
            .get("https://www.rust-lang.org".parse().unwrap())
            .await
            .unwrap();
        assert!(response.status().is_success());
        response.into_body().collect().await.unwrap();
        assert!(now.elapsed() < Duration::from_secs(2));
    }
}
