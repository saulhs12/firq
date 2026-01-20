use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use firq_core::SchedulerStats;
pub use firq_core::{
    BackpressurePolicy, DequeueResult, EnqueueResult, Scheduler, SchedulerConfig, Task, TenantKey,
};
use futures_core::Stream;
use tokio::sync::Semaphore;

pub struct AsyncScheduler<T> {
    inner: Arc<Scheduler<T>>,
}

impl<T> Clone for AsyncScheduler<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T> AsyncScheduler<T> {
    pub fn new(inner: Arc<Scheduler<T>>) -> Self {
        Self { inner }
    }

    pub fn inner(&self) -> &Arc<Scheduler<T>> {
        &self.inner
    }

    pub fn enqueue(&self, tenant: TenantKey, task: Task<T>) -> EnqueueResult {
        self.inner.enqueue(tenant, task)
    }

    pub fn try_dequeue(&self) -> DequeueResult<T> {
        self.inner.try_dequeue()
    }

    pub fn stats(&self) -> SchedulerStats {
        self.inner.stats()
    }

    pub fn close(&self) {
        self.inner.close();
    }

    pub fn receiver(&self) -> AsyncReceiver<T> {
        AsyncReceiver::new(self.clone())
    }

    pub fn stream(&self) -> AsyncStream<T> {
        AsyncStream::new(self.clone())
    }
}

impl<T: Send + Sync + 'static> AsyncScheduler<T> {
    /// Espera una tarea sin polling usando la señalización del core.
    pub async fn dequeue_async(&self) -> DequeueResult<T> {
        let scheduler = Arc::clone(&self.inner);
        match tokio::task::spawn_blocking(move || scheduler.dequeue_blocking()).await {
            Ok(result) => result,
            Err(_) => DequeueResult::Closed,
        }
    }
}

pub struct DequeueItem<T> {
    pub tenant: TenantKey,
    pub task: Task<T>,
}

#[derive(Clone)]
pub struct AsyncReceiver<T> {
    scheduler: AsyncScheduler<T>,
}

impl<T> AsyncReceiver<T> {
    pub fn new(scheduler: AsyncScheduler<T>) -> Self {
        Self { scheduler }
    }
}

impl<T: Send + Sync + 'static> AsyncReceiver<T> {
    pub async fn recv(&self) -> Option<DequeueItem<T>> {
        loop {
            match self.scheduler.dequeue_async().await {
                DequeueResult::Task { tenant, task } => {
                    return Some(DequeueItem { tenant, task });
                }
                DequeueResult::Closed => return None,
                DequeueResult::Empty => {
                    tokio::task::yield_now().await;
                }
            }
        }
    }
}

pub struct AsyncStream<T> {
    scheduler: AsyncScheduler<T>,
    pending: Option<Pin<Box<dyn Future<Output = DequeueResult<T>> + Send>>>,
}

impl<T> AsyncStream<T> {
    pub fn new(scheduler: AsyncScheduler<T>) -> Self {
        Self {
            scheduler,
            pending: None,
        }
    }
}

impl<T: Send + Sync + 'static> Stream for AsyncStream<T> {
    type Item = DequeueItem<T>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.pending.is_none() {
            let scheduler = this.scheduler.clone();
            this.pending = Some(Box::pin(async move { scheduler.dequeue_async().await }));
        }

        let pending = match this.pending.as_mut() {
            Some(pending) => pending,
            None => return Poll::Pending,
        };

        match pending.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                this.pending = None;
                match result {
                    DequeueResult::Task { tenant, task } => {
                        Poll::Ready(Some(DequeueItem { tenant, task }))
                    }
                    DequeueResult::Closed => Poll::Ready(None),
                    DequeueResult::Empty => {
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                }
            }
        }
    }
}

pub struct Dispatcher<T> {
    scheduler: AsyncScheduler<T>,
    semaphore: Arc<Semaphore>,
    max_in_flight: usize,
}

impl<T> Dispatcher<T> {
    pub fn new(scheduler: AsyncScheduler<T>, max_in_flight: usize) -> Self {
        let max_in_flight = max_in_flight.max(1);
        Self {
            scheduler,
            semaphore: Arc::new(Semaphore::new(max_in_flight)),
            max_in_flight,
        }
    }
}

impl<T: Send + Sync + 'static> Dispatcher<T> {
    pub async fn run<F, Fut>(&self, handler: F)
    where
        F: Fn(DequeueItem<T>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let handler = Arc::new(handler);
        loop {
            match self.scheduler.dequeue_async().await {
                DequeueResult::Task { tenant, task } => {
                    let permit = match Arc::clone(&self.semaphore).acquire_owned().await {
                        Ok(permit) => permit,
                        Err(_) => break,
                    };
                    let handler = Arc::clone(&handler);
                    tokio::spawn(async move {
                        handler(DequeueItem { tenant, task }).await;
                        drop(permit);
                    });
                }
                DequeueResult::Closed => break,
                DequeueResult::Empty => {
                    tokio::task::yield_now().await;
                }
            }
        }

        let _ = self.semaphore.acquire_many(self.max_in_flight as u32).await;
    }
}
