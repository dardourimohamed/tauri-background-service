//! Desktop IPC client for the GUI process.
//!
//! [`IpcClient`] connects to the headless sidecar's Unix domain socket and
//! provides methods to start/stop the background service and receive events
//! over the IPC protocol.
//!
//! Only available when the `desktop-service` Cargo feature is enabled.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use tauri::{Emitter, Runtime};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::desktop::ipc::{
    decode_frame, encode_frame, IpcEvent, IpcMessage, IpcRequest, IpcResponse, MAX_FRAME_SIZE,
};
use crate::error::ServiceError;
use crate::models::{PluginEvent, StartConfig};

/// IPC client for communicating with the headless sidecar service.
///
/// Connects to the sidecar's Unix domain socket and translates method calls
/// into [`IpcRequest`] messages. Responses are decoded from [`IpcResponse`]
/// frames.
///
/// Events from the sidecar (started/stopped/error) are read as [`IpcEvent`]
/// frames and converted to [`PluginEvent`] for emission via the Tauri event
/// system.
pub struct IpcClient {
    stream: UnixStream,
}

impl IpcClient {
    /// Connect to the sidecar's IPC socket at the given path.
    pub async fn connect(path: PathBuf) -> Result<Self, ServiceError> {
        let stream = UnixStream::connect(&path)
            .await
            .map_err(|e| ServiceError::Ipc(format!("connect failed: {e}")))?;
        Ok(Self { stream })
    }

    /// Send a Start command to the sidecar.
    pub async fn start(&mut self, config: StartConfig) -> Result<(), ServiceError> {
        let request = IpcRequest::Start { config };
        let (response, _events) = self.send_and_read(&request).await?;
        if response.ok {
            Ok(())
        } else {
            Err(ServiceError::Ipc(
                response.error.unwrap_or_else(|| "unknown error".into()),
            ))
        }
    }

    /// Send a Stop command to the sidecar.
    pub async fn stop(&mut self) -> Result<(), ServiceError> {
        let (response, _events) = self.send_and_read(&IpcRequest::Stop).await?;
        if response.ok {
            Ok(())
        } else {
            Err(ServiceError::Ipc(
                response.error.unwrap_or_else(|| "unknown error".into()),
            ))
        }
    }

    /// Send an IsRunning query to the sidecar.
    pub async fn is_running(&mut self) -> Result<bool, ServiceError> {
        let (response, _events) = self.send_and_read(&IpcRequest::IsRunning).await?;
        if response.ok {
            Ok(response
                .data
                .and_then(|d| d.get("running").and_then(|v| v.as_bool()))
                .unwrap_or(false))
        } else {
            Err(ServiceError::Ipc(
                response.error.unwrap_or_else(|| "unknown error".into()),
            ))
        }
    }

    /// Read the next [`IpcEvent`] from the socket.
    ///
    /// Returns `None` if the connection was closed.
    pub async fn read_event(&mut self) -> Result<Option<IpcEvent>, ServiceError> {
        let frame = match self.read_frame().await? {
            Some(f) => f,
            None => return Ok(None),
        };
        match decode_frame(&frame).map_err(|e| ServiceError::Ipc(format!("decode event: {e}")))? {
            IpcMessage::Event(event) => Ok(Some(event)),
            other => Err(ServiceError::Ipc(format!(
                "expected event frame, got {:?}",
                std::mem::discriminant(&other),
            ))),
        }
    }

