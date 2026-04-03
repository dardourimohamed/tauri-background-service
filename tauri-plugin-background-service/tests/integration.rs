//! Integration tests for the actor-path service lifecycle.
//!
//! Tests the full start→stop lifecycle, error cases, callbacks, and context
//! field propagation using `ServiceManagerHandle` public async methods.

use async_trait::async_trait;
use std::sync::atomic::{AtomicI8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::Runtime;
use tauri_plugin_background_service::{
    BackgroundService, ServiceContext, ServiceError, ServiceFactory,
    ServiceManagerHandle, StartConfig, manager_loop,
};

// ─── Test Services ─────────────────────────────────────────────────────

/// Service that blocks in run() until cancelled.
struct BlockingService;

#[async_trait]
impl<R: Runtime> BackgroundService<R> for BlockingService {
    async fn init(&mut self, _ctx: &ServiceContext<R>) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn run(&mut self, ctx: &ServiceContext<R>) -> Result<(), ServiceError> {
        ctx.shutdown.cancelled().await;
        Ok(())
    }
}

/// Service that completes run() immediately with Ok.
struct ImmediateSuccessService;

#[async_trait]
impl<R: Runtime> BackgroundService<R> for ImmediateSuccessService {
    async fn init(&mut self, _ctx: &ServiceContext<R>) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn run(&mut self, _ctx: &ServiceContext<R>) -> Result<(), ServiceError> {
        Ok(())
    }
}

/// Service that completes run() immediately with Err.
struct ImmediateErrorService;

#[async_trait]
impl<R: Runtime> BackgroundService<R> for ImmediateErrorService {
    async fn init(&mut self, _ctx: &ServiceContext<R>) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn run(&mut self, _ctx: &ServiceContext<R>) -> Result<(), ServiceError> {
        Err(ServiceError::Runtime("test error".into()))
    }
}

/// Service that captures ServiceContext fields for inspection.
struct ContextInspectingService {
    label: Arc<Mutex<Option<String>>>,
    fst: Arc<Mutex<Option<String>>>,
}

impl ContextInspectingService {
    fn new(
        label: Arc<Mutex<Option<String>>>,
        fst: Arc<Mutex<Option<String>>>,
    ) -> Self {
        Self { label, fst }
    }
}

#[async_trait]
impl<R: Runtime> BackgroundService<R> for ContextInspectingService {
    async fn init(&mut self, ctx: &ServiceContext<R>) -> Result<(), ServiceError> {
        *self.label.lock().unwrap() = ctx.service_label.clone();
        *self.fst.lock().unwrap() = ctx.foreground_service_type.clone();
        Ok(())
    }

    async fn run(&mut self, _ctx: &ServiceContext<R>) -> Result<(), ServiceError> {
        Ok(())
    }
}

// ─── Helpers ───────────────────────────────────────────────────────────

fn setup_manager() -> ServiceManagerHandle<tauri::test::MockRuntime> {
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
    let handle = ServiceManagerHandle::new(cmd_tx);
    let factory: ServiceFactory<tauri::test::MockRuntime> = Box::new(|| Box::new(BlockingService));
    tokio::spawn(manager_loop(cmd_rx, factory, 28.0));
    handle
}

fn setup_manager_with_factory(
    factory: ServiceFactory<tauri::test::MockRuntime>,
) -> ServiceManagerHandle<tauri::test::MockRuntime> {
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
    let handle = ServiceManagerHandle::new(cmd_tx);
    tokio::spawn(manager_loop(cmd_rx, factory, 28.0));
    handle
}

/// Wait for the service to finish (is_running becomes false).
async fn wait_until_stopped(
    handle: &ServiceManagerHandle<tauri::test::MockRuntime>,
    timeout_ms: u64,
) {
    let start = std::time::Instant::now();
    while start.elapsed().as_millis() < timeout_ms as u128 {
        if !handle.is_running().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("Service did not stop within {timeout_ms}ms");
}

// ─── Test 1: Start from idle succeeds ────────────────────────────────

#[tokio::test]
async fn start_from_idle_succeeds() {
    let handle = setup_manager();
    let app = tauri::test::mock_app();

    let result = handle
        .start(app.handle().clone(), StartConfig::default())
        .await;

    assert!(result.is_ok(), "start should succeed from idle");
    assert!(
        handle.is_running().await,
        "should be running after start"
    );
}

// ─── Test 2: Stop from running succeeds ──────────────────────────────

#[tokio::test]
async fn stop_from_running_succeeds() {
    let handle = setup_manager();
    let app = tauri::test::mock_app();

    handle
        .start(app.handle().clone(), StartConfig::default())
        .await
        .unwrap();

    let result = handle.stop().await;

    assert!(result.is_ok(), "stop should succeed from running");
    assert!(
        !handle.is_running().await,
        "should not be running after stop"
    );
}

// ─── Test 3: Double start returns AlreadyRunning ────────────────────

#[tokio::test]
async fn double_start_returns_already_running() {
    let handle = setup_manager();
    let app = tauri::test::mock_app();

    handle
        .start(app.handle().clone(), StartConfig::default())
        .await
        .unwrap();

    let result = handle
        .start(app.handle().clone(), StartConfig::default())
        .await;

    assert!(
        matches!(result, Err(ServiceError::AlreadyRunning)),
        "second start should return AlreadyRunning"
    );
}

// ─── Test 4: Stop when not running returns NotRunning ───────────────

#[tokio::test]
async fn stop_when_not_running_returns_not_running() {
    let handle = setup_manager();

    let result = handle.stop().await;

    assert!(
        matches!(result, Err(ServiceError::NotRunning)),
        "stop should return NotRunning when idle"
    );
}

// ─── Test 5: Start-stop-restart cycle ─────────────────────────────────

#[tokio::test]
async fn start_stop_restart_cycle() {
    let handle = setup_manager();
    let app = tauri::test::mock_app();

    // Start
    handle
        .start(app.handle().clone(), StartConfig::default())
        .await
        .unwrap();
    assert!(handle.is_running().await);

    // Stop
    handle.stop().await.unwrap();
    assert!(!handle.is_running().await);

    // Restart
    let result = handle
        .start(app.handle().clone(), StartConfig::default())
        .await;

    assert!(result.is_ok(), "restart should succeed after stop");
    assert!(
        handle.is_running().await,
        "should be running after restart"
    );
}

// ─── Test 6: is_running reports correct state ────────────────────────

#[tokio::test]
async fn is_running_reports_correct_state() {
    let handle = setup_manager();
    let app = tauri::test::mock_app();

    assert!(
        !handle.is_running().await,
        "should not be running initially"
    );

    handle
        .start(app.handle().clone(), StartConfig::default())
        .await
        .unwrap();
    assert!(
        handle.is_running().await,
        "should be running after start"
    );

    handle.stop().await.unwrap();
    assert!(
        !handle.is_running().await,
        "should not be running after stop"
    );
}

// ─── Test 7: Callback fires on success ──────────────────────────────

#[tokio::test]
async fn callback_fires_on_success() {
    let handle =
        setup_manager_with_factory(Box::new(|| Box::new(ImmediateSuccessService)));
    let app = tauri::test::mock_app();

    let called = Arc::new(AtomicI8::new(-1));
    let called_clone = called.clone();
    handle
        .set_on_complete(Box::new(move |success| {
            called_clone.store(if success { 1 } else { 0 }, Ordering::Release);
        }))
        .await;

    handle
        .start(app.handle().clone(), StartConfig::default())
        .await
        .unwrap();
    wait_until_stopped(&handle, 1000).await;

    assert_eq!(
        called.load(Ordering::Acquire),
        1,
        "callback should be called with true"
    );
}

// ─── Test 8: Callback fires on error ────────────────────────────────

#[tokio::test]
async fn callback_fires_on_error() {
    let handle =
        setup_manager_with_factory(Box::new(|| Box::new(ImmediateErrorService)));
    let app = tauri::test::mock_app();

    let called = Arc::new(AtomicI8::new(-1));
    let called_clone = called.clone();
    handle
        .set_on_complete(Box::new(move |success| {
            called_clone.store(if success { 1 } else { 0 }, Ordering::Release);
        }))
        .await;

    handle
        .start(app.handle().clone(), StartConfig::default())
        .await
        .unwrap();
    wait_until_stopped(&handle, 1000).await;

    assert_eq!(
        called.load(Ordering::Acquire),
        0,
        "callback should be called with false on error"
    );
}

// ─── Test 9: ServiceContext fields populated ─────────────────────────

#[tokio::test]
async fn service_context_fields_populated() {
    let label = Arc::new(Mutex::new(None::<String>));
    let fst = Arc::new(Mutex::new(None::<String>));
    let label_clone = label.clone();
    let fst_clone = fst.clone();

    let handle = setup_manager_with_factory(Box::new(move || {
        let l = label_clone.clone();
        let f = fst_clone.clone();
        Box::new(ContextInspectingService::new(l, f))
    }));
    let app = tauri::test::mock_app();

    let config = StartConfig {
        service_label: "Integration Test".into(),
        foreground_service_type: "specialUse".into(),
    };

    handle.start(app.handle().clone(), config).await.unwrap();
    wait_until_stopped(&handle, 1000).await;

    assert_eq!(
        label.lock().unwrap().as_deref(),
        Some("Integration Test"),
        "service_label should be populated from StartConfig"
    );
    assert_eq!(
        fst.lock().unwrap().as_deref(),
        Some("specialUse"),
        "foreground_service_type should be populated from StartConfig"
    );
}

// ─── Test 10: Trait implementation compiles ───────────────────────────

#[test]
fn trait_implementation_compiles() {
    // Compile-time proof: BlockingService implements BackgroundService<R>
    // for any Runtime (both Wry and MockRuntime).
    fn assert_impl<R: Runtime>()
    where
        BlockingService: BackgroundService<R>,
    {
    }
    assert_impl::<tauri::Wry>();
    assert_impl::<tauri::test::MockRuntime>();
}
