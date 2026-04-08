//! Desktop IPC client for the GUI process.
//!
//! [`IpcClient`] connects to the headless sidecar's Unix domain socket and
//! provides methods to start/stop the background service and receive events
//! over the IPC protocol.
//!
//! Only available when the `desktop-service` Cargo feature is enabled.

use std::path::PathBuf;

use tauri::{Emitter, Runtime};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::desktop::ipc::{
    decode_frame, encode_frame, IpcEvent, IpcRequest, IpcResponse, MAX_FRAME_SIZE,
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
        let response = self.send_and_read(&request).await?;
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
        let response = self.send_and_read(&IpcRequest::Stop).await?;
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
        let response = self.send_and_read(&IpcRequest::IsRunning).await?;
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
        decode_frame::<IpcEvent>(&frame)
            .map(Some)
            .map_err(|e| ServiceError::Ipc(format!("decode event: {e}")))
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
    ) -> Result<IpcResponse, ServiceError> {
        self.send_request(request).await?;
        // The server interleaves IpcResponse and broadcast IpcEvent frames on
        // the same socket.  Read frames in a loop until we get one that
        // deserialises as an IpcResponse — any IpcEvent frames encountered
        // along the way are silently discarded (they'll also be delivered via
        // listen_events if the caller uses that path).
        loop {
            let frame = self
                .read_frame()
                .await?
                .ok_or_else(|| ServiceError::Ipc("connection closed".into()))?;
            if let Ok(resp) = decode_frame::<IpcResponse>(&frame) {
                return Ok(resp);
            }
            // Not a response frame — it's an event or unknown frame; skip it.
        }
    }

    async fn send_request(&mut self, request: &IpcRequest) -> Result<(), ServiceError> {
        let frame = encode_frame(request).map_err(|e| ServiceError::Ipc(format!("encode: {e}")))?;
        self.stream
            .write_all(&frame)
            .await
            .map_err(|e| ServiceError::Ipc(format!("send request: {e}")))?;
        Ok(())
    }

    /// Read a single length-prefixed frame from the socket.
    ///
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
        let mut frame = Vec::with_capacity(4 + len);
        frame.extend_from_slice(&len_buf);
        frame.extend_from_slice(&payload);
        Ok(Some(frame))
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
/// - Reconnects on connection failure with a 1-second delay
/// - Forwards commands (start/stop/is_running) over the same connection
pub struct PersistentIpcClientHandle {
    cmd_tx: tokio::sync::mpsc::Sender<IpcCommand>,
}

impl PersistentIpcClientHandle {
    /// Spawn the persistent IPC client background task.
    ///
    /// The task immediately begins trying to connect to the socket at
    /// `socket_path`. Events are relayed to the Tauri event system via
    /// `app.emit()`.
    pub fn spawn<R: Runtime>(socket_path: PathBuf, app: tauri::AppHandle<R>) -> Self {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);

        tokio::spawn(persistent_client_loop(socket_path, app, cmd_rx));

        Self { cmd_tx }
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
        reply_rx.await.map_err(|_| ServiceError::Ipc("command dropped".into()))?
    }

    /// Send a Stop command through the persistent connection.
    pub async fn stop(&self) -> Result<(), ServiceError> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(IpcCommand::Stop { reply: reply_tx })
            .await
            .map_err(|_| ServiceError::Ipc("persistent client shut down".into()))?;
        reply_rx.await.map_err(|_| ServiceError::Ipc("command dropped".into()))?
    }

    /// Query whether the service is running through the persistent connection.
    pub async fn is_running(&self) -> Result<bool, ServiceError> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(IpcCommand::IsRunning { reply: reply_tx })
            .await
            .map_err(|_| ServiceError::Ipc("persistent client shut down".into()))?;
        reply_rx.await.map_err(|_| ServiceError::Ipc("command dropped".into()))?
    }
}

