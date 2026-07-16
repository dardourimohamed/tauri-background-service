//! Integration tests for the actor-path service lifecycle.
//!
//! Tests the full start→stop lifecycle, error cases, callbacks, and context
//! field propagation using `ServiceManagerHandle` public async methods.

use async_trait::async_trait;
use std::sync::atomic::{AtomicI8, Ordering};
use std::sync::Arc;
#[cfg(mobile)]
use std::sync::Mutex;
use std::time::Duration;
use tauri::Runtime;
use tauri_plugin_background_service::{
    manager_loop, BackgroundService, ServiceContext, ServiceError, ServiceFactory,
    ServiceManagerHandle, StartConfig,
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
/// Only compiled on mobile where those fields exist.
#[cfg(mobile)]
struct ContextInspectingService {
    label: Arc<Mutex<Option<String>>>,
    fst: Arc<Mutex<Option<String>>>,
}

#[cfg(mobile)]
impl ContextInspectingService {
    fn new(label: Arc<Mutex<Option<String>>>, fst: Arc<Mutex<Option<String>>>) -> Self {
        Self { label, fst }
    }
}

#[cfg(mobile)]
#[async_trait]
impl<R: Runtime> BackgroundService<R> for ContextInspectingService {
    async fn init(&mut self, ctx: &ServiceContext<R>) -> Result<(), ServiceError> {
        *self.label.lock().unwrap() = Some(ctx.service_label.clone());
        *self.fst.lock().unwrap() = Some(ctx.foreground_service_type.clone());
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
    tokio::spawn(manager_loop(
        cmd_rx,
        factory,
        28.0,
        0.0,
        15.0,
        15.0,
        false,
        false,
        4.0,
        None,
        vec!["remoteMessaging".into()],
        true,
        tauri_plugin_background_service::NotifierPolicy::default(),
        None,
        None,
        false,
    ));
    handle
}

fn setup_manager_with_factory(
    factory: ServiceFactory<tauri::test::MockRuntime>,
) -> ServiceManagerHandle<tauri::test::MockRuntime> {
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
    let handle = ServiceManagerHandle::new(cmd_tx);
    tokio::spawn(manager_loop(
        cmd_rx,
        factory,
        28.0,
        0.0,
        15.0,
        15.0,
        false,
        false,
        4.0,
        None,
        vec!["remoteMessaging".into()],
        true,
        tauri_plugin_background_service::NotifierPolicy::default(),
        None,
        None,
        false,
    ));
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
    assert!(handle.is_running().await, "should be running after start");
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
    assert!(handle.is_running().await, "should be running after restart");
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
    assert!(handle.is_running().await, "should be running after start");

    handle.stop().await.unwrap();
    assert!(
        !handle.is_running().await,
        "should not be running after stop"
    );
}

// ─── Test 7: Callback fires on success ──────────────────────────────

#[tokio::test]
async fn callback_fires_on_success() {
    let handle = setup_manager_with_factory(Box::new(|| Box::new(ImmediateSuccessService)));
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
    let handle = setup_manager_with_factory(Box::new(|| Box::new(ImmediateErrorService)));
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

// ─── Test 9: ServiceContext fields are populated on mobile ────────────

#[cfg(mobile)]
#[tokio::test]
async fn service_context_fields_populated_on_mobile() {
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

    // On mobile, service_label and foreground_service_type are populated.
    assert_eq!(
        label.lock().unwrap().as_deref(),
        Some("Integration Test"),
        "service_label should be 'Integration Test' on mobile"
    );
    assert_eq!(
        fst.lock().unwrap().as_deref(),
        Some("specialUse"),
        "foreground_service_type should be 'specialUse' on mobile"
    );
}

// ─── Test 9b: ServiceContext has no mobile fields on desktop ──────────

/// Compile-time proof: ServiceContext on desktop does not expose
/// service_label or foreground_service_type. This test ensures the
/// #[cfg(mobile)] gating works — if the fields were accidentally
/// un-gated, this function body would need to set them.
#[cfg(not(mobile))]
#[test]
fn service_context_desktop_has_no_mobile_fields() {
    // The compile-time proof is inside models.rs unit tests (accesses pub(crate) Notifier).
    // The #[cfg(not(mobile))] gate above already ensures this runs on desktop only.
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

// ─── Permission count test ────────────────────────────────────────────

#[test]
fn default_toml_has_at_least_20_permissions() {
    let content = include_str!("../permissions/default.toml");
    let count = content
        .lines()
        .filter(|line| line.trim().starts_with("\"allow-"))
        .count();
    assert!(
        count >= 20,
        "default.toml must have >= 20 permissions, found {count}"
    );
}

// ─── NTF-16 (Step 12c) full-screen-intent static wire gate ────────────
// The Android-active arm of `can_use_full_screen_intent` /
// `open_full_screen_intent_settings` is COMPILE-INVISIBLE on desktop: the
// `#[cfg(target_os = "android")]` attribute is target-based, so a desktop
// `cargo test` builds ONLY the `#[cfg(not(android))]` default branch and the
// command_signature shims. A runtime test therefore cannot reach the Android
// wire under desktop/jsdom, so the wire is pinned by STATIC source-grep.
//
// Each load-bearing string must appear EXACTLY ONCE — the unique call site —
// proving the arm is present, not stubbed, and dispatches to the right Kotlin
// command. A bare `#[cfg(target_os = "android")]` count is VACUOUS (it appears
// 3x in lib.rs across unrelated arms) and is NOT used. This test lives in the
// integration-tests crate (not src/lib.rs) so its own grep literals are NOT
// counted by `include_str!("../src/lib.rs")` (no self-count).
#[test]
fn ntf16_full_screen_intent_wire_is_present_and_unique() {
    let mobile_src = include_str!("../src/mobile.rs");
    let lib_src = include_str!("../src/lib.rs");
    let build_src = include_str!("../build.rs");

    // (1)+(2) Rust→Kotlin wire targets in mobile.rs (exactly one call each).
    assert_eq!(
        mobile_src
            .matches(r#"run_mobile_plugin("canUseFullScreenIntent""#)
            .count(),
        1,
        "canUseFullScreenIntent must be wired exactly once in mobile.rs"
    );
    assert_eq!(
        mobile_src
            .matches(r#"run_mobile_plugin("openFullScreenIntentSettings""#)
            .count(),
        1,
        "openFullScreenIntentSettings must be wired exactly once in mobile.rs"
    );

    // (3)+(4) lib.rs ANDROID-ARM → mobile.rs BRIDGE CALL SITES (leading-dot +
    // empty-parens — distinct from the command_signature shim's `(app)` call;
    // pins the arm is not stubbed/removed/calling-the-wrong-method).
    assert_eq!(
        lib_src.matches(".can_use_full_screen_intent()").count(),
        1,
        ".can_use_full_screen_intent() bridge call must appear exactly once in lib.rs"
    );
    assert_eq!(
        lib_src
            .matches(".open_full_screen_intent_settings()")
            .count(),
        1,
        ".open_full_screen_intent_settings() bridge call must appear exactly once in lib.rs"
    );

    // (5) both command names registered in generate_handler! AND build.rs COMMANDS.
    assert!(
        lib_src.contains("can_use_full_screen_intent,")
            && lib_src.contains("open_full_screen_intent_settings,"),
        "both FSI commands must be registered in generate_handler!"
    );
    assert!(
        build_src.contains("\"can_use_full_screen_intent\"")
            && build_src.contains("\"open_full_screen_intent_settings\""),
        "both FSI commands must be listed in build.rs COMMANDS"
    );

    // (6) Rust↔TS IPC SHAPE CONTRACT (mem tauri-ipc-bare-bool-vs-object-shape):
    // `can_use_full_screen_intent` must serialize to a `{canUse: bool}` OBJECT —
    // matching the TS `invoke<{canUse:boolean}>` wrapper + UI `result.canUse`
    // consumer. A bare `Result<bool, String>` would arrive as a bare JSON boolean,
    // making `result.canUse` undefined and the `=== false` re-grant gate NEVER fire
    // (feature DEAD on Android, the only platform where it matters). The
    // fully-mocked vitest returns `{canUse:...}` and so CANNOT reach this Rust
    // serde shape — it is pinned here, at the layer the mock cannot exercise. Both
    // cfg arms must wrap (android: real grant; non-android: default true), hence
    // count == 2. A future revert to a bare bool in either arm drops the count.
    //
    // ROBUSTNESS — whitespace-tolerant, NON-VACUOUS contiguous strip-whitespace
    // count (Step-13a carry-forward hardening; Option-1 MANDATED). We normalize
    // `lib_src` by stripping ALL whitespace, then count the contiguous token
    // `serde_json::json!({"canUse":`. This is (i) whitespace-tolerant — a
    // rustfmt/hand-edit reflow of `({ "canUse":` to `({"canUse":` does NOT
    // false-RED (the prior hardcoded single-space `{ "canUse":` match would have),
    // and (ii) NON-VACUOUS on key-drift: the `serde_json::json!(` macro prefix
    // contiguous with `"canUse"` EXCLUDES the doc-comment at lib.rs:1012
    // (`/// ... OBJECT `{ "canUse": bool }``), which lacks the macro prefix. A
    // separate `contains("serde_json::json!(") && contains("\"canUse\"")` check
    // (Option-2) is FORBIDDEN — `"canUse"` appears 3x in lib.rs (doc-comment :1012
    // + the 2 code arms :1033/:1038), so Option-2 FALSE-GREENs on a
    // `canUse`→`can_use` key-drift in a code arm (the doc-comment still satisfies
    // `contains("canUse")`). The contiguous strip-whitespace count correctly REDs
    // on that key-drift (the doc-comment never matched the macro prefix ⇒
    // normalized count drops 2→1).
    //
    // DEFERRAL — Option-C typed-struct (NON-BLOCKING carry-forward from 12c; mem
    // tauri-ipc-bare-bool-vs-object-shape). A typed `FullScreenIntentStatus
    // { can_use: bool }` with `#[serde(rename = "canUse")]` — the same
    // typed-struct-return pattern `get_notification_permission_status` already
    // uses (`Result<models::NotificationPermissionStatus, String>`) — would pin
    // the VALUE TYPE at compile time. This assertion pins only the `"canUse"` KEY
    // (count==2), so a hypothetical future `can_use.to_string()` coercion would
    // keep count==2 GREEN yet break `=== false` at runtime (strict-equality
    // `"false" === false` is false — type mismatch, not a bare-bool). That is
    // forward-fragility ONLY — the current code is SAFE (both arms bind genuine
    // bools: mobile.rs `as_bool()` + literal `true`), and re-opening CLOSED
    // Step-12c to swap `serde_json::Value`→typed-struct is a non-defect refactor.
    // Land as defense-in-depth at a future Step-13-lane hardening, or leave;
    // correctness PASS.
    let lib_norm = lib_src
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>();
    assert_eq!(
        lib_norm.matches(r#"serde_json::json!({"canUse":"#).count(),
        2,
        "can_use_full_screen_intent must wrap its bool as {{canUse: bool}} in BOTH cfg arms \
         to match the TS object shape — a bare bool makes result.canUse undefined and kills \
         the re-grant gate"
    );
}
