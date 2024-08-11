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
//!     .read_rate(65536.) // 64 KiB/s
//!     .build(connector);
//! let connector = HttpsConnectorBuilder::new()
//!     .with_native_roots()?
//!     .https_or_http()
//!     .enable_all_versions()
//!     .wrap_connector(connector);
//! let client = Client::builder(TokioExecutor::new()).build::<_, Empty<Bytes>>(connector);
//! # std::io::Result::Ok(())
//! ```

mod throttle;

use http::Uri;
use hyper::rt::{Read, ReadBuf, ReadBufCursor, Timer, Write};
use std::future;
use std::io::{self, IoSlice};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use throttle::Throttle;
use tower_service::Service;

pub struct ConnectorBuilder {
    timer: Arc<dyn Timer + Send + Sync>,
    read_rate: Option<f64>,
    write_rate: Option<f64>,
}

impl ConnectorBuilder {
    pub fn build<C>(self, inner: C) -> Connector<C> {
        Connector {
            inner,
            read: Throttle::new(self.timer.clone(), self.read_rate),
            write: Throttle::new(self.timer.clone(), self.write_rate),
        }
    }

    #[must_use]
    pub fn read_rate(mut self, rate: f64) -> Self {
        self.read_rate = Some(rate);
        self
    }

    #[must_use]
    pub fn write_rate(mut self, rate: f64) -> Self {
        self.write_rate = Some(rate);
        self
    }
}

#[derive(Clone)]
pub struct Connector<C> {
    inner: C,
    read: Throttle,
    write: Throttle,
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
            read: Some(self.read.clone()),
            write: Some(self.write.clone()),
        }
    }
}

#[pin_project::pin_project]
pub struct Future<F> {
    #[pin]
    inner: F,
    read: Option<Throttle>,
    write: Option<Throttle>,
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
            read: this.read.take().unwrap(),
            write: this.write.take().unwrap(),
        })
    }
}

#[pin_project::pin_project]
pub struct Stream<S> {
    #[pin]
    inner: S,
    read: Throttle,
    write: Throttle,
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
        let len = ready!(this.read.poll(cx, |cx| unsafe {
            let mut buf = ReadBuf::uninit(buf.as_mut());
            ready!(this.inner.as_mut().poll_read(cx, buf.unfilled())?);
            Poll::Ready(Ok(buf.filled().len()))
        })?);
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
        this.write
            .poll(cx, |cx| this.inner.as_mut().poll_write(cx, buf))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let this = self.project();
        this.inner.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let this = self.project();
        this.inner.poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        let mut this = self.project();
        this.write
            .poll(cx, |cx| this.inner.as_mut().poll_write_vectored(cx, bufs))
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

#[cfg(test)]
mod tests;
