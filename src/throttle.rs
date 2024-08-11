use hyper::rt::{Sleep, Timer};
use std::io;
use std::pin::Pin;
use std::sync::{Arc, PoisonError, RwLock};
use std::task::{ready, Context, Poll};
use std::time::{Duration, Instant};

pub(crate) struct Throttle {
    timer: Arc<dyn Timer + Send + Sync>,
    rate: Option<f64>,
    filter: Arc<RwLock<Filter>>,
    sleep: Option<Pin<Box<dyn Sleep>>>,
}

impl Clone for Throttle {
    fn clone(&self) -> Self {
        Self {
            timer: self.timer.clone(),
            rate: self.rate,
            filter: self.filter.clone(),
            sleep: None,
        }
    }
}

impl Throttle {
    pub(crate) fn new(timer: Arc<dyn Timer + Send + Sync>, rate: Option<f64>) -> Self {
        Self {
            timer,
            rate,
            filter: Arc::new(RwLock::new(Filter::new(Duration::from_secs(1)))),
            sleep: None,
        }
    }

    pub(crate) fn poll<F>(
        &mut self,
        cx: &mut Context<'_>,
        mut f: F,
    ) -> Poll<Result<usize, io::Error>>
    where
        F: FnMut(&mut Context<'_>) -> Poll<Result<usize, io::Error>>,
    {
        if let Some(deadline) = self.rate.and_then(|rate| {
            self.filter
                .read()
                .unwrap_or_else(|e| self.clear_poison(e))
                .deadline(rate)
        }) {
            self.sleep = Some(self.timer.sleep_until(deadline));
        }

        if let Some(sleep) = &mut self.sleep {
            ready!(sleep.as_mut().poll(cx));
            self.sleep = None;
        }

        let len = ready!(f(cx)?);
        self.filter
            .write()
            .unwrap_or_else(|e| self.clear_poison(e))
            .update(len as _);

        Poll::Ready(Ok(len))
    }

    fn clear_poison<T>(&self, e: PoisonError<T>) -> T {
        self.filter.clear_poison();
        e.into_inner()
    }
}

struct Filter {
    tau: Duration,
    now: Instant,
    rate: f64,
}

impl Filter {
    fn new(tau: Duration) -> Self {
        Self {
            tau,
            now: Instant::now(),
            rate: 0.,
        }
    }

    fn deadline(&self, rate: f64) -> Option<Instant> {
        if self.rate > rate {
            let duration = (self.rate / rate).ln() * self.tau.as_secs_f64();
            Some(self.now + Duration::from_secs_f64(duration))
        } else {
            None
        }
    }

    fn update(&mut self, delta: f64) {
        let now = Instant::now();
        let elapsed = now.checked_duration_since(self.now).unwrap();
        self.now = now;
        self.rate = (-elapsed.as_secs_f64() / self.tau.as_secs_f64()).exp() * self.rate
            + delta / self.tau.as_secs_f64();
    }
}