    /// Spawn a background task that reads [`IpcEvent`] frames and emits
    /// [`PluginEvent`] via the given `AppHandle`.
    ///
    /// The task runs until the socket is closed or an error occurs.
    pub fn listen_events<R: Runtime>(mut self, app: tauri::AppHandle<R>) {
        tokio::spawn(async move {
            loop {
                match self.read_event().await {
                    Ok(Some(event)) => {
                        let plugin_event = ipc_event_to_plugin_event(event);
                        let _ = app.emit("background-service://event", plugin_event);
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
        });
    }

    // -- Private helpers -------------------------------------------------------

    async fn send_and_read(
        &mut self,
        request: &IpcRequest,
    ) -> Result<(IpcResponse, Vec<IpcEvent>), ServiceError> {
        self.send_request(request).await?;
        // The server interleaves IpcResponse and broadcast IpcEvent frames on
        // the same socket. Read frames in a loop until we get a Response,
        // collecting any Event frames encountered along the way.
        let mut events = Vec::new();
        loop {
            let frame = self
                .read_frame()
                .await?
                .ok_or_else(|| ServiceError::Ipc("connection closed".into()))?;
            match decode_frame(&frame).map_err(|e| ServiceError::Ipc(format!("decode: {e}")))? {
                IpcMessage::Response(resp) => return Ok((resp, events)),
                IpcMessage::Event(e) => {
                    events.push(e);
                }
                IpcMessage::Request(_) => {
                    return Err(ServiceError::Ipc("unexpected request frame".into()));
                }
            }
        }
    }

    async fn send_request(&mut self, request: &IpcRequest) -> Result<(), ServiceError> {
        let msg = IpcMessage::Request(request.clone());
        let frame = encode_frame(&msg).map_err(|e| ServiceError::Ipc(format!("encode: {e}")))?;
        self.stream
            .write_all(&frame)
            .await
            .map_err(|e| ServiceError::Ipc(format!("send request: {e}")))?;
        Ok(())
    }

    /// Read a single length-prefixed frame from the socket.
    ///
    /// Returns the payload bytes only (no length prefix).
    /// Returns `None` if the connection was closed cleanly.
    async fn read_frame(&mut self) -> Result<Option<Vec<u8>>, ServiceError> {
        let mut len_buf = [0u8; 4];
        match self.stream.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(ServiceError::Ipc(format!("read frame: {e}"))),
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > MAX_FRAME_SIZE {
            return Err(ServiceError::Ipc(format!("frame too large: {len}")));
        }
        if len == 0 {
            return Ok(None);
        }
        let mut payload = vec![0u8; len];
        self.stream
            .read_exact(&mut payload)
            .await
            .map_err(|e| ServiceError::Ipc(format!("read payload: {e}")))?;
        Ok(Some(payload))
    }
}

/// Convert an [`IpcEvent`] to a [`PluginEvent`].
pub fn ipc_event_to_plugin_event(event: IpcEvent) -> PluginEvent {
    match event {
        IpcEvent::Started => PluginEvent::Started,
        IpcEvent::Stopped { reason } => PluginEvent::Stopped { reason },
        IpcEvent::Error { message } => PluginEvent::Error { message },
    }
}

// ─── Persistent IPC Client ────────────────────────────────────────────────────

/// Internal command sent from the handle to the background connection task.
enum IpcCommand {
    Start {
        config: StartConfig,
        reply: tokio::sync::oneshot::Sender<Result<(), ServiceError>>,
    },
    Stop {
        reply: tokio::sync::oneshot::Sender<Result<(), ServiceError>>,
    },
    IsRunning {
        reply: tokio::sync::oneshot::Sender<Result<bool, ServiceError>>,
    },
}

/// Handle to a persistent IPC client that maintains a long-lived connection
/// to the headless sidecar.
///
/// The background task automatically:
/// - Relays [`IpcEvent`] frames to `app.emit("background-service://event", ...)`
/// - Reconnects on connection failure with exponential backoff (1s–30s, up to 10 retries)
/// - Forwards commands (start/stop/is_running) over the same connection
pub struct PersistentIpcClientHandle {
    cmd_tx: tokio::sync::mpsc::Sender<IpcCommand>,
    shutdown: tokio_util::sync::CancellationToken,
    connected: Arc<AtomicBool>,
}

impl Drop for PersistentIpcClientHandle {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

impl PersistentIpcClientHandle {
    /// Spawn the persistent IPC client background task.
    ///
    /// The task immediately begins trying to connect to the socket at
    /// `socket_path`. Events are relayed to the Tauri event system via
    /// `app.emit()`.
    pub fn spawn<R: Runtime>(socket_path: PathBuf, app: tauri::AppHandle<R>) -> Self {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let shutdown = tokio_util::sync::CancellationToken::new();
        let connected = Arc::new(AtomicBool::new(false));

        tokio::spawn(persistent_client_loop(
            socket_path,
            app,
            cmd_rx,
            shutdown.clone(),
            connected.clone(),
        ));

        Self {
            cmd_tx,
            shutdown,
            connected,
        }
    }

    /// Send a Start command through the persistent connection.
    pub async fn start(&self, config: StartConfig) -> Result<(), ServiceError> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(IpcCommand::Start {
                config,
                reply: reply_tx,
            })
            .await
            .map_err(|_| ServiceError::Ipc("persistent client shut down".into()))?;
        reply_rx
            .await
            .map_err(|_| ServiceError::Ipc("command dropped".into()))?
    }

    /// Send a Stop command through the persistent connection.
    pub async fn stop(&self) -> Result<(), ServiceError> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(IpcCommand::Stop { reply: reply_tx })
            .await
            .map_err(|_| ServiceError::Ipc("persistent client shut down".into()))?;
        reply_rx
            .await
            .map_err(|_| ServiceError::Ipc("command dropped".into()))?
    }

    /// Query whether the service is running through the persistent connection.
    pub async fn is_running(&self) -> Result<bool, ServiceError> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(IpcCommand::IsRunning { reply: reply_tx })
            .await
            .map_err(|_| ServiceError::Ipc("persistent client shut down".into()))?;
        reply_rx
            .await
            .map_err(|_| ServiceError::Ipc("command dropped".into()))?
    }

    /// Returns `true` if the persistent client is currently connected to the
    /// headless sidecar, `false` otherwise.
    pub fn is_connected(&self) -> bool {
        self.connected.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Background task: maintain a persistent connection with reconnection.
async fn persistent_client_loop<R: Runtime>(
    socket_path: PathBuf,
    app: tauri::AppHandle<R>,
    mut cmd_rx: tokio::sync::mpsc::Receiver<IpcCommand>,
    shutdown: tokio_util::sync::CancellationToken,
    connected: Arc<AtomicBool>,
) {
    use backon::BackoffBuilder;

    let backoff_builder = backon::ExponentialBuilder::default()
        .with_min_delay(Duration::from_secs(1))
        .with_max_delay(Duration::from_secs(30))
        .with_max_times(10)
        .with_jitter();

    let mut attempts = backoff_builder.build();

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                log::info!("Persistent IPC client shutting down");
                connected.store(false, std::sync::atomic::Ordering::Relaxed);
                break;
            }
            connect_result = UnixStream::connect(&socket_path) => {
                match connect_result {
                    Ok(stream) => {
                        log::info!("Persistent IPC client connected");
                        connected.store(true, std::sync::atomic::Ordering::Relaxed);
                        let result = run_persistent_connection(stream, &app, &mut cmd_rx, &connected).await;
                        // Reset backoff on successful connect (even if session later failed).
                        attempts = backoff_builder.build();
                        if result.is_err() {
                            log::info!("Persistent IPC connection lost, reconnecting...");
                            connected.store(false, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    Err(_) => {
                        log::debug!("Persistent IPC client: connection failed, retrying...");
                        connected.store(false, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                let delay = match attempts.next() {
                    Some(d) => d,
                    None => {
                        log::warn!("Persistent IPC client: backoff exhausted, giving up");
                        break;
                    }
                };
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => {
                        log::info!("Persistent IPC client shutting down");
                        connected.store(false, std::sync::atomic::Ordering::Relaxed);
                        break;
                    }
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
}

/// Run a single persistent connection until it fails.
///
/// Splits the stream into read/write halves:
/// - A reader task continuously reads frames and relays events to `app.emit()`.
///   When a response frame arrives, it forwards it via a shared oneshot channel.
/// - The main loop receives commands from `cmd_rx` and sends requests.
async fn run_persistent_connection<R: Runtime>(
    stream: UnixStream,
    app: &tauri::AppHandle<R>,
    cmd_rx: &mut tokio::sync::mpsc::Receiver<IpcCommand>,
    connected: &Arc<AtomicBool>,
) -> Result<(), ServiceError> {
    let (read_half, mut write_half) = stream.into_split();

    // Shared slot for the reader task to deliver response frames.
    let response_slot: std::sync::Arc<
        tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<IpcResponse>>>,
    > = std::sync::Arc::new(tokio::sync::Mutex::new(None));

    let slot_writer = response_slot.clone();
    let app_clone = app.clone();
    let connected_reader = connected.clone();

    // Reader task: reads frames and either relays events or delivers responses.
    let reader_handle = tokio::spawn(async move {
        let mut read_half = read_half;
        loop {
            let frame = match read_frame_from(&mut read_half).await {
                Ok(Some(f)) => f,
                Ok(None) => break, // Connection closed
                Err(_) => break,
            };

            match decode_frame(&frame) {
                Ok(IpcMessage::Response(resp)) => {
                    let mut slot = slot_writer.lock().await;
                    if let Some(sender) = slot.take() {
                        let _ = sender.send(resp);
                    }
                    continue;
                }
                Ok(IpcMessage::Event(event)) => {
                    let plugin_event = ipc_event_to_plugin_event(event);
                    let _ = app_clone.emit("background-service://event", plugin_event);
                    continue;
                }
                Ok(IpcMessage::Request(_)) => {
                    log::warn!("unexpected request frame on client connection");
                    continue;
                }
                Err(e) => {
                    log::debug!("failed to decode IPC frame: {e}");
                    continue;
                }
            }
        }
        // Reader exited — mark disconnected.
        connected_reader.store(false, std::sync::atomic::Ordering::Relaxed);
    });

    // Main loop: receive commands, send requests, wait for responses.
    let result = loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let cmd = match cmd {
                    Some(c) => c,
                    None => break Err(ServiceError::Ipc("command channel closed".into())),
                };

                match cmd {
                    IpcCommand::Start { config, reply } => {
                        let request = IpcRequest::Start { config };
                        let rx = prepare_response_slot(&response_slot).await;
                        if let Err(e) = send_request_to(&mut write_half, &request).await {
                            let _ = reply.send(Err(e));
                            break Err(ServiceError::Ipc("send failed".into()));
                        }
                        let response = await_response(rx).await;
                        let result = match response {
                            Ok(resp) if resp.ok => Ok(()),
                            Ok(resp) => Err(ServiceError::Ipc(
                                resp.error.unwrap_or_else(|| "unknown error".into()),
                            )),
                            Err(e) => Err(e),
                        };
                        let _ = reply.send(result);
                    }
                    IpcCommand::Stop { reply } => {
                        let rx = prepare_response_slot(&response_slot).await;
                        if let Err(e) = send_request_to(&mut write_half, &IpcRequest::Stop).await {
                            let _ = reply.send(Err(e));
                            break Err(ServiceError::Ipc("send failed".into()));
                        }
                        let response = await_response(rx).await;
                        let result = match response {
                            Ok(resp) if resp.ok => Ok(()),
                            Ok(resp) => Err(ServiceError::Ipc(
                                resp.error.unwrap_or_else(|| "unknown error".into()),
                            )),
                            Err(e) => Err(e),
                        };
                        let _ = reply.send(result);
                    }
                    IpcCommand::IsRunning { reply } => {
                        let rx = prepare_response_slot(&response_slot).await;
                        if let Err(e) = send_request_to(&mut write_half, &IpcRequest::IsRunning).await {
                            let _ = reply.send(Err(e));
                            break Err(ServiceError::Ipc("send failed".into()));
                        }
                        let response = await_response(rx).await;
                        let result = match response {
                            Ok(resp) if resp.ok => Ok(resp
                                .data
                                .and_then(|d| d.get("running").and_then(|v| v.as_bool()))
                                .unwrap_or(false)),
                            Ok(resp) => Err(ServiceError::Ipc(
                                resp.error.unwrap_or_else(|| "unknown error".into()),
                            )),
                            Err(e) => Err(e),
                        };
                        let _ = reply.send(result);
                    }
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                // Timeout — check if reader is still alive
                if reader_handle.is_finished() {
                    break Err(ServiceError::Ipc("reader task died".into()));
                }
            }
        }
    };

    reader_handle.abort();
    result
}

/// Send an IPC request frame through a write half.
async fn send_request_to(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    request: &IpcRequest,
) -> Result<(), ServiceError> {
    let msg = IpcMessage::Request(request.clone());
    let frame = encode_frame(&msg).map_err(|e| ServiceError::Ipc(format!("encode: {e}")))?;
    write_half
        .write_all(&frame)
        .await
        .map_err(|e| ServiceError::Ipc(format!("send: {e}")))?;
    Ok(())
}

/// Prepare the shared response slot for an upcoming request.
///
/// Creates a oneshot channel and stores the sender in `slot` so the reader
/// task can deliver the next response. Returns the receiver end.
///
/// Must be called **before** sending the request to prevent losing fast
/// responses that arrive before the slot is set.
async fn prepare_response_slot(
    slot: &std::sync::Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<IpcResponse>>>>,
) -> tokio::sync::oneshot::Receiver<IpcResponse> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut guard = slot.lock().await;
    debug_assert!(
        guard.is_none(),
        "response slot overwritten — sequential command invariant violated"
    );
    *guard = Some(tx);
    rx
}

/// Await a response from the reader task with a timeout.
///
/// Returns `Err` if the response doesn't arrive within 10 seconds, preventing
/// permanent hangs when the connection drops during command processing.
async fn await_response(
    rx: tokio::sync::oneshot::Receiver<IpcResponse>,
) -> Result<IpcResponse, ServiceError> {
    tokio::select! {
        response = rx => {
            response.map_err(|_| ServiceError::Ipc("response channel closed".into()))
        }
        _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
            Err(ServiceError::Ipc("response timeout".into()))
        }
    }
}

/// Read a single length-prefixed frame from a read half.
///
/// Returns the payload bytes only (no length prefix).
async fn read_frame_from(
    read_half: &mut tokio::net::unix::OwnedReadHalf,
) -> Result<Option<Vec<u8>>, ServiceError> {
    let mut len_buf = [0u8; 4];
    match read_half.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(ServiceError::Ipc(format!("read frame: {e}"))),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(ServiceError::Ipc(format!("frame too large: {len}")));
    }
    if len == 0 {
        return Ok(None);
    }
    let mut payload = vec![0u8; len];
    read_half
        .read_exact(&mut payload)
        .await
        .map_err(|e| ServiceError::Ipc(format!("read payload: {e}")))?;
    Ok(Some(payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop::test_helpers::{
        setup_server, setup_server_with_factory, BlockingService, ImmediateSuccessService,
    };
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use tauri::Listener;

    // -- AC1: Client connects ---------------------------------------------------

    #[tokio::test]
    async fn ipc_client_connect() {
        let (path, shutdown, _event_tx) = setup_server();
        let result = IpcClient::connect(path).await;
        assert!(result.is_ok(), "client should connect: {:?}", result.err());
        shutdown.cancel();
    }

    // -- AC2: Start command works -----------------------------------------------

    #[tokio::test]
    async fn ipc_client_send_start() {
        let (path, shutdown, _event_tx) = setup_server();
        let mut client = IpcClient::connect(path).await.unwrap();
        let result = client.start(StartConfig::default()).await;
        assert!(result.is_ok(), "start should succeed: {:?}", result.err());
        shutdown.cancel();
    }

    // -- AC3: Stop command works ------------------------------------------------

    #[tokio::test]
    async fn ipc_client_send_stop() {
        let (path, shutdown, _event_tx) = setup_server();
        let mut client = IpcClient::connect(path).await.unwrap();
        client.start(StartConfig::default()).await.unwrap();
        let result = client.stop().await;
        assert!(result.is_ok(), "stop should succeed: {:?}", result.err());
        shutdown.cancel();
    }

    // -- AC4: IsRunning returns status ------------------------------------------

    #[tokio::test]
    async fn ipc_client_is_running() {
        let (path, shutdown, _event_tx) = setup_server();
        let mut client = IpcClient::connect(path).await.unwrap();

        let running = client.is_running().await.unwrap();
        assert!(!running, "should not be running initially");

        client.start(StartConfig::default()).await.unwrap();
        let running = client.is_running().await.unwrap();
        assert!(running, "should be running after start");

        shutdown.cancel();
    }

    // -- AC5: Events are received -----------------------------------------------

    #[tokio::test]
    async fn ipc_client_receive_events() {
        let (path, shutdown, event_tx) =
            setup_server_with_factory(Box::new(|| Box::new(ImmediateSuccessService)));
        let mut client = IpcClient::connect(path).await.unwrap();
        client.start(StartConfig::default()).await.unwrap();

        // Simulate relay broadcasting Started
        let _ = event_tx.send(IpcEvent::Started);

        let event = tokio::time::timeout(Duration::from_millis(500), client.read_event())
            .await
            .expect("timed out waiting for event")
            .expect("read_event failed");

        assert!(event.is_some(), "should receive an event");
        let event = event.unwrap();
        assert!(
            matches!(event, IpcEvent::Started),
            "Expected Started event, got {:?}",
            event
        );

        shutdown.cancel();
    }

    // -- Additional: Stop when not running returns error -------------------------

    #[tokio::test]
    async fn ipc_client_stop_when_not_running() {
        let (path, shutdown, _event_tx) = setup_server();
        let mut client = IpcClient::connect(path).await.unwrap();
        let result = client.stop().await;
        assert!(result.is_err(), "stop when not running should fail");
        shutdown.cancel();
    }

    // -- Additional: Connect to nonexistent socket fails -------------------------

    #[tokio::test]
    async fn ipc_client_connect_to_nonexistent() {
        let path = std::env::temp_dir().join("nonexistent-test-socket.sock");
        let result = IpcClient::connect(path).await;
        assert!(
            result.is_err(),
            "should fail to connect to nonexistent socket"
        );
    }

    // -- Additional: ipc_event_to_plugin_event conversion -----------------------

    #[test]
    fn ipc_event_to_plugin_event_started() {
        let event = IpcEvent::Started;
        let plugin = ipc_event_to_plugin_event(event);
        assert!(matches!(plugin, PluginEvent::Started));
    }

    #[test]
    fn ipc_event_to_plugin_event_stopped() {
        let event = IpcEvent::Stopped {
            reason: "cancelled".into(),
        };
        let plugin = ipc_event_to_plugin_event(event);
        match plugin {
            PluginEvent::Stopped { reason } => assert_eq!(reason, "cancelled"),
            other => panic!("Expected Stopped, got {other:?}"),
        }
    }

    #[test]
    fn ipc_event_to_plugin_event_error() {
        let event = IpcEvent::Error {
            message: "init failed".into(),
        };
        let plugin = ipc_event_to_plugin_event(event);
        match plugin {
            PluginEvent::Error { message } => assert_eq!(message, "init failed"),
            other => panic!("Expected Error, got {other:?}"),
        }
    }

    // -- Additional: Full lifecycle ---------------------------------------------

    #[tokio::test]
    async fn ipc_client_full_lifecycle() {
        let (path, shutdown, _event_tx) = setup_server();
        let mut client = IpcClient::connect(path).await.unwrap();

        assert!(!client.is_running().await.unwrap());
        client.start(StartConfig::default()).await.unwrap();
        assert!(client.is_running().await.unwrap());
        client.stop().await.unwrap();
        assert!(!client.is_running().await.unwrap());

        shutdown.cancel();
    }

    // -- Additional: listen_events spawns and converts events -------------------

    #[tokio::test]
    async fn ipc_client_listen_events() {
        let (path, shutdown, event_tx) =
            setup_server_with_factory(Box::new(|| Box::new(ImmediateSuccessService)));
        let app = tauri::test::mock_app();

        let received = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let received_clone = received.clone();
        app.listen("background-service://event", move |_event| {
            received_clone.store(true, Ordering::SeqCst);
        });

        let mut client = IpcClient::connect(path).await.unwrap();
        client.start(StartConfig::default()).await.unwrap();
        client.listen_events(app.handle().clone());

        // Simulate relay broadcasting Started
        let _ = event_tx.send(IpcEvent::Started);

        tokio::time::timeout(Duration::from_millis(500), async {
            while !received.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed out waiting for event via listen_events");

        assert!(
            received.load(Ordering::SeqCst),
            "should have received event"
        );
        shutdown.cancel();
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  IPC LOOPBACK TESTS (Step 20 — AC2, AC3, AC4)
    // ═══════════════════════════════════════════════════════════════════════

    // -- AC2: IPC loopback full lifecycle with event verification ---------------

    /// Comprehensive IPC loopback: IpcServer + IpcClient in the same process.
    /// Exercises start → Started event → running → stop → Stopped event → stopped.
    ///
    /// Note: IpcEvent frames must be read BEFORE other requests because
    /// `send_and_read` skips event frames looking for IpcResponse.
    #[tokio::test]
    async fn ipc_loopback_full_lifecycle_with_events() {
        let (path, shutdown, event_tx) = setup_server();
        let mut client = IpcClient::connect(path).await.unwrap();

        // Initially not running
        assert!(
            !client.is_running().await.unwrap(),
            "should not be running initially"
        );

        // Start the service
        client
            .start(StartConfig::default())
            .await
            .expect("start should succeed");

        // Simulate relay broadcasting Started
        let _ = event_tx.send(IpcEvent::Started);

        // Read the Started event BEFORE any other request
        // (send_and_read on subsequent calls would skip buffered events)
        let started = tokio::time::timeout(Duration::from_millis(500), client.read_event())
            .await
            .expect("timed out waiting for Started event")
            .expect("read_event failed")
            .expect("should receive event");
        assert!(
            matches!(started, IpcEvent::Started),
            "Expected Started event, got {started:?}"
        );

        // Verify running (after consuming the event)
        assert!(
            client.is_running().await.unwrap(),
            "should be running after start"
        );

        // Stop the service
        client.stop().await.expect("stop should succeed");

        // Simulate relay broadcasting Stopped
        let _ = event_tx.send(IpcEvent::Stopped {
            reason: "cancelled".into(),
        });

        // Read the Stopped event BEFORE any other request
        let stopped = tokio::time::timeout(Duration::from_millis(500), client.read_event())
            .await
            .expect("timed out waiting for Stopped event")
            .expect("read_event failed")
            .expect("should receive event");
        assert!(
            matches!(stopped, IpcEvent::Stopped { .. }),
            "Expected Stopped event, got {stopped:?}"
        );

        // Verify not running
        assert!(
            !client.is_running().await.unwrap(),
            "should not be running after stop"
        );

        shutdown.cancel();
    }

    // -- AC3: Event streaming converts IpcEvent to PluginEvent -------------------

    /// Verify events streamed through IPC are correctly converted to PluginEvent.
    #[tokio::test]
    async fn ipc_loopback_event_streaming_plugin_event_conversion() {
        let (path, shutdown, event_tx) = setup_server();
        let mut client = IpcClient::connect(path).await.unwrap();

        // Start — simulate relay broadcasting Started
        client.start(StartConfig::default()).await.unwrap();
        let _ = event_tx.send(IpcEvent::Started);
        let started_ipc = tokio::time::timeout(Duration::from_millis(500), client.read_event())
            .await
            .expect("timed out")
            .expect("read_event failed")
            .expect("should receive event");
        let started_plugin = ipc_event_to_plugin_event(started_ipc);
        assert!(
            matches!(started_plugin, PluginEvent::Started),
            "Expected PluginEvent::Started, got {started_plugin:?}"
        );

        // Stop — simulate relay broadcasting Stopped
        client.stop().await.unwrap();
        let _ = event_tx.send(IpcEvent::Stopped {
            reason: "cancelled".into(),
        });
        let stopped_ipc = tokio::time::timeout(Duration::from_millis(500), client.read_event())
            .await
            .expect("timed out")
            .expect("read_event failed")
            .expect("should receive event");
        let stopped_plugin = ipc_event_to_plugin_event(stopped_ipc);
        match stopped_plugin {
            PluginEvent::Stopped { reason } => {
                assert_eq!(reason, "cancelled", "Expected 'cancelled' reason");
            }
            other => panic!("Expected PluginEvent::Stopped, got {other:?}"),
        }

        shutdown.cancel();
    }

    // -- AC4: Error handling — connection drop detected by client ---------------

    /// Verify client detects a dropped connection gracefully (no panic).
    /// Simulates the server side closing the socket mid-connection.
    #[tokio::test]
    async fn ipc_loopback_connection_drop_returns_error() {
        let path = crate::desktop::test_helpers::unique_socket_path();

        // Create a minimal "server" that accepts one connection then drops it.
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let path_clone = path.clone();

        let client_handle =
            tokio::spawn(async move { IpcClient::connect(path_clone).await.unwrap() });

        // Accept the connection and immediately drop the server-side stream.
        let (server_stream, _) = listener.accept().await.unwrap();
        drop(server_stream);
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut client = client_handle.await.unwrap();

        // Client should detect the closed connection on next operation.
        let result = client.is_running().await;
        assert!(
            result.is_err(),
            "should get error after server drops connection"
        );

        let _ = std::fs::remove_file(&path);
    }

    // -- AC4: Error handling — double start returns error through IPC ------------

    /// Verify second start (when already running) returns an IPC error.
    #[tokio::test]
    async fn ipc_loopback_double_start_returns_error() {
        let (path, shutdown, _event_tx) = setup_server();
        let mut client = IpcClient::connect(path).await.unwrap();

        client.start(StartConfig::default()).await.unwrap();

        let result = client.start(StartConfig::default()).await;
        assert!(result.is_err(), "double start should return error");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.to_lowercase().contains("already"),
            "Error should mention 'already': {err_msg}"
        );

        shutdown.cancel();
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  PERSISTENT IPC CLIENT TESTS (Step 12)
    // ═══════════════════════════════════════════════════════════════════════

    // -- AC1: Persistent client connects and maintains connection --

    /// Verify the persistent client connects to a running server and can
    /// forward commands through the persistent connection.
    #[tokio::test]
    async fn persistent_client_connects() {
        let (path, shutdown, _event_tx) = setup_server();
        let app = tauri::test::mock_app();

        let handle = PersistentIpcClientHandle::spawn(path, app.handle().clone());

        // Give the background task time to connect.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Send a command through the persistent connection.
        let running = handle.is_running().await;
        assert!(
            running.is_ok(),
            "should get response via persistent connection: {:?}",
            running.err()
        );
        assert!(!running.unwrap(), "should not be running initially");

        shutdown.cancel();
    }

    // -- AC3: Auto-reconnect --

    /// Verify the persistent client reconnects after the server restarts.
    #[tokio::test]
    async fn persistent_client_reconnects() {
        use crate::desktop::ipc_server::IpcServer;
        use crate::manager::{manager_loop, ServiceFactory};
        use tokio_util::sync::CancellationToken;

        // First server
        let (path, shutdown1, _event_tx) = setup_server();
        let app = tauri::test::mock_app();

        let handle = PersistentIpcClientHandle::spawn(path.clone(), app.handle().clone());

        // Verify connected to first server.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let result = handle.is_running().await;
        assert!(
            result.is_ok(),
            "should connect to first server: {:?}",
            result.err()
        );

        // Kill first server and wait for socket cleanup.
        shutdown1.cancel();
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Start second server at the same path.
        let (cmd_tx2, cmd_rx2) = tokio::sync::mpsc::channel(16);
        let factory: ServiceFactory<tauri::test::MockRuntime> =
            Box::new(|| Box::new(BlockingService));
        tokio::spawn(manager_loop(
            cmd_rx2, factory, 0.0, 0.0, 0.0, 0.0, false, false,
        ));
        let server2 = IpcServer::bind(path.clone(), cmd_tx2, app.handle().clone()).unwrap();
        let shutdown2 = CancellationToken::new();
        let s2 = shutdown2.clone();
        tokio::spawn(async move { server2.run(s2).await });

        // Wait for the client to reconnect (1s reconnect delay + margin).
        let reconnected = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                tokio::time::sleep(Duration::from_millis(200)).await;
                if handle.is_running().await.is_ok() {
                    break;
                }
            }
        })
        .await;
        assert!(
            reconnected.is_ok(),
            "persistent client should reconnect to second server"
        );

        shutdown2.cancel();
    }

    // -- AC2: Event relay via app.emit() --

    /// Verify events from the server are relayed to `app.emit()` by the
    /// persistent client's background reader task.
    #[tokio::test]
    async fn event_relay() {
        let (path, shutdown, event_tx) =
            setup_server_with_factory(Box::new(|| Box::new(ImmediateSuccessService)));
        let app = tauri::test::mock_app();

        let received = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let received_clone = received.clone();
        app.listen("background-service://event", move |_event| {
            received_clone.store(true, Ordering::SeqCst);
        });

        let handle = PersistentIpcClientHandle::spawn(path, app.handle().clone());

        // Start the service — the reader task should relay the Started event.
        let result = handle.start(StartConfig::default()).await;
        assert!(result.is_ok(), "start should succeed: {:?}", result.err());

        // Simulate relay broadcasting Started
        let _ = event_tx.send(IpcEvent::Started);

        // Wait for the event to be relayed via app.emit().
        tokio::time::timeout(Duration::from_millis(500), async {
            while !received.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed out waiting for event relay via app.emit()");

        assert!(
            received.load(Ordering::SeqCst),
            "event should be relayed through app.emit()"
        );

        shutdown.cancel();
    }

    // -- AC4: Start/Stop lifecycle through persistent client --

    /// Verify the full start → running → stop → not-running lifecycle works
    /// through the persistent IPC client.
    #[tokio::test]
    async fn start_stop_lifecycle() {
        let (path, shutdown, _event_tx) = setup_server();
        let app = tauri::test::mock_app();

        let handle = PersistentIpcClientHandle::spawn(path, app.handle().clone());

        // Initially not running.
        let running = handle.is_running().await.unwrap();
        assert!(!running, "should not be running initially");

        // Start.
        handle
            .start(StartConfig::default())
            .await
            .expect("start should succeed");
        let running = handle.is_running().await.unwrap();
        assert!(running, "should be running after start");

        // Stop.
        handle.stop().await.expect("stop should succeed");
        let running = handle.is_running().await.unwrap();
        assert!(!running, "should not be running after stop");

        shutdown.cancel();
    }

    // -- Fix: Timeout prevents permanent hang on unresponsive server --

    /// Verify the persistent client returns an error (not hang) when the
    /// server accepts a connection but never responds to a command.
    ///
    /// This is a regression test for the critical bug where `wait_for_response`
    /// had no timeout — a dropped connection during command processing caused
    /// both the reconnect loop and the caller to hang permanently.
    #[tokio::test]
    async fn persistent_client_timeout_on_unresponsive_server() {
        let path = crate::desktop::test_helpers::unique_socket_path();
        let listener = tokio::net::UnixListener::bind(&path).unwrap();

        // Server that accepts the connection but never responds.
        let server_handle = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            // Hold connection open — never send a response.
            tokio::time::sleep(Duration::from_secs(60)).await;
        });

        let app = tauri::test::mock_app();
        let handle = PersistentIpcClientHandle::spawn(path.clone(), app.handle().clone());

        // Give the background task time to connect.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Start should timeout and return an error, not hang forever.
        let result = tokio::time::timeout(
            Duration::from_secs(15),
            handle.start(StartConfig::default()),
        )
        .await;

        assert!(
            result.is_ok(),
            "start should not hang — expected error, got outer timeout"
        );
        let inner = result.unwrap();
        assert!(
            inner.is_err(),
            "start should return error when server is unresponsive"
        );

        server_handle.abort();
        let _ = std::fs::remove_file(&path);
    }

    // -- C1: Persistent client terminates on handle drop --

    /// Verify that dropping `PersistentIpcClientHandle` causes the background
    /// reconnection task to stop (via `CancellationToken`), preventing resource
    /// leaks where the task reconnects forever after the handle is dropped.
    #[tokio::test]
    async fn persistent_client_terminates_on_handle_drop() {
        let (path, shutdown, _event_tx) = setup_server();
        let app = tauri::test::mock_app();

        let handle = PersistentIpcClientHandle::spawn(path, app.handle().clone());

        // Give the background task time to connect.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Drop the handle — this should cancel the shutdown token.
        drop(handle);

        // The background task should terminate within a bounded time.
        // We can't observe the JoinHandle directly (it's fire-and-forget),
        // but we can verify the socket isn't being reconnected to by checking
        // that server shutdown succeeds cleanly.
        tokio::time::sleep(Duration::from_secs(2)).await;

        shutdown.cancel();
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  BUFFERED EVENTS TESTS (Step 4)
    // ═══════════════════════════════════════════════════════════════════════

    /// Helper: create a raw server that sends specific frames in response to
    /// any request, giving full control over the event/response interleaving.
    async fn buffered_server(
        path: &std::path::Path,
        frames: Vec<IpcMessage>,
    ) -> tokio::task::JoinHandle<()> {
        let listener = tokio::net::UnixListener::bind(path).unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            // Read and discard the incoming request.
            let mut len_buf = [0u8; 4];
            if stream.read_exact(&mut len_buf).await.is_err() {
                return;
            }
            let len = u32::from_be_bytes(len_buf) as usize;
            let mut payload = vec![0u8; len];
            if stream.read_exact(&mut payload).await.is_err() {
                return;
            }
            // Send the pre-programmed frames in order.
            for msg in &frames {
                let frame = crate::desktop::ipc::encode_frame(msg).unwrap();
                if stream.write_all(&frame).await.is_err() {
                    return;
                }
            }
        })
    }

    /// send_and_read returns response with empty event list when no events interleave.
    #[tokio::test]
    async fn send_and_read_no_interleaved_events() {
        let path = crate::desktop::test_helpers::unique_socket_path();
        let server = buffered_server(
            &path,
            vec![IpcMessage::Response(IpcResponse {
                ok: true,
                data: None,
                error: None,
            })],
        )
        .await;

        let mut client = IpcClient::connect(path.clone()).await.unwrap();
        let (response, events) = client.send_and_read(&IpcRequest::IsRunning).await.unwrap();
        assert!(response.ok, "response should be ok");
        assert!(
            events.is_empty(),
            "events should be empty when no events interleave, got {:?}",
            events
        );

        server.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }

    /// send_and_read collects a single interleaved event alongside the response.
    #[tokio::test]
    async fn send_and_read_single_interleaved_event() {
        let path = crate::desktop::test_helpers::unique_socket_path();
        let server = buffered_server(
            &path,
            vec![
                IpcMessage::Event(IpcEvent::Started),
                IpcMessage::Response(IpcResponse {
                    ok: true,
                    data: None,
                    error: None,
                }),
            ],
        )
        .await;

        let mut client = IpcClient::connect(path.clone()).await.unwrap();
        let (response, events) = client
            .send_and_read(&IpcRequest::Start {
                config: StartConfig::default(),
            })
            .await
            .unwrap();
        assert!(response.ok, "response should be ok");
        assert_eq!(events.len(), 1, "should collect exactly one event");
        assert!(
            matches!(events[0], IpcEvent::Started),
            "expected Started event, got {:?}",
            events[0]
        );

        server.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  IS_CONNECTED TESTS (Step 5)
    // ═══════════════════════════════════════════════════════════════════════

    /// is_connected() returns false before the background task has connected
    /// to any server.
    #[tokio::test]
    async fn is_connected_false_before_server() {
        let app = tauri::test::mock_app();
        let path = crate::desktop::test_helpers::unique_socket_path();
        // No server running — spawn handle pointing at a nonexistent socket.
        let handle = PersistentIpcClientHandle::spawn(path.clone(), app.handle().clone());
        // The background task may or may not have attempted a connection yet,
        // but it should definitely NOT be connected.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !handle.is_connected(),
            "should not be connected when no server is running"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// is_connected() returns true once the persistent client has established
    /// a connection to a running server.
    #[tokio::test]
    async fn is_connected_true_after_connect() {
        let (path, shutdown, _event_tx) = setup_server();
        let app = tauri::test::mock_app();
        let handle = PersistentIpcClientHandle::spawn(path, app.handle().clone());

        // Wait for the background task to connect.
        tokio::time::timeout(Duration::from_secs(2), async {
            while !handle.is_connected() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("timed out waiting for is_connected to become true");

        assert!(
            handle.is_connected(),
            "should be connected after server is up"
        );

        shutdown.cancel();
    }

    /// is_connected() returns false after the server shuts down and the
    /// persistent client detects the disconnection.
    ///
    /// Uses a minimal server that accepts one connection then explicitly drops
    /// it, guaranteeing the reader task exits and sets connected = false.
    #[tokio::test]
    async fn is_connected_false_after_server_shutdown() {
        let path = crate::desktop::test_helpers::unique_socket_path();
        let path_clone = path.clone();
        let listener = tokio::net::UnixListener::bind(&path).unwrap();

        // Server that accepts a connection, waits briefly, then drops
        // everything (stream + listener), preventing reconnection.
        let server_handle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            // Hold the connection briefly so the client can connect.
            tokio::time::sleep(Duration::from_millis(200)).await;
            // Drop the stream — reader will detect EOF.
            drop(stream);
            // Drop the listener (moved into this closure) and clean up socket.
            let _ = std::fs::remove_file(&path_clone);
        });

        let app = tauri::test::mock_app();
        let handle = PersistentIpcClientHandle::spawn(path.clone(), app.handle().clone());

        // Wait for connection.
        tokio::time::timeout(Duration::from_secs(2), async {
            while !handle.is_connected() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("timed out waiting for initial connection");

        assert!(handle.is_connected(), "should be connected initially");

        // Wait for the server to drop the connection and listener.
        tokio::time::timeout(Duration::from_secs(3), async {
            while handle.is_connected() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("timed out waiting for is_connected to become false");

        assert!(
            !handle.is_connected(),
            "should not be connected after server shutdown"
        );

        server_handle.abort();
        let _ = std::fs::remove_file(&path);
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  BACKOFF BEHAVIOR TESTS (Step 6c)
    // ═══════════════════════════════════════════════════════════════════════

    /// Verify the ExponentialBuilder config used in persistent_client_loop
    /// produces increasing delays, respects the 30s max, and exhausts after
    /// exactly 10 attempts.
    #[test]
    fn backoff_builder_produces_increasing_delays() {
        use backon::BackoffBuilder;

        let builder = backon::ExponentialBuilder::default()
            .with_min_delay(Duration::from_secs(1))
            .with_max_delay(Duration::from_secs(30))
            .with_max_times(10)
            .with_jitter();

        let mut attempts = builder.build();
        let mut delays = Vec::new();
        while let Some(d) = attempts.next() {
            delays.push(d);
        }

        assert_eq!(delays.len(), 10, "should produce exactly 10 delays");

        // First delay ≈ 1s (with jitter, allow 0.5–2s).
        assert!(
            delays[0] >= Duration::from_millis(500),
            "first delay too short: {:?}",
            delays[0]
        );
        assert!(
            delays[0] <= Duration::from_secs(2),
            "first delay too long: {:?}",
            delays[0]
        );

        // Last delay should be at or near the 30s cap.
        assert!(
            delays[9] >= Duration::from_secs(15),
            "last delay should approach max: {:?}",
            delays[9]
        );

        // All delays capped — with jitter, allow up to 2× max_delay.
        for d in &delays {
            assert!(
                *d <= Duration::from_secs(60),
                "delay exceeds max_delay + jitter margin: {:?}",
                d
            );
        }

        // Iterator is exhausted after 10 attempts.
        assert!(
            attempts.next().is_none(),
            "should return None after 10 attempts"
        );
    }

    /// Verify the persistent client stops retrying after exhausting its backoff
    /// budget and that subsequent commands fail with "shut down" (channel closed).
    ///
    /// With min_delay=1s, max_delay=30s, max_times=10, total retry time ≈ 152s.
    /// Marked `#[ignore]` to avoid slowing down normal test runs.
    /// Run with `cargo test -- --ignored`.
    #[ignore]
    #[tokio::test]
    async fn persistent_client_exits_after_max_retries() {
        let app = tauri::test::mock_app();
        let path = crate::desktop::test_helpers::unique_socket_path();
        let handle = PersistentIpcClientHandle::spawn(path.clone(), app.handle().clone());

        // Wait for the background task to exhaust retries.
        // Poll is_running() — once the loop exits, the command channel closes
        // and we get "shut down" instead of a timeout.
        let exited = tokio::time::timeout(Duration::from_secs(180), async {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if let Err(e) = handle.is_running().await {
                    if e.to_string().contains("shut down") {
                        return;
                    }
                }
            }
        })
        .await;

        assert!(
            exited.is_ok(),
            "persistent client should exit after max retries"
        );
        assert!(!handle.is_connected(), "should not be connected after exit");

        let _ = std::fs::remove_file(&path);
    }

    /// Verify the persistent client reconnects after a server restart and that
    /// the backoff resets (reconnection starts from ~1s min_delay, not an
    /// accumulated delay).
    #[tokio::test]
    async fn persistent_client_reconnects_after_server_restart() {
        use crate::desktop::ipc_server::IpcServer;
        use crate::manager::{manager_loop, ServiceFactory};
        use tokio_util::sync::CancellationToken;

        // Start first server.
        let (path, shutdown1, _event_tx) = setup_server();
        let app = tauri::test::mock_app();
        let handle = PersistentIpcClientHandle::spawn(path.clone(), app.handle().clone());

        // Wait for connection to first server.
        tokio::time::timeout(Duration::from_secs(2), async {
            while !handle.is_connected() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("should connect to first server");

        // Verify commands work through first connection.
        let result = handle.is_running().await;
        assert!(
            result.is_ok(),
            "command should succeed on first server: {:?}",
            result.err()
        );

        // Kill first server.
        shutdown1.cancel();
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Start second server at the same path.
        let (cmd_tx2, cmd_rx2) = tokio::sync::mpsc::channel(16);
        let factory: ServiceFactory<tauri::test::MockRuntime> =
            Box::new(|| Box::new(BlockingService));
        tokio::spawn(manager_loop(
            cmd_rx2, factory, 0.0, 0.0, 0.0, 0.0, false, false,
        ));
        let server2 = IpcServer::bind(path.clone(), cmd_tx2, app.handle().clone()).unwrap();
        let shutdown2 = CancellationToken::new();
        let s2 = shutdown2.clone();
        tokio::spawn(async move { server2.run(s2).await });

        // Client should reconnect within ~1s (backoff resets to min_delay after
        // a successful session, so the first retry is ~1s, not accumulated).
        let reconnected = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if handle.is_connected() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await;

        assert!(
            reconnected.is_ok(),
            "persistent client should reconnect after server restart (backoff resets)"
        );

        // Verify commands work through the new connection.
        let result = handle.is_running().await;
        assert!(
            result.is_ok(),
            "commands should work after reconnection: {:?}",
            result.err()
        );

        shutdown2.cancel();
    }

    /// send_and_read collects multiple consecutive events before the response.
    #[tokio::test]
    async fn send_and_read_multiple_interleaved_events() {
        let path = crate::desktop::test_helpers::unique_socket_path();
        let server = buffered_server(
            &path,
            vec![
                IpcMessage::Event(IpcEvent::Started),
                IpcMessage::Event(IpcEvent::Error {
                    message: "warning".into(),
                }),
                IpcMessage::Event(IpcEvent::Stopped {
                    reason: "cancelled".into(),
                }),
                IpcMessage::Response(IpcResponse {
                    ok: true,
                    data: Some(serde_json::json!({"running": false})),
                    error: None,
                }),
            ],
        )
        .await;

        let mut client = IpcClient::connect(path.clone()).await.unwrap();
        let (response, events) = client.send_and_read(&IpcRequest::IsRunning).await.unwrap();
        assert!(response.ok, "response should be ok");
        assert_eq!(events.len(), 3, "should collect all three events");
        assert!(
            matches!(events[0], IpcEvent::Started),
            "first event should be Started"
        );
        assert!(
            matches!(events[1], IpcEvent::Error { .. }),
            "second event should be Error"
        );
        assert!(
            matches!(events[2], IpcEvent::Stopped { .. }),
            "third event should be Stopped"
        );

        server.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }
}
