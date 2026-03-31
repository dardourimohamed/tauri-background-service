//! Integration tests for the ServiceRunner lifecycle.
//!
//! Tests the full start→stop lifecycle, error cases, and restart using
//! `tauri::test::MockRuntime` to provide an AppHandle without a full Tauri app.

use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::atomic::AtomicU8;
use std::sync::Arc;
use std::time::Duration;
use tauri::Runtime;
use tauri_plugin_background_service::{
    BackgroundService, ServiceContext, ServiceError, ServiceRunner, StartConfig,
};

// ─── Test Service ─────────────────────────────────────────────────────

/// Minimal service that tracks whether init was called and waits for
/// cancellation in `run()`.
struct TestService {
    init_called: Arc<AtomicBool>,
}

impl TestService {
    fn new() -> Self {
        Self {
            init_called: Arc::new(AtomicBool::new(false)),
        }
    }

    fn new_tracked() -> (Self, Arc<AtomicBool>) {
        let flag = Arc::new(AtomicBool::new(false));
        let service = Self {
            init_called: flag.clone(),
        };
        (service, flag)
    }
}

#[async_trait]
impl<R: Runtime> BackgroundService<R> for TestService {
    async fn init(&mut self, _ctx: &ServiceContext<R>) -> Result<(), ServiceError> {
        self.init_called.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn run(&mut self, ctx: &ServiceContext<R>) -> Result<(), ServiceError> {
        // Cooperatively wait for cancellation so stop() can clean up.
        ctx.shutdown.cancelled().await;
        Ok(())
    }
}

// ─── AC1: Trait Implementation Compiles ───────────────────────────────

#[test]
fn trait_implementation_compiles() {
    // Compile-time proof: TestService implements BackgroundService<R>
    // for any Runtime (both Wry and MockRuntime).
    fn assert_impl<R: Runtime>()
    where
        TestService: BackgroundService<R>,
    {
    }
    assert_impl::<tauri::Wry>();
    assert_impl::<tauri::test::MockRuntime>();
}

// ─── AC2: Start-Stop Lifecycle ────────────────────────────────────────

#[tokio::test]
async fn start_stop_lifecycle() {
    let app = tauri::test::mock_app();
    let runner = ServiceRunner::new();
    let service = TestService::new();

    assert!(!runner.is_running(), "should not be running initially");

    runner
        .start(app.handle().clone(), service, StartConfig::default())
        .expect("start should succeed");

    assert!(runner.is_running(), "should be running after start");

    runner.stop().expect("stop should succeed");

    assert!(
        !runner.is_running(),
        "should not be running after stop"
    );
}

// ─── AC2 extended: Init is called on start ─────────────────────────────

#[tokio::test]
async fn start_calls_init() {
    let app = tauri::test::mock_app();
    let runner = ServiceRunner::new();
    let (service, init_flag) = TestService::new_tracked();

    assert!(!init_flag.load(Ordering::SeqCst), "init should not be called yet");

    runner
        .start(app.handle().clone(), service, StartConfig::default())
        .expect("start should succeed");

    // Give the spawned task time to call init()
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(
        init_flag.load(Ordering::SeqCst),
        "init should have been called"
    );

    runner.stop().expect("cleanup");
}

// ─── AC3: Double-start returns AlreadyRunning ─────────────────────────

#[tokio::test]
async fn double_start_returns_already_running() {
    let app = tauri::test::mock_app();
    let runner = ServiceRunner::new();
    let service1 = TestService::new();
    let service2 = TestService::new();

    runner
        .start(app.handle().clone(), service1, StartConfig::default())
        .expect("first start should succeed");

    let result = runner.start(app.handle().clone(), service2, StartConfig::default());

    assert!(
        matches!(result, Err(ServiceError::AlreadyRunning)),
        "second start should return AlreadyRunning"
    );

    // Clean up
    runner.stop().expect("cleanup");
}

// ─── AC3: Stop when not running returns NotRunning ─────────────────────

#[test]
fn stop_when_not_running_returns_not_running() {
    let runner = ServiceRunner::new();

    let result = runner.stop();

    assert!(
        matches!(result, Err(ServiceError::NotRunning)),
        "stop on idle runner should return NotRunning"
    );
}

// ─── AC2 extended: Restart after stop ─────────────────────────────────

#[tokio::test]
async fn restart_after_stop() {
    let app = tauri::test::mock_app();
    let runner = ServiceRunner::new();

    // First start
    let service1 = TestService::new();
    runner
        .start(app.handle().clone(), service1, StartConfig::default())
        .expect("first start should succeed");
    assert!(runner.is_running(), "should be running after first start");

    // Stop
    runner.stop().expect("stop should succeed");
    assert!(!runner.is_running(), "should not be running after stop");

    // Allow spawned task cleanup to finish
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Restart with a new service instance
    let service2 = TestService::new();
    runner
        .start(app.handle().clone(), service2, StartConfig::default())
        .expect("restart should succeed");
    assert!(
        runner.is_running(),
        "should be running after restart"
    );

    // Clean up
    runner.stop().expect("final cleanup");
}

// ─── Callback Test Services ────────────────────────────────────────────

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

/// Service that fails during init() - used to test init failure callback behavior.
struct FailingInitService;

#[async_trait]
impl<R: Runtime> BackgroundService<R> for FailingInitService {
    async fn init(&mut self, _ctx: &ServiceContext<R>) -> Result<(), ServiceError> {
        Err(ServiceError::Runtime("Init failed".into()))
    }

