use firq_async::{AsyncScheduler, EnqueueResult, EnqueueRejectReason, Task, TenantKey};

use pin_project_lite::pin_project;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::oneshot;
use tower_service::Service;

/// Payload interno usado por firq-tower en el scheduler.
/// Contiene el canal para despertar al request cuando sea su turno.
pub struct FirqPermit {
    tx: oneshot::Sender<()>,
}

/// Errores que puede retornar el middleware de Firq.
#[derive(Debug)]
pub enum FirqError<E> {
    /// Error del servicio interior.
    Service(E),
    /// Rechazado por backpressure (GlobalFull o TenantFull).
    Rejected(EnqueueRejectReason),
    /// Scheduler cerrado.
    Closed,
    /// Error interno esperando el permiso (probablemente expirado o worker caído).
    PermitError,
}

impl<E: fmt::Display> fmt::Display for FirqError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FirqError::Service(e) => write!(f, "Service error: {}", e),
            FirqError::Rejected(reason) => write!(f, "Request rejected: {:?}", reason),
            FirqError::Closed => write!(f, "Scheduler closed"),
            FirqError::PermitError => write!(f, "Permit acquisition failed (task expired or dropped)"),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for FirqError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FirqError::Service(e) => Some(e),
            _ => None,
        }
    }
}

/// Trait para extraer la clave de tenant de un request.
pub trait KeyExtractor<Request>: Clone {
    fn extract(&self, req: &Request) -> TenantKey;
}

impl<F, Request> KeyExtractor<Request> for F
where
    F: Fn(&Request) -> TenantKey + Clone,
{
    fn extract(&self, req: &Request) -> TenantKey {
        (self)(req)
    }
}

/// Layer que aplica rate limiting y fairness usando Firq.
#[derive(Clone)]
pub struct FirqLayer<Request, K> {
    scheduler: AsyncScheduler<FirqPermit>,
    extractor: K,
    _marker: PhantomData<Request>,
}

impl<Request, K> FirqLayer<Request, K> {
    /// Crea una nueva capa de Firq.
    ///
    /// Inicia automáticamente un worker en background (tokio::spawn) que
    /// consume permisos del scheduler y los notifica.
    pub fn new(scheduler: AsyncScheduler<FirqPermit>, extractor: K) -> Self {
        {
            let scheduler = scheduler.clone();
            tokio::spawn(async move {
                loop {
                    match scheduler.dequeue_async().await {
                        firq_async::DequeueResult::Task { task, .. } => {
                            let _ = task.payload.tx.send(());
                        }
                        firq_async::DequeueResult::Closed => break,
                        firq_async::DequeueResult::Empty => {
                            // Should not happen with dequeue_async logic
                        }
                    }
                }
            });
        }

        Self {
            scheduler,
            extractor,
            _marker: PhantomData,
        }
    }
}

impl<S, Request, K> tower::Layer<S> for FirqLayer<Request, K>
where
    K: KeyExtractor<Request> + Clone,
{
    type Service = FirqService<S, K, Request>;

    fn layer(&self, inner: S) -> Self::Service {
        FirqService {
            inner,
            scheduler: self.scheduler.clone(),
            extractor: self.extractor.clone(),
            _marker: PhantomData,
        }
    }
}

/// Service middleware que encola requests en Firq.
#[derive(Clone)]
pub struct FirqService<S, K, Request> {
    inner: S,
    scheduler: AsyncScheduler<FirqPermit>,
    extractor: K,
    _marker: PhantomData<Request>,
}

impl<S, K, Request> Service<Request> for FirqService<S, K, Request>
where
    S: Service<Request> + Clone + Send + 'static,
    S::Future: Send + 'static,
    K: KeyExtractor<Request> + Send + 'static,
    Request: Send + 'static,
{
    type Response = S::Response;
    type Error = FirqError<S::Error>;
    type Future = ResponseFuture<S, Request>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(FirqError::Service)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let tenant = self.extractor.extract(&req);
        let (tx, rx) = oneshot::channel();
        
        let task = Task {
            payload: FirqPermit { tx },
            enqueue_ts: std::time::Instant::now(),
            deadline: None,
            priority: Default::default(),
            cost: 1,
        };

        match self.scheduler.enqueue(tenant, task) {
            EnqueueResult::Enqueued => {
                ResponseFuture::WaitingPermit {
                    rx,
                    inner: Some(self.inner.clone()),
                    req: Some(req),
                }
            }
            EnqueueResult::Rejected(reason) => {
                ResponseFuture::Error {
                    error: Some(FirqError::Rejected(reason)),
                }
            }
            EnqueueResult::Closed => {
                ResponseFuture::Error {
                    error: Some(FirqError::Closed),
                }
            }
        }
    }
}

pin_project! {
    #[project = StateProj]
    pub enum ResponseFuture<S, Request>
    where
        S: Service<Request>,
    {
        WaitingPermit {
            #[pin]
            rx: oneshot::Receiver<()>,
            inner: Option<S>,
            req: Option<Request>,
        },
        Calling {
            #[pin]
            fut: S::Future,
        },
        Error {
            error: Option<FirqError<S::Error>>,
        },
    }
}


impl<S, Request> Future for ResponseFuture<S, Request>
where
    S: Service<Request>,
{
    type Output = Result<S::Response, FirqError<S::Error>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
         loop {
            let this = self.as_mut();
            match this.project() {
                StateProj::WaitingPermit { rx, inner, req } => {
                     match rx.poll(cx) {
                        Poll::Ready(Ok(())) => {
                            // Permiso concedido.
                            let mut svc = inner.take().expect("Invalid state: inner missing");
                            let req = req.take().expect("Invalid state: req missing");
                            let call_fut = svc.call(req);
                            self.set(ResponseFuture::Calling { fut: call_fut });
                        }
                        Poll::Ready(Err(_)) => {
                             // Sender dropped implies scheduler dropped or task dropped (expired?).
                             return Poll::Ready(Err(FirqError::PermitError));
                        }
                        Poll::Pending => return Poll::Pending,
                     }
                }
                StateProj::Calling { fut } => {
                    return fut.poll(cx).map_err(FirqError::Service);
                }
                StateProj::Error { error } => {
                    let err = error.take().expect("Polled ResponseFuture::Error twice");
                    return Poll::Ready(Err(err));
                }
            }
         }
    }
}
