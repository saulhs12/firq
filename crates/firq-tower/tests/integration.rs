use firq_tower::{Firq, TenantKey};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tower::{Service, ServiceBuilder, ServiceExt};

#[derive(Clone)]
struct MockService;

impl Service<String> for MockService {
    type Response = String;
    type Error = std::io::Error;
    type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: String) -> Self::Future {
        std::future::ready(Ok(format!("Processed: {}", req)))
    }
}

#[derive(Clone)]
struct BlockingService {
    calls: Arc<AtomicUsize>,
    first_started: Arc<Notify>,
    release_first: Arc<Notify>,
}

#[derive(Clone)]
struct TestRequest {
    tenant: u64,
    payload: &'static str,
}

impl Service<TestRequest> for BlockingService {
    type Response = &'static str;
    type Error = std::io::Error;
    type Future =
        Pin<Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: TestRequest) -> Self::Future {
        let calls = Arc::clone(&self.calls);
        let first_started = Arc::clone(&self.first_started);
        let release_first = Arc::clone(&self.release_first);
        Box::pin(async move {
            let observed = calls.fetch_add(1, Ordering::SeqCst);
            if observed == 0 {
                first_started.notify_waiters();
                release_first.notified().await;
            }
            Ok(req.payload)
        })
    }
}

#[derive(Clone)]
struct OrderedService {
    starts: Arc<Mutex<Vec<&'static str>>>,
}

impl Service<TestRequest> for OrderedService {
    type Response = &'static str;
    type Error = std::io::Error;
    type Future =
        Pin<Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: TestRequest) -> Self::Future {
        let starts = Arc::clone(&self.starts);
        Box::pin(async move {
            starts
                .lock()
                .expect("start-order mutex poisoned")
                .push(req.payload);
            tokio::time::sleep(Duration::from_millis(20)).await;
            Ok(req.payload)
        })
    }
}

#[derive(Clone)]
struct TrackingService {
    active: Arc<AtomicUsize>,
    max_active: Arc<AtomicUsize>,
    executed: Arc<AtomicUsize>,
}

