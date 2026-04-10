//! Shared test helpers for desktop IPC tests.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::desktop::ipc::{IpcEvent, IpcMessage};
use crate::desktop::ipc_server::IpcServer;
use crate::error::ServiceError;
use crate::manager::{manager_loop, ServiceFactory};
use crate::models::ServiceContext;
use crate::service_trait::BackgroundService;

static TEST_ID: AtomicU64 = AtomicU64::new(0);

/// Generate a unique socket path for testing.
pub fn unique_socket_path() -> PathBuf {
    let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("ipc-test-{}-{id}.sock", std::process::id()))
}

/// Service that blocks in `run()` until cancelled.
pub struct BlockingService;

#[async_trait]
impl BackgroundService<tauri::test::MockRuntime> for BlockingService {
    async fn init(
        &mut self,
        _ctx: &ServiceContext<tauri::test::MockRuntime>,
    ) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn run(
        &mut self,
        ctx: &ServiceContext<tauri::test::MockRuntime>,
    ) -> Result<(), ServiceError> {
        ctx.shutdown.cancelled().await;
        Ok(())
    }
}

/// Service that completes immediately.
pub struct ImmediateSuccessService;

#[async_trait]
impl BackgroundService<tauri::test::MockRuntime> for ImmediateSuccessService {
    async fn init(
        &mut self,
        _ctx: &ServiceContext<tauri::test::MockRuntime>,
    ) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn run(
        &mut self,
        _ctx: &ServiceContext<tauri::test::MockRuntime>,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
}

/// Set up an IPC server with a custom service factory.
///
/// Returns `(socket_path, shutdown_token, event_sender)`.
///
/// The `event_sender` can be used to simulate the event relay from
/// `headless.rs` by sending `IpcEvent`s to the broadcast channel.
pub fn setup_server_with_factory(
    factory: ServiceFactory<tauri::test::MockRuntime>,
) -> (PathBuf, CancellationToken, broadcast::Sender<IpcEvent>) {
    let path = unique_socket_path();
    let app = tauri::test::mock_app();
    let (cmd_tx, cmd_rx) = mpsc::channel(16);
    tokio::spawn(manager_loop(
        cmd_rx, factory, 0.0, 0.0, 0.0, 0.0, false, false,
    ));
    let server = IpcServer::bind(path.clone(), cmd_tx, app.handle().clone()).unwrap();
    let event_tx = server.event_sender();
    let shutdown = CancellationToken::new();
    let s = shutdown.clone();
    tokio::spawn(async move { server.run(s).await });
    (path, shutdown, event_tx)
}

/// Set up an IPC server with the default [`BlockingService`].
///
/// Returns `(socket_path, shutdown_token, event_sender)`.
pub fn setup_server() -> (PathBuf, CancellationToken, broadcast::Sender<IpcEvent>) {
    setup_server_with_factory(Box::new(|| Box::new(BlockingService)))
}

/// Set up an IPC server without spawning `run()`, for tests that need manual
/// lifecycle control.
///
/// Returns `(server, socket_path, shutdown_token)`.
pub fn setup_server_raw(
    factory: ServiceFactory<tauri::test::MockRuntime>,
) -> (
    IpcServer<tauri::test::MockRuntime>,
    PathBuf,
    CancellationToken,
) {
    let path = unique_socket_path();
    let app = tauri::test::mock_app();
    let (cmd_tx, cmd_rx) = mpsc::channel(16);
    tokio::spawn(manager_loop(
        cmd_rx, factory, 0.0, 0.0, 0.0, 0.0, false, false,
    ));
    let server = IpcServer::bind(path.clone(), cmd_tx, app.handle().clone()).unwrap();
    let shutdown = CancellationToken::new();
    (server, path, shutdown)
}

/// Connect a raw [`UnixStream`] to the given socket path.
pub async fn connect(path: &PathBuf) -> tokio::net::UnixStream {
    tokio::net::UnixStream::connect(path).await.unwrap()
}

/// Send an [`IpcRequest`] over a raw stream, wrapped in [`IpcMessage::Request`].
pub async fn send_request(
    stream: &mut tokio::net::UnixStream,
    request: &crate::desktop::ipc::IpcRequest,
) {
    use tokio::io::AsyncWriteExt;
    let msg = IpcMessage::Request(request.clone());
    let frame = crate::desktop::ipc::encode_frame(&msg).unwrap();
    stream.write_all(&frame).await.unwrap();
}

/// Read an [`IpcResponse`] from a raw stream (expects [`IpcMessage::Response`] on the wire).
pub async fn read_response(
    stream: &mut tokio::net::UnixStream,
) -> crate::desktop::ipc::IpcResponse {
    use tokio::io::AsyncReadExt;
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await.unwrap();
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await.unwrap();
    match serde_json::from_slice::<IpcMessage>(&payload).unwrap() {
        IpcMessage::Response(resp) => resp,
        other => panic!("Expected IpcMessage::Response, got {other:?}"),
    }
}

/// Read an [`IpcEvent`] from a raw stream (expects [`IpcMessage::Event`] on the wire).
pub async fn read_event(stream: &mut tokio::net::UnixStream) -> crate::desktop::ipc::IpcEvent {
    use tokio::io::AsyncReadExt;
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await.unwrap();
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await.unwrap();
    match serde_json::from_slice::<IpcMessage>(&payload).unwrap() {
        IpcMessage::Event(event) => event,
        other => panic!("Expected IpcMessage::Event, got {other:?}"),
    }
}