/// Background task: maintain a persistent connection with reconnection.
async fn persistent_client_loop<R: Runtime>(
    socket_path: PathBuf,
    app: tauri::AppHandle<R>,
    mut cmd_rx: tokio::sync::mpsc::Receiver<IpcCommand>,
) {
    loop {
        match UnixStream::connect(&socket_path).await {
            Ok(stream) => {
                log::info!("Persistent IPC client connected");
                if run_persistent_connection(stream, &app, &mut cmd_rx).await.is_err() {
                    log::info!("Persistent IPC connection lost, reconnecting...");
                }
            }
            Err(_) => {
                log::debug!("Persistent IPC client: connection failed, retrying...");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
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
) -> Result<(), ServiceError> {
    let (read_half, mut write_half) = stream.into_split();

    // Shared slot for the reader task to deliver response frames.
    let response_slot: std::sync::Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<IpcResponse>>>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(None));

    let slot_writer = response_slot.clone();
    let app_clone = app.clone();

    // Reader task: reads frames and either relays events or delivers responses.
    let reader_handle = tokio::spawn(async move {
        let mut read_half = read_half;
        loop {
            let frame = match read_frame_from(&mut read_half).await {
                Ok(Some(f)) => f,
                Ok(None) => break, // Connection closed
                Err(_) => break,
            };

            // Try to decode as IpcResponse first (command reply)
            if let Ok(resp) = decode_frame::<IpcResponse>(&frame) {
                let mut slot = slot_writer.lock().await;
                if let Some(sender) = slot.take() {
                    let _ = sender.send(resp);
                }
                continue;
            }

            // Try to decode as IpcEvent
            if let Ok(event) = decode_frame::<IpcEvent>(&frame) {
                let plugin_event = ipc_event_to_plugin_event(event);
                let _ = app_clone.emit("background-service://event", plugin_event);
                continue;
            }

            // Unknown frame type — skip
        }
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
    let frame = encode_frame(request).map_err(|e| ServiceError::Ipc(format!("encode: {e}")))?;
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
    *slot.lock().await = Some(tx);
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
    let mut frame = Vec::with_capacity(4 + len);
    frame.extend_from_slice(&len_buf);
    frame.extend_from_slice(&payload);
    Ok(Some(frame))
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
        let (path, shutdown) = setup_server();
        let result = IpcClient::connect(path).await;
        assert!(result.is_ok(), "client should connect: {:?}", result.err());
        shutdown.cancel();
    }

    // -- AC2: Start command works -----------------------------------------------

    #[tokio::test]
    async fn ipc_client_send_start() {
        let (path, shutdown) = setup_server();
        let mut client = IpcClient::connect(path).await.unwrap();
        let result = client.start(StartConfig::default()).await;
        assert!(
            result.is_ok(),
            "start should succeed: {:?}",
            result.err()
        );
        shutdown.cancel();
    }

    // -- AC3: Stop command works ------------------------------------------------

    #[tokio::test]
    async fn ipc_client_send_stop() {
        let (path, shutdown) = setup_server();
        let mut client = IpcClient::connect(path).await.unwrap();
        client.start(StartConfig::default()).await.unwrap();
        let result = client.stop().await;
        assert!(
            result.is_ok(),
            "stop should succeed: {:?}",
            result.err()
        );
        shutdown.cancel();
    }

    // -- AC4: IsRunning returns status ------------------------------------------

    #[tokio::test]
    async fn ipc_client_is_running() {
        let (path, shutdown) = setup_server();
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
        let (path, shutdown) =
            setup_server_with_factory(Box::new(|| Box::new(ImmediateSuccessService)));
        let mut client = IpcClient::connect(path).await.unwrap();
        client.start(StartConfig::default()).await.unwrap();

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
        let (path, shutdown) = setup_server();
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
        let (path, shutdown) = setup_server();
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
        let (path, shutdown) =
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

        tokio::time::timeout(Duration::from_millis(500), async {
            while !received.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed out waiting for event via listen_events");

        assert!(received.load(Ordering::SeqCst), "should have received event");
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
        let (path, shutdown) = setup_server();
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
        let (path, shutdown) = setup_server();
        let mut client = IpcClient::connect(path).await.unwrap();

        // Start — expect Started event → PluginEvent::Started
        client.start(StartConfig::default()).await.unwrap();
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

        // Stop — expect Stopped event → PluginEvent::Stopped
        client.stop().await.unwrap();
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

        let client_handle = tokio::spawn(async move {
            IpcClient::connect(path_clone).await.unwrap()
        });

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
        let (path, shutdown) = setup_server();
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
        let (path, shutdown) = setup_server();
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
        let (path, shutdown1) = setup_server();
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
        tokio::spawn(manager_loop(cmd_rx2, factory, 0.0, 0.0));
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
        let (path, shutdown) =
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
        let (path, shutdown) = setup_server();
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
}