impl Service<TestRequest> for TrackingService {
    type Response = &'static str;
    type Error = std::io::Error;
    type Future =
        Pin<Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: TestRequest) -> Self::Future {
        struct ActiveGuard {
            active: Arc<AtomicUsize>,
        }
        impl Drop for ActiveGuard {
            fn drop(&mut self) {
                self.active.fetch_sub(1, Ordering::SeqCst);
            }
        }

        let active = Arc::clone(&self.active);
        let max_active = Arc::clone(&self.max_active);
        let executed = Arc::clone(&self.executed);
        Box::pin(async move {
            let now_active = active.fetch_add(1, Ordering::SeqCst) + 1;
            let _guard = ActiveGuard {
                active: Arc::clone(&active),
            };
            let mut seen_max = max_active.load(Ordering::SeqCst);
            while now_active > seen_max {
                match max_active.compare_exchange(
                    seen_max,
                    now_active,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => break,
                    Err(actual) => seen_max = actual,
                }
            }

            tokio::time::sleep(Duration::from_millis(40)).await;
            executed.fetch_add(1, Ordering::SeqCst);
            Ok(req.payload)
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_firq_tower_integration() {
    let layer = Firq::new()
        .with_shards(1)
        .with_max_global(10)
        .with_max_per_tenant(5)
        .with_quantum(10)
        .build(|req: &String| TenantKey::from(req.len() as u64));
    let scheduler = layer.scheduler().clone();

    let mut service = ServiceBuilder::new().layer(layer).service(MockService);

    let req = "test".to_string();
    let ready = tokio::time::timeout(Duration::from_secs(10), service.ready())
        .await
        .expect("service readiness timed out")
        .map_err(std::io::Error::other)
        .expect("service readiness failed");

    let result = tokio::time::timeout(Duration::from_secs(10), ready.call(req))
        .await
        .unwrap_or_else(|_| panic!("service call timed out, stats={:?}", scheduler.stats()));
    let result: Result<String, _> = result;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "Processed: test");

    scheduler.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_abort_before_handler_turn_keeps_second_request_unexecuted() {
    let calls = Arc::new(AtomicUsize::new(0));
    let first_started = Arc::new(Notify::new());
    let release_first = Arc::new(Notify::new());

    let layer = Firq::new()
        .with_shards(1)
        .with_max_global(32)
        .with_max_per_tenant(32)
        .with_quantum(10)
        .with_in_flight_limit(1)
        .build(|req: &TestRequest| TenantKey::from(req.tenant));
    let scheduler = layer.scheduler().clone();

    let service = ServiceBuilder::new().layer(layer).service(BlockingService {
        calls: Arc::clone(&calls),
        first_started: Arc::clone(&first_started),
        release_first: Arc::clone(&release_first),
    });

    let mut first_service = service.clone();
    let first = tokio::spawn(async move {
        let ready = first_service
            .ready()
            .await
            .expect("first readiness should succeed");
        ready
            .call(TestRequest {
                tenant: 1,
                payload: "first",
            })
            .await
            .expect("first request should complete")
    });

    tokio::time::timeout(Duration::from_secs(2), first_started.notified())
        .await
        .expect("first handler should start and hold in-flight slot");

    let mut second_service = service.clone();
    let second = tokio::spawn(async move {
        let ready = second_service
            .ready()
            .await
            .expect("second readiness should succeed");
        ready
            .call(TestRequest {
                tenant: 1,
                payload: "second",
            })
            .await
    });

    tokio::time::sleep(Duration::from_millis(30)).await;
    second.abort();
    let aborted = second.await;
    assert!(
        aborted.is_err(),
        "second task should be aborted by client drop"
    );

    release_first.notify_waiters();
    let first_payload = tokio::time::timeout(Duration::from_secs(2), first)
        .await
        .expect("first join should complete")
        .expect("first join should succeed");
    assert_eq!(first_payload, "first");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "second request must not reach handler execution"
    );

    let mut third_service = service.clone();
    let third_payload = tokio::time::timeout(Duration::from_secs(2), async move {
        let ready = third_service
            .ready()
            .await
            .expect("third readiness should succeed");
        ready
            .call(TestRequest {
                tenant: 1,
                payload: "third",
            })
            .await
            .expect("third request should complete")
    })
    .await
    .expect("third request should not deadlock");
    assert_eq!(third_payload, "third");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "third request should execute after abort frees permit"
    );

    scheduler.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrency_and_cancellation_no_deadlock_or_permit_leak() {
    let in_flight_limit = 3usize;
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let executed = Arc::new(AtomicUsize::new(0));

    let layer = Firq::new()
        .with_shards(2)
        .with_max_global(128)
        .with_max_per_tenant(64)
        .with_quantum(1)
        .with_in_flight_limit(in_flight_limit)
        .build(|req: &TestRequest| TenantKey::from(req.tenant));
    let scheduler = layer.scheduler().clone();
    let inspector = layer.clone();

    let service = ServiceBuilder::new().layer(layer).service(TrackingService {
        active: Arc::clone(&active),
        max_active: Arc::clone(&max_active),
        executed: Arc::clone(&executed),
    });

    let total_requests = 48usize;
    let mut handles = Vec::with_capacity(total_requests);
    for idx in 0..total_requests {
        let mut svc = service.clone();
        handles.push(tokio::spawn(async move {
            let ready = tokio::time::timeout(Duration::from_secs(2), svc.ready())
                .await
                .expect("readiness timeout")
                .expect("service should become ready");
            tokio::time::timeout(
                Duration::from_secs(4),
                ready.call(TestRequest {
                    tenant: (idx % 6 + 1) as u64,
                    payload: "ok",
                }),
            )
            .await
        }));
    }

    tokio::time::sleep(Duration::from_millis(25)).await;
    for idx in (0..handles.len()).step_by(4) {
        handles[idx].abort();
    }

    let mut aborted = 0usize;
    let mut completed = 0usize;
    for handle in handles {
        let joined = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("join timeout");
        match joined {
            Ok(call_result) => {
                let response = call_result
                    .expect("service call should not time out")
                    .expect("service call should succeed");
                assert_eq!(response, "ok");
                completed += 1;
            }
            Err(join_err) => {
                assert!(join_err.is_cancelled(), "unexpected join error: {join_err}");
                aborted += 1;
            }
        }
    }

    assert!(completed > 0, "at least one request should complete");
    assert!(
        aborted > 0,
        "at least one request should be client-cancelled"
    );
    assert!(
        max_active.load(Ordering::SeqCst) <= in_flight_limit,
        "handler concurrency must stay under in-flight limit"
    );

    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let in_flight_active = inspector.in_flight_active();
            let queue_len = scheduler.stats().queue_len_estimate;
            if in_flight_active == 0 && queue_len == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("scheduler should drain without deadlock or semaphore leak");

    scheduler.close();
    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert_eq!(
        completed + aborted,
        total_requests,
        "every spawned request should complete or be cancelled"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_start_order_respects_scheduler_under_contention() {
    let starts = Arc::new(Mutex::new(Vec::new()));

    let layer = Firq::new()
        .with_shards(1)
        .with_max_global(32)
        .with_max_per_tenant(32)
        .with_quantum(1)
        .with_in_flight_limit(1)
        .build(|req: &TestRequest| TenantKey::from(req.tenant));
    let scheduler = layer.scheduler().clone();

    let mut service = ServiceBuilder::new().layer(layer).service(OrderedService {
        starts: Arc::clone(&starts),
    });

    let fut_a1 = service.ready().await.expect("ready a1").call(TestRequest {
        tenant: 1,
        payload: "a1",
    });
    let fut_a2 = service.ready().await.expect("ready a2").call(TestRequest {
        tenant: 1,
        payload: "a2",
    });
    let fut_b1 = service.ready().await.expect("ready b1").call(TestRequest {
        tenant: 2,
        payload: "b1",
    });
    let fut_b2 = service.ready().await.expect("ready b2").call(TestRequest {
        tenant: 2,
        payload: "b2",
    });

    let (r1, r2, r3, r4) = tokio::join!(fut_a1, fut_a2, fut_b1, fut_b2);
    assert!(r1.is_ok());
    assert!(r2.is_ok());
    assert!(r3.is_ok());
    assert!(r4.is_ok());

    let order = starts.lock().expect("start-order mutex poisoned").clone();
    assert_eq!(order, vec!["a1", "b1", "a2", "b2"]);

    scheduler.close();
}