    async fn run(&mut self, _ctx: &ServiceContext<R>) -> Result<(), ServiceError> {
        unreachable!("run should not be called after init failure")
    }
}

/// Service that waits for cancellation, tracking how many times run() completed.
struct TrackedCallbackService {
    run_started: Arc<AtomicBool>,
}

impl TrackedCallbackService {
    fn new() -> (Self, Arc<AtomicBool>) {
        let flag = Arc::new(AtomicBool::new(false));
        let service = Self {
            run_started: flag.clone(),
        };
        (service, flag)
    }
}

#[async_trait]
impl<R: Runtime> BackgroundService<R> for TrackedCallbackService {
    async fn init(&mut self, _ctx: &ServiceContext<R>) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn run(&mut self, ctx: &ServiceContext<R>) -> Result<(), ServiceError> {
        self.run_started.store(true, Ordering::SeqCst);
        ctx.shutdown.cancelled().await;
        Ok(())
    }
}

// ─── Callback Tests ────────────────────────────────────────────────────

#[tokio::test]
async fn on_complete_fires_on_run_success() {
    let app = tauri::test::mock_app();
    let runner = ServiceRunner::new();

    let success_val = Arc::new(AtomicU8::new(255)); // 255 = not called yet
    let success_clone = success_val.clone();
    runner.set_on_complete(Box::new(move |success| {
        success_clone.store(if success { 1 } else { 0 }, Ordering::SeqCst);
    }));

    runner
        .start(
            app.handle().clone(),
            ImmediateSuccessService,
            StartConfig::default(),
        )
        .expect("start should succeed");

    // Wait for the spawned task to complete
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        success_val.load(Ordering::SeqCst),
        1,
        "callback should be called with true on success"
    );
}

#[tokio::test]
async fn on_complete_fires_on_run_error() {
    let app = tauri::test::mock_app();
    let runner = ServiceRunner::new();

    let success_val = Arc::new(AtomicU8::new(255)); // 255 = not called yet
    let success_clone = success_val.clone();
    runner.set_on_complete(Box::new(move |success| {
        success_clone.store(if success { 1 } else { 0 }, Ordering::SeqCst);
    }));

    runner
        .start(
            app.handle().clone(),
            ImmediateErrorService,
            StartConfig::default(),
        )
        .expect("start should succeed");

    // Wait for the spawned task to complete
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        success_val.load(Ordering::SeqCst),
        0,
        "callback should be called with false on error"
    );
}

