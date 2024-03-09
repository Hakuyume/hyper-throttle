use http::Uri;
use hyper::rt::{Read, ReadBuf, ReadBufCursor, Write};
use std::future::{self, Future as _};
use std::io;
use std::pin::Pin;
use std::task::{ready, Context, Poll};
use std::time::Duration;
use tokio::time::{self, Sleep};
use tower_service::Service;

#[derive(Clone, Copy, Debug)]
pub struct Connector<T> {
    inner: T,
    read_rate: Option<u64>,
    write_rate: Option<u64>,
}

impl<T> Connector<T> {
    pub fn new(inner: T, read_rate: Option<u64>, write_rate: Option<u64>) -> Self {
        Self {
            inner,
            read_rate,
            write_rate,
        }
    }
}

impl<T> Service<Uri> for Connector<T>
where
    T: Service<Uri>,
{
    type Response = Stream<T::Response>;
    type Error = T::Error;
    type Future = Future<T::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Uri) -> Self::Future {
        Future {
            inner: self.inner.call(request),
            read_rate: self.read_rate,
            write_rate: self.write_rate,
        }
    }
}

pub struct Stream<T> {
    inner: T,
    read_rate: Option<u64>,
    write_rate: Option<u64>,
    read_sleep: Option<(usize, Pin<Box<Sleep>>)>,
    write_sleep: Option<(usize, Pin<Box<Sleep>>)>,
}

fn call<F>(
    cx: &mut Context<'_>,
    rate: Option<u64>,
    sleep: &mut Option<(usize, Pin<Box<Sleep>>)>,
    mut f: F,
) -> Poll<Result<usize, io::Error>>
where
    F: FnMut(&mut Context<'_>) -> Poll<Result<usize, io::Error>>,
{
    loop {
        if let Some((len, ref mut f)) = *sleep {
            ready!(f.as_mut().poll(cx));
            *sleep = None;
            break Poll::Ready(Ok(len));
        } else {
            let len = ready!(f(cx)?);
            if let Some(rate) = rate {
                *sleep = Some((
                    len,
                    Box::pin(time::sleep(Duration::from_nanos(
                        len as u64 * 1_000_000_000 / rate,
                    ))),
                ));
            } else {
                break Poll::Ready(Ok(len));
            }
        }
    }
}

impl<T> Read for Stream<T>
where
    T: Read + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: ReadBufCursor<'_>,
    ) -> Poll<Result<(), io::Error>> {
        let this = self.get_mut();
        let len = ready!(call(
            cx,
            this.read_rate,
            &mut this.read_sleep,
            |cx| unsafe {
                let mut buf = ReadBuf::uninit(buf.as_mut());
                ready!(Pin::new(&mut this.inner).poll_read(cx, buf.unfilled())?);
                Poll::Ready(Ok(buf.filled().len()))
            }
        )?);
        unsafe { buf.advance(len) };
        Poll::Ready(Ok(()))
    }
}

impl<T> Write for Stream<T>
where
    T: Write + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let this = self.get_mut();
        call(cx, this.write_rate, &mut this.write_sleep, |cx| {
            Pin::new(&mut this.inner).poll_write(cx, buf)
        })
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

#[cfg(feature = "hyper-util")]
impl<T> hyper_util::client::legacy::connect::Connection for Stream<T>
where
    T: hyper_util::client::legacy::connect::Connection,
{
    fn connected(&self) -> hyper_util::client::legacy::connect::Connected {
        self.inner.connected()
    }
}

#[pin_project::pin_project]
pub struct Future<T> {
    #[pin]
    inner: T,
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
    use hyper_util::rt::TokioExecutor;
    use std::time::{Duration, Instant};

    fn client(
        read_rate: Option<u64>,
        write_rate: Option<u64>,
    ) -> Client<HttpsConnector<super::Connector<HttpConnector>>, Empty<Bytes>> {
        let mut connector = HttpConnector::new();
        connector.enforce_http(false);
        let connector = super::Connector::new(connector, read_rate, write_rate);
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