#[tokio::test]
async fn on_complete_none_no_panic() {
    let app = tauri::test::mock_app();
    let runner = ServiceRunner::new();
    // No set_on_complete call — desktop behavior

    runner
        .start(
            app.handle().clone(),
            ImmediateSuccessService,
            StartConfig::default(),
        )
        .expect("start should succeed");

    // Wait for the spawned task to complete without panicking
    tokio::time::sleep(Duration::from_millis(100)).await;

    // If we get here, no panic occurred
    assert!(!runner.is_running(), "should not be running after completion");
}

#[tokio::test]
async fn on_complete_fires_on_init_failure() {
    let app = tauri::test::mock_app();
    let runner = ServiceRunner::new();

    let callback_val = Arc::new(AtomicU8::new(255)); // 255 = not called yet
    let callback_clone = callback_val.clone();
    runner.set_on_complete(Box::new(move |success| {
        callback_clone.store(if success { 0 } else { 1 }, Ordering::SeqCst);
    }));

    runner
        .start(
            app.handle().clone(),
            FailingInitService,
            StartConfig::default(),
        )
        .expect("start should succeed");

    // Wait for the spawned task to complete
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        callback_val.load(Ordering::SeqCst),
        1,
        "callback should be called with false on init failure"
    );
}

#[tokio::test]
async fn on_complete_generation_guarded() {
    let app = tauri::test::mock_app();
    let runner = ServiceRunner::new();

    let callback_a_val = Arc::new(AtomicU8::new(0));
    let callback_b_val = Arc::new(AtomicU8::new(0));
    let callback_a_clone = callback_a_val.clone();
    let callback_b_clone = callback_b_val.clone();

    // Set callback A
    runner.set_on_complete(Box::new(move |_success| {
        callback_a_clone.fetch_add(1, Ordering::SeqCst);
    }));

    // Start service that waits for cancellation
    let (service, run_started) = TrackedCallbackService::new();
    runner
        .start(app.handle().clone(), service, StartConfig::default())
        .expect("start should succeed");

    // Wait for run() to start
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(run_started.load(Ordering::SeqCst), "run should have started");

    // Overwrite callback to B while service is still running
    runner.set_on_complete(Box::new(move |_success| {
        callback_b_clone.fetch_add(1, Ordering::SeqCst);
    }));

    // Stop the service — triggers completion
    runner.stop().expect("stop should succeed");
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The OLD task should have fired callback A (captured at spawn time)
    assert_eq!(
        callback_a_val.load(Ordering::SeqCst),
        1,
        "original callback A should have been called"
    );
    assert_eq!(
        callback_b_val.load(Ordering::SeqCst),
        0,
        "new callback B should NOT have been called by old task"
    );
}

#[tokio::test]
async fn init_failure_preserves_generation_guard() {
    let app = tauri::test::mock_app();
    let runner = ServiceRunner::new();

    // First start: service that fails in init
    let first_callback_val = Arc::new(AtomicU8::new(255)); // 255 = not called yet
    let first_clone = first_callback_val.clone();
    runner.set_on_complete(Box::new(move |success| {
        first_clone.store(if success { 1 } else { 0 }, Ordering::SeqCst);
    }));

    runner
        .start(
            app.handle().clone(),
            FailingInitService,
            StartConfig::default(),
        )
        .expect("first start should succeed");

    // Wait for first task to complete (init failure + cleanup)
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        first_callback_val.load(Ordering::SeqCst),
        0,
        "first callback should be called with false on init failure"
    );
    assert!(
        !runner.is_running(),
        "should not be running after init failure"
    );

    // Second start: service that succeeds — proves first task's cleanup
    // didn't clear the second task's token (generation guard works).
    let second_callback_val = Arc::new(AtomicU8::new(255)); // 255 = not called yet
    let second_clone = second_callback_val.clone();
    runner.set_on_complete(Box::new(move |success| {
        second_clone.store(if success { 1 } else { 0 }, Ordering::SeqCst);
    }));

    runner
        .start(
            app.handle().clone(),
            ImmediateSuccessService,
            StartConfig::default(),
        )
        .expect("second start should succeed");

    // Wait for second task to complete
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        second_callback_val.load(Ordering::SeqCst),
        1,
        "second callback should be called with true on success"
    );
    assert!(
        !runner.is_running(),
        "should not be running after both tasks complete"
    );
}
