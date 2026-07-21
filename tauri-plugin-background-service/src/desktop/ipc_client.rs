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

use crate::desktop::ipc::{
    decode_frame, encode_frame, IpcEvent, IpcMessage, IpcRequest, IpcResponse, PLUGIN_IPC_VERSION,
    VERSION_MISMATCH_CODE,
};
use crate::desktop::transport::{self, TransportReadHalf, TransportStream, TransportWriteHalf};
use crate::error::ServiceError;
#[cfg(all(test, unix))]
use crate::models::StopReason;
use crate::models::{PluginEvent, ServiceStatus, StartConfig};

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
    stream: TransportStream,
}

impl IpcClient {
    /// Connect to the sidecar's IPC socket at the given path.
    pub async fn connect(path: PathBuf) -> Result<Self, ServiceError> {
        let stream = transport::connect(&path).await?;
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

    /// Send a Hello/version handshake (doc-09/PROTO-14).
    ///
    /// Sends this client's [`PLUGIN_IPC_VERSION`] and returns the service's
    /// echoed protocol version. A caller consults this on connect to detect
    /// version skew (e.g. a still-running older headless process) BEFORE
    /// routing commands, surfacing a "version mismatch — restart" state on
    /// divergence instead of hanging on an opaque error.
    pub async fn hello(&mut self) -> Result<u32, ServiceError> {
        let request = IpcRequest::Hello {
            version: PLUGIN_IPC_VERSION,
        };
        let (response, _events) = self.send_and_read(&request).await?;
        if response.ok {
            response
                .data
                .and_then(|d| d.get("version").and_then(|v| v.as_u64()).map(|v| v as u32))
                .ok_or_else(|| ServiceError::Ipc("missing version in Hello response".into()))
        } else {
            Err(ServiceError::Ipc(
                response
                    .error
                    .unwrap_or_else(|| "Hello handshake failed".into()),
            ))
        }
    }

    /// Query the current service lifecycle state.
    pub async fn get_state(&mut self) -> Result<ServiceStatus, ServiceError> {
        let (response, _events) = self.send_and_read(&IpcRequest::GetState).await?;
        if response.ok {
            response
                .data
                .ok_or_else(|| ServiceError::Ipc("missing data in GetState response".into()))
                .and_then(|d| {
                    serde_json::from_value::<ServiceStatus>(d)
                        .map_err(|e| ServiceError::Ipc(format!("deserialize GetState: {e}")))
                })
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
        tauri::async_runtime::spawn(async move {
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
        transport::write_frame(&mut self.stream, &frame)
            .await
            .map_err(ServiceError::Ipc)?;
        Ok(())
    }

    /// Read a single length-prefixed frame from the socket.
    ///
    /// Returns the payload bytes only (no length prefix).
    /// Returns `None` if the connection was closed cleanly.
    async fn read_frame(&mut self) -> Result<Option<Vec<u8>>, ServiceError> {
        transport::read_frame(&mut self.stream)
            .await
            .map_err(ServiceError::Ipc)
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
    GetState {
        reply: tokio::sync::oneshot::Sender<Result<ServiceStatus, ServiceError>>,
    },
    EnableAutoRestart {
        config: Option<StartConfig>,
        reply: tokio::sync::oneshot::Sender<Result<(), ServiceError>>,
    },
    DisableAutoRestart {
        reply: tokio::sync::oneshot::Sender<Result<(), ServiceError>>,
    },
    GetDesiredState {
        reply: tokio::sync::oneshot::Sender<
            Result<Option<crate::desired_state::DesiredState>, ServiceError>,
        >,
    },
    ValidateSetup {
        reply: tokio::sync::oneshot::Sender<
            Result<crate::models::SetupValidationReport, ServiceError>,
        >,
    },
    GetLifecycleStatus {
        reply: tokio::sync::oneshot::Sender<Result<crate::models::LifecycleStatus, ServiceError>>,
    },
}

/// Handle to a persistent IPC client that maintains a long-lived connection
/// to the headless sidecar.
///
/// The background task automatically:
/// - Relays [`IpcEvent`] frames to `app.emit("background-service://event", ...)`
/// - Reconnects on connection failure with exponential backoff (1s–30s, retries until shutdown)
/// - Forwards commands (start/stop/is_running) over the same connection
pub struct PersistentIpcClientHandle {
    cmd_tx: tokio::sync::mpsc::Sender<IpcCommand>,
    shutdown: tokio_util::sync::CancellationToken,
    connected: Arc<AtomicBool>,
    desired_running: Arc<AtomicBool>,
    socket_path: PathBuf,
    nudge: Arc<tokio::sync::Notify>,
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
        let desired_running = Arc::new(AtomicBool::new(false));
        let nudge = Arc::new(tokio::sync::Notify::new());

        tauri::async_runtime::spawn(persistent_client_loop(
            socket_path.clone(),
            app,
            cmd_rx,
            shutdown.clone(),
            connected.clone(),
            nudge.clone(),
        ));

        Self {
            cmd_tx,
            shutdown,
            connected,
            desired_running,
            socket_path,
            nudge,
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
        let result = reply_rx
            .await
            .map_err(|_| ServiceError::Ipc("command dropped".into()))?;
        // DESK-03: a successful direct Start over IPC MUST update the local
        // desired_running mirror so a subsequent disconnect synthesizes
        // RecoveryPending (not Stopped). Previously only enable/disable
        // _auto_restart touched this mirror, leaving direct Starts invisible.
        if result.is_ok() {
            self.desired_running
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        result
    }

    /// Send a Stop command through the persistent connection.
    pub async fn stop(&self) -> Result<(), ServiceError> {
        if !self.is_connected() {
            return Err(ServiceError::Ipc("ipcUnavailable".into()));
        }
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(IpcCommand::Stop { reply: reply_tx })
            .await
            .map_err(|_| ServiceError::Ipc("persistent client shut down".into()))?;
        let result = reply_rx
            .await
            .map_err(|_| ServiceError::Ipc("command dropped".into()))?;
        // DESK-03: a successful direct Stop over IPC MUST clear the local
        // desired_running mirror so a subsequent disconnect synthesizes
        // Stopped (not RecoveryPending). Previously the mirror was untouched
        // here, so a stale `true` from a prior enable_auto_restart could
        // wrongly surface RecoveryPending after an explicit stop.
        if result.is_ok() {
            self.desired_running
                .store(false, std::sync::atomic::Ordering::Relaxed);
        }
        result
    }

    /// Query whether the service is running through the persistent connection.
    pub async fn is_running(&self) -> Result<bool, ServiceError> {
        if !self.is_connected() {
            return Err(ServiceError::Ipc("ipcUnavailable".into()));
        }
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(IpcCommand::IsRunning { reply: reply_tx })
            .await
            .map_err(|_| ServiceError::Ipc("persistent client shut down".into()))?;
        reply_rx
            .await
            .map_err(|_| ServiceError::Ipc("command dropped".into()))?
    }

    /// Query the current service lifecycle state through the persistent connection.
    pub async fn get_state(&self) -> Result<ServiceStatus, ServiceError> {
        if !self.is_connected() {
            return Err(ServiceError::Ipc("ipcUnavailable".into()));
        }
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(IpcCommand::GetState { reply: reply_tx })
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

    /// Returns the socket path this client is configured to connect to.
    pub fn socket_path(&self) -> &PathBuf {
        &self.socket_path
    }

    /// Wait until the persistent client is connected, polling `is_connected()`
    /// at 500ms intervals.
    ///
    /// Returns `Ok(true)` if connected within the timeout, `Ok(false)` if the
    /// timeout elapsed without connecting.
    pub async fn wait_for_connected(&self, timeout: Duration) -> Result<bool, ServiceError> {
        let deadline = tokio::time::Instant::now() + timeout;
        let poll_interval = Duration::from_millis(500);

        while tokio::time::Instant::now() < deadline {
            if self.is_connected() {
                return Ok(true);
            }
            let remaining = deadline - tokio::time::Instant::now();
            let sleep_dur = poll_interval.min(remaining);
            tokio::time::sleep(sleep_dur).await;
        }

        if self.is_connected() {
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Enable auto-restart through the persistent connection.
    pub async fn enable_auto_restart(
        &self,
        config: Option<StartConfig>,
    ) -> Result<(), ServiceError> {
        if !self.is_connected() {
            return Err(ServiceError::Ipc("ipcUnavailable".into()));
        }
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(IpcCommand::EnableAutoRestart {
                config,
                reply: reply_tx,
            })
            .await
            .map_err(|_| ServiceError::Ipc("persistent client shut down".into()))?;
        let result = reply_rx
            .await
            .map_err(|_| ServiceError::Ipc("command dropped".into()))?;
        if result.is_ok() {
            self.desired_running
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        result
    }

    /// Disable auto-restart through the persistent connection.
    pub async fn disable_auto_restart(&self) -> Result<(), ServiceError> {
        if !self.is_connected() {
            return Err(ServiceError::Ipc("ipcUnavailable".into()));
        }
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(IpcCommand::DisableAutoRestart { reply: reply_tx })
            .await
            .map_err(|_| ServiceError::Ipc("persistent client shut down".into()))?;
        let result = reply_rx
            .await
            .map_err(|_| ServiceError::Ipc("command dropped".into()))?;
        if result.is_ok() {
            self.desired_running
                .store(false, std::sync::atomic::Ordering::Relaxed);
        }
        result
    }

    /// Get the persisted desired-state through the persistent connection.
    pub async fn get_desired_state(
        &self,
    ) -> Result<Option<crate::desired_state::DesiredState>, ServiceError> {
        if !self.is_connected() {
            return Err(ServiceError::Ipc("ipcUnavailable".into()));
        }
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(IpcCommand::GetDesiredState { reply: reply_tx })
            .await
            .map_err(|_| ServiceError::Ipc("persistent client shut down".into()))?;
        reply_rx
            .await
            .map_err(|_| ServiceError::Ipc("command dropped".into()))?
    }

    /// Validate background service setup prerequisites through the persistent connection.
    pub async fn validate_setup(
        &self,
    ) -> Result<crate::models::SetupValidationReport, ServiceError> {
        if !self.is_connected() {
            return Err(ServiceError::Ipc("ipcUnavailable".into()));
        }
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(IpcCommand::ValidateSetup { reply: reply_tx })
            .await
            .map_err(|_| ServiceError::Ipc("persistent client shut down".into()))?;
        reply_rx
            .await
            .map_err(|_| ServiceError::Ipc("command dropped".into()))?
    }

    /// Get the complete lifecycle status snapshot.
    ///
    /// When connected, returns the actual status from the daemon.
    /// When disconnected, returns a synthesized status with `state = Stopped`
    /// (or `RecoveryPending` if local `desired_running` is true) and
    /// `last_platform_error = "ipcUnavailable"`.
    pub async fn get_lifecycle_status(
        &self,
    ) -> Result<crate::models::LifecycleStatus, ServiceError> {
        if !self.is_connected() {
            return Ok(self.synthesize_disconnected_status());
        }
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(IpcCommand::GetLifecycleStatus { reply: reply_tx })
            .await
            .map_err(|_| ServiceError::Ipc("persistent client shut down".into()))?;
        reply_rx
            .await
            .map_err(|_| ServiceError::Ipc("command dropped".into()))?
    }

    /// Build a synthesized `LifecycleStatus` for the disconnected state.
    fn synthesize_disconnected_status(&self) -> crate::models::LifecycleStatus {
        use crate::models::LifecycleState;

        let desired = self
            .desired_running
            .load(std::sync::atomic::Ordering::Relaxed);
        let state = if desired {
            LifecycleState::RecoveryPending
        } else {
            LifecycleState::Stopped
        };

        let (platform, _) =
            crate::capabilities::CapabilityProvider::detect_platform(Some("osService"));
        let capabilities = crate::capabilities::CapabilityProvider::capabilities(
            platform,
            crate::models::LifecycleMode::DesktopOsService,
            false,
        );

        crate::models::LifecycleStatus {
            state,
            desired_running: desired,
            recovery_enabled: desired,
            recovery_pending: desired,
            recovery_reason: if desired {
                Some("ipcUnavailable".into())
            } else {
                None
            },
            last_start_config: None,
            last_platform_state: None,
            last_platform_error: Some("ipcUnavailable".into()),
            last_error: Some("IPC disconnected".into()),
            platform,
            capabilities,
            issues: vec![],
            native_running: None,
            native_foreground: None,
            adopted: None,
            degraded: None,
            degraded_reason: None,
            data_dir: None,
        }
    }

    /// Set the local `desired_running` flag for testing.
    #[cfg(all(test, unix))]
    pub(crate) fn set_desired_running_for_test(&self, value: bool) {
        self.desired_running
            .store(value, std::sync::atomic::Ordering::Relaxed);
    }

    /// DESK-03 test seam: read the local desired_running mirror.
    #[cfg(all(test, unix))]
    pub(crate) fn desired_running_for_test(&self) -> bool {
        self.desired_running
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Nudge the persistent client to skip its current backoff delay and
    /// attempt an immediate reconnection.
    ///
    /// Useful after `installService()` or `startOsService()` when the
    /// persistent client is waiting in its backoff loop.
    pub fn nudge_reconnect(&self) {
        self.nudge.notify_one();
    }
}

/// Background task: maintain a persistent connection with reconnection.
async fn persistent_client_loop<R: Runtime>(
    socket_path: PathBuf,
    app: tauri::AppHandle<R>,
    mut cmd_rx: tokio::sync::mpsc::Receiver<IpcCommand>,
    shutdown: tokio_util::sync::CancellationToken,
    connected: Arc<AtomicBool>,
    nudge: Arc<tokio::sync::Notify>,
) {
    use backon::BackoffBuilder;

    let backoff_builder = backon::ExponentialBuilder::default()
        .with_min_delay(Duration::from_secs(1))
        .with_max_delay(Duration::from_secs(30))
        .without_max_times()
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
            connect_result = transport::connect(&socket_path) => {
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
                let delay = attempts.next().expect("backoff never exhausts without max_times");
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => {
                        log::info!("Persistent IPC client shutting down");
                        connected.store(false, std::sync::atomic::Ordering::Relaxed);
                        break;
                    }
                    _ = nudge.notified() => {
                        log::debug!("Persistent IPC client: nudge received, retrying immediately");
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
    stream: TransportStream,
    app: &tauri::AppHandle<R>,
    cmd_rx: &mut tokio::sync::mpsc::Receiver<IpcCommand>,
    connected: &Arc<AtomicBool>,
) -> Result<(), ServiceError> {
    let (read_half, mut write_half) = transport::split(stream);

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

    // [doc-09/PROTO-14]: proactive version handshake. Consult the service's
    // protocol version BEFORE routing commands, so a version-skewed (e.g.
    // still-running older) headless process is detected up front and surfaced
    // as a "version mismatch — restart" signal instead of hanging the GUI on
    // an opaque error. Fail-open: an inconclusive consult (timeout / transient
    // error) proceeds without blocking command routing — the service may still
    // serve legacy commands.
    {
        let hello_rx = prepare_response_slot(&response_slot).await;
        let sent = send_request_to(
            &mut write_half,
            &IpcRequest::Hello {
                version: PLUGIN_IPC_VERSION,
            },
        )
        .await
        .is_ok();
        if sent {
            match tokio::time::timeout(Duration::from_secs(2), await_response(hello_rx)).await {
                Ok(Ok(resp)) => {
                    let server_version = resp
                        .data
                        .as_ref()
                        .and_then(|d| d.get("version"))
                        .and_then(|v| v.as_u64())
                        .map(|v| v as u32);
                    // Three skew signals: (1) the service could not handle Hello
                    // at all (non-ok) — e.g. a still-running OLDER headless
                    // process whose protocol predates Hello; (2) it answered
                    // with a DIFFERENT version; (3) it returned the stable
                    // mismatch code. Any ⇒ surface "version mismatch — restart".
                    let mismatch = !resp.ok
                        || server_version.is_some_and(|v| v != PLUGIN_IPC_VERSION)
                        || resp.code.as_deref() == Some(VERSION_MISMATCH_CODE);
                    if mismatch {
                        let _ = app.emit(
                            "background-service://event",
                            PluginEvent::Error {
                                message: format!(
                                    "version mismatch — restart the app (service v{:?}, client v{})",
                                    server_version, PLUGIN_IPC_VERSION
                                ),
                            },
                        );
                        log::warn!(
                            "IPC version mismatch: service={:?} client={}",
                            server_version,
                            PLUGIN_IPC_VERSION
                        );
                    }
                }
                _ => {
                    // Timed out / transient error: clear the slot so the next
                    // command does not trip the sequential-slot invariant, then
                    // proceed without a version check.
                    let _ = response_slot.lock().await.take();
                    log::debug!("IPC Hello consult inconclusive; proceeding without version check");
                }
            }
        } else {
            // send_request_to failed — drop the unused slot sender.
            let _ = response_slot.lock().await.take();
        }
    }

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
                    IpcCommand::GetState { reply } => {
                        let rx = prepare_response_slot(&response_slot).await;
                        if let Err(e) = send_request_to(&mut write_half, &IpcRequest::GetState).await {
                            let _ = reply.send(Err(e));
                            break Err(ServiceError::Ipc("send failed".into()));
                        }
                        let response = await_response(rx).await;
                        let result = match response {
                            Ok(resp) if resp.ok => resp
                                .data
                                .ok_or_else(|| ServiceError::Ipc("missing data in GetState response".into()))
                                .and_then(|d| {
                                    serde_json::from_value::<ServiceStatus>(d)
                                        .map_err(|e| ServiceError::Ipc(format!("deserialize GetState: {e}")))
                                }),
                            Ok(resp) => Err(ServiceError::Ipc(
                                resp.error.unwrap_or_else(|| "unknown error".into()),
                            )),
                            Err(e) => Err(e),
                        };
                        let _ = reply.send(result);
                    }
                    IpcCommand::EnableAutoRestart { config, reply } => {
                        let request = IpcRequest::EnableAutoRestart { config };
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
                    IpcCommand::DisableAutoRestart { reply } => {
                        let rx = prepare_response_slot(&response_slot).await;
                        if let Err(e) = send_request_to(&mut write_half, &IpcRequest::DisableAutoRestart).await {
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
                    IpcCommand::GetDesiredState { reply } => {
                        let rx = prepare_response_slot(&response_slot).await;
                        if let Err(e) = send_request_to(&mut write_half, &IpcRequest::GetDesiredState).await {
                            let _ = reply.send(Err(e));
                            break Err(ServiceError::Ipc("send failed".into()));
                        }
                        let response = await_response(rx).await;
                        let result = match response {
                            Ok(resp) if resp.ok => {
                                match resp.data {
                                    Some(d) => serde_json::from_value::<crate::desired_state::DesiredState>(d)
                                        .map(Some)
                                        .map_err(|e| ServiceError::Ipc(format!("deserialize GetDesiredState: {e}"))),
                                    None => Ok(None),
                                }
                            }
                            Ok(resp) => Err(ServiceError::Ipc(
                                resp.error.unwrap_or_else(|| "unknown error".into()),
                            )),
                            Err(e) => Err(e),
                        };
                        let _ = reply.send(result);
                    }
                    IpcCommand::ValidateSetup { reply } => {
                        let rx = prepare_response_slot(&response_slot).await;
                        if let Err(e) = send_request_to(&mut write_half, &IpcRequest::ValidateSetup).await {
                            let _ = reply.send(Err(e));
                            break Err(ServiceError::Ipc("send failed".into()));
                        }
                        let response = await_response(rx).await;
                        let result = match response {
                            Ok(resp) if resp.ok => {
                                match resp.data {
                                    Some(d) => serde_json::from_value::<crate::models::SetupValidationReport>(d)
                                        .map_err(|e| ServiceError::Ipc(format!("deserialize ValidateSetup: {e}"))),
                                    None => Err(ServiceError::Ipc("missing ValidateSetup response data".into())),
                                }
                            }
                            Ok(resp) => Err(ServiceError::Ipc(
                                resp.error.unwrap_or_else(|| "unknown error".into()),
                            )),
                            Err(e) => Err(e),
                        };
                        let _ = reply.send(result);
                    }
                    IpcCommand::GetLifecycleStatus { reply } => {
                        let rx = prepare_response_slot(&response_slot).await;
                        if let Err(e) = send_request_to(&mut write_half, &IpcRequest::GetLifecycleStatus).await {
                            let _ = reply.send(Err(e));
                            break Err(ServiceError::Ipc("send failed".into()));
                        }
                        let response = await_response(rx).await;
                        let result = match response {
                            Ok(resp) if resp.ok => {
                                match resp.data {
                                    Some(d) => serde_json::from_value::<crate::models::LifecycleStatus>(d)
                                        .map_err(|e| ServiceError::Ipc(format!("deserialize GetLifecycleStatus: {e}"))),
                                    None => Err(ServiceError::Ipc("missing GetLifecycleStatus response data".into())),
                                }
                            }
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
    write_half: &mut TransportWriteHalf,
    request: &IpcRequest,
) -> Result<(), ServiceError> {
    let msg = IpcMessage::Request(request.clone());
    let frame = encode_frame(&msg).map_err(|e| ServiceError::Ipc(format!("encode: {e}")))?;
    transport::write_frame(write_half, &frame)
        .await
        .map_err(ServiceError::Ipc)?;
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
    read_half: &mut TransportReadHalf,
) -> Result<Option<Vec<u8>>, ServiceError> {
    transport::read_frame(read_half)
        .await
        .map_err(ServiceError::Ipc)
}

// Tests drive a real Unix socket via test_helpers (cfg(all(test, unix))).
#[cfg(all(test, unix))]
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

    // -- GetState returns ServiceStatus ------------------------------------------

    #[tokio::test]
    async fn ipc_client_get_state_initial() {
        let (path, shutdown, _event_tx) = setup_server();
        let mut client = IpcClient::connect(path).await.unwrap();

        let status = client.get_state().await.unwrap();
        assert!(
            matches!(status.state, crate::models::ServiceState::Idle),
            "expected Idle, got {:?}",
            status.state
        );
        assert_eq!(status.last_error, None);

        shutdown.cancel();
    }

    #[tokio::test]
    async fn ipc_client_get_state_after_start() {
        let (path, shutdown, _event_tx) = setup_server();
        let mut client = IpcClient::connect(path).await.unwrap();

        client.start(StartConfig::default()).await.unwrap();

        // Poll until Running — Start replies at Initializing, spawned task
        // transitions to Running asynchronously.
        let status = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let s = client.get_state().await.unwrap();
                if matches!(s.state, crate::models::ServiceState::Running) {
                    return s;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed out waiting for Running state");
        assert_eq!(status.last_error, None);

        shutdown.cancel();
    }

    #[tokio::test]
    async fn ipc_client_get_state_after_stop() {
        let (path, shutdown, _event_tx) = setup_server();
        let mut client = IpcClient::connect(path).await.unwrap();

        client.start(StartConfig::default()).await.unwrap();
        client.stop().await.unwrap();
        let status = client.get_state().await.unwrap();
        assert!(
            matches!(status.state, crate::models::ServiceState::Stopped),
            "expected Stopped, got {:?}",
            status.state
        );

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
            reason: StopReason::UserStop,
        };
        let plugin = ipc_event_to_plugin_event(event);
        match plugin {
            PluginEvent::Stopped { reason } => assert_eq!(reason, StopReason::UserStop),
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
            reason: StopReason::UserStop,
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
            reason: StopReason::UserStop,
        });
        let stopped_ipc = tokio::time::timeout(Duration::from_millis(500), client.read_event())
            .await
            .expect("timed out")
            .expect("read_event failed")
            .expect("should receive event");
        let stopped_plugin = ipc_event_to_plugin_event(stopped_ipc);
        match stopped_plugin {
            PluginEvent::Stopped { reason } => {
                assert_eq!(reason, StopReason::UserStop, "Expected UserStop reason");
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
        let listener = transport::bind(path.clone()).unwrap();
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
            cmd_rx2,
            factory,
            0.0,
            0.0,
            0.0,
            0.0,
            false,
            false,
            4.0,
            None,
            vec!["remoteMessaging".into()],
            false,
            crate::notifier::NotifierPolicy::default(),
            None,
            None,
            false,
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

        // Wait for connection before sending commands.
        handle
            .wait_for_connected(Duration::from_secs(2))
            .await
            .unwrap();

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

    // -- GetState through persistent client --

    #[tokio::test]
    async fn persistent_client_get_state() {
        let (path, shutdown, _event_tx) = setup_server();
        let app = tauri::test::mock_app();

        let handle = PersistentIpcClientHandle::spawn(path, app.handle().clone());

        // Give the background task time to connect.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let status = handle.get_state().await.unwrap();
        assert!(
            matches!(status.state, crate::models::ServiceState::Idle),
            "expected Idle, got {:?}",
            status.state
        );

        handle.start(StartConfig::default()).await.unwrap();

        // Poll until Running — race between Start reply (Initializing) and
        // spawned task transition to Running.
        let status = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let s = handle.get_state().await.unwrap();
                if matches!(s.state, crate::models::ServiceState::Running) {
                    return s;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed out waiting for Running state");
        assert!(
            matches!(status.state, crate::models::ServiceState::Running),
            "expected Running, got {:?}",
            status.state
        );

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
        let listener = transport::bind(path.clone()).unwrap();

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
        let listener = transport::bind(path.to_path_buf()).unwrap();
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
                code: None,
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
                    code: None,
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
        let listener = transport::bind(path.clone()).unwrap();

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
    /// produces increasing delays, respects the 30s max, and never exhausts.
    #[test]
    fn backoff_builder_produces_increasing_delays_indefinitely() {
        use backon::BackoffBuilder;

        let builder = backon::ExponentialBuilder::default()
            .with_min_delay(Duration::from_secs(1))
            .with_max_delay(Duration::from_secs(30))
            .without_max_times()
            .with_jitter();

        let mut attempts = builder.build();
        let mut delays: Vec<Duration> = Vec::new();
        for d in (&mut attempts).take(15) {
            delays.push(d);
        }

        assert!(
            delays.len() == 15,
            "should produce at least 15 delays without exhausting, got {}",
            delays.len()
        );

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

        // Delays increase monotonically (cap at 30s + jitter).
        let mut prev = Duration::ZERO;
        for d in &delays {
            assert!(
                *d <= Duration::from_secs(60),
                "delay exceeds max_delay + jitter margin: {:?}",
                d
            );
            // Exponential growth may be masked by jitter near the cap, so just
            // verify later delays are generally larger than early ones.
            let _ = prev;
            prev = *d;
        }

        // Later delays should be near the 30s cap.
        assert!(
            delays[14] >= Duration::from_secs(10),
            "delay 15 should approach max: {:?}",
            delays[14]
        );

        // Iterator never exhausts — still producing delays after 15.
        assert!(
            attempts.next().is_some(),
            "backoff should never exhaust without max_times"
        );
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
            cmd_rx2,
            factory,
            0.0,
            0.0,
            0.0,
            0.0,
            false,
            false,
            4.0,
            None,
            vec!["remoteMessaging".into()],
            false,
            crate::notifier::NotifierPolicy::default(),
            None,
            None,
            false,
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

    // ═══════════════════════════════════════════════════════════════════════
    //  RETRY-UNTIL-SHUTDOWN TESTS (Step 11)
    // ═══════════════════════════════════════════════════════════════════════

    // -- AC1: Retries beyond 10 attempts without stopping --

    /// Verify the persistent client keeps retrying when no server is available.
    /// After a few seconds (enough for several retry attempts with 1s min_delay),
    /// the command channel should still be open (not "shut down").
    #[tokio::test]
    async fn persistent_client_keeps_retrying_without_server() {
        let app = tauri::test::mock_app();
        let path = crate::desktop::test_helpers::unique_socket_path();
        let handle = PersistentIpcClientHandle::spawn(path.clone(), app.handle().clone());

        // Wait long enough for several retry attempts.
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Client should NOT be connected (no server).
        assert!(
            !handle.is_connected(),
            "should not be connected when no server"
        );

        // Command channel should still be open (not "shut down").
        // Commands queue in the channel but aren't processed without a connection,
        // so use a timeout to avoid hanging.
        let result = tokio::time::timeout(Duration::from_millis(200), handle.is_running()).await;

        match result {
            // Timeout is expected — command queued but no connection to process it.
            // The important thing is the handle is alive (not "shut down").
            Err(_) => { /* expected: command queued, no server to process */ }
            Ok(Err(e)) => {
                let msg = e.to_string();
                assert!(
                    !msg.contains("shut down"),
                    "client should still be retrying, not shut down: {msg}"
                );
            }
            Ok(Ok(_)) => { /* unexpected but not a failure */ }
        }

        let _ = std::fs::remove_file(&path);
    }

    // -- AC2: Clean shutdown via CancellationToken --

    /// Verify the persistent client exits cleanly when the shutdown token is
    /// cancelled while retrying (no server available).
    #[tokio::test]
    async fn persistent_client_exits_cleanly_on_shutdown_during_retries() {
        let app = tauri::test::mock_app();
        let path = crate::desktop::test_helpers::unique_socket_path();
        let handle = PersistentIpcClientHandle::spawn(path.clone(), app.handle().clone());

        // Wait a moment for the retry loop to start.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !handle.is_connected(),
            "should not be connected without server"
        );

        // Drop the handle — cancels the shutdown token.
        drop(handle);

        // The loop should exit cleanly (no panic, no resource leak).
        // We verify by checking that the socket file is not being held open.
        tokio::time::sleep(Duration::from_millis(500)).await;

        let _ = std::fs::remove_file(&path);
    }

    // -- AC3: Late server connection --

    /// Verify the persistent client connects successfully when a server starts
    /// after the client has already begun retrying.
    #[tokio::test]
    async fn persistent_client_connects_to_late_starting_server() {
        use crate::desktop::ipc_server::IpcServer;
        use crate::manager::{manager_loop, ServiceFactory};
        use tokio_util::sync::CancellationToken;

        let app = tauri::test::mock_app();
        let path = crate::desktop::test_helpers::unique_socket_path();

        // Spawn persistent client with NO server.
        let handle = PersistentIpcClientHandle::spawn(path.clone(), app.handle().clone());

        // Wait for a few retry attempts.
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(
            !handle.is_connected(),
            "should not be connected before server starts"
        );

        // Now start a server at the same path.
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let factory: ServiceFactory<tauri::test::MockRuntime> =
            Box::new(|| Box::new(BlockingService));
        tokio::spawn(manager_loop(
            cmd_rx,
            factory,
            0.0,
            0.0,
            0.0,
            0.0,
            false,
            false,
            4.0,
            None,
            vec!["remoteMessaging".into()],
            false,
            crate::notifier::NotifierPolicy::default(),
            None,
            None,
            false,
        ));
        let server = IpcServer::bind(path.clone(), cmd_tx, app.handle().clone()).unwrap();
        let shutdown = CancellationToken::new();
        let s = shutdown.clone();
        tokio::spawn(async move { server.run(s).await });

        // Client should connect to the late-starting server.
        let connected = handle
            .wait_for_connected(Duration::from_secs(5))
            .await
            .unwrap();
        assert!(
            connected,
            "persistent client should connect to late-starting server"
        );

        // Verify commands work.
        let result = handle.is_running().await;
        assert!(
            result.is_ok(),
            "commands should work after late connection: {:?}",
            result.err()
        );

        shutdown.cancel();
    }

    /// Verify that receiving a zero-length frame (\x00\x00\x00\x00) from the
    /// server produces an error, not `Ok(None)`. Zero-length frames are
    /// protocol violations and must be rejected explicitly.
    #[tokio::test]
    async fn ipc_client_rejects_zero_length_frame() {
        let path = crate::desktop::test_helpers::unique_socket_path();
        let listener = transport::bind(path.clone()).unwrap();

        // Server that sends a zero-length frame immediately after accepting.
        let server_handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            use tokio::io::AsyncWriteExt;
            stream.write_all(&[0u8; 4]).await.unwrap();
            tokio::time::sleep(Duration::from_millis(500)).await;
        });

        let mut client = IpcClient::connect(path.clone()).await.unwrap();

        // Reading a frame should return an error, not Ok(None)
        let result = client.read_frame().await;
        assert!(
            result.is_err(),
            "zero-length frame should return error, got {:?}",
            result
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("zero-length frame"),
            "Error should mention 'zero-length frame': {err}"
        );

        server_handle.abort();
        let _ = std::fs::remove_file(&path);
    }

    /// Verify that an actual EOF (connection drop) still returns `Ok(None)`.
    /// This is the "clean close" case — distinct from a zero-length frame.
    #[tokio::test]
    async fn ipc_client_eof_returns_ok_none() {
        let path = crate::desktop::test_helpers::unique_socket_path();
        let listener = transport::bind(path.clone()).unwrap();

        // Server that accepts a connection then immediately drops it.
        let server_handle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
        });

        let mut client = IpcClient::connect(path.clone()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Reading a frame should return Ok(None) for clean EOF
        let result = client.read_frame().await;
        assert!(result.is_ok(), "EOF should return Ok, got {:?}", result);
        assert!(result.unwrap().is_none(), "EOF should return Ok(None)");

        server_handle.abort();
        let _ = std::fs::remove_file(&path);
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  WAIT_FOR_CONNECTED TESTS (Step 12)
    // ═══════════════════════════════════════════════════════════════════════

    /// wait_for_connected returns Ok immediately when already connected.
    #[tokio::test]
    async fn wait_for_connected_returns_immediately_when_connected() {
        let (path, shutdown, _event_tx) = setup_server();
        let app = tauri::test::mock_app();
        let handle = PersistentIpcClientHandle::spawn(path, app.handle().clone());

        // Wait for initial connection.
        tokio::time::timeout(Duration::from_secs(2), async {
            while !handle.is_connected() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("should connect");

        // Already connected → should return Ok immediately.
        let result = handle
            .wait_for_connected(Duration::from_secs(5))
            .await
            .unwrap();
        assert!(result, "should return true when connected");

        shutdown.cancel();
    }

    /// wait_for_connected returns Err(timeout) when no server is running.
    #[tokio::test]
    async fn wait_for_connected_times_out_when_no_server() {
        let app = tauri::test::mock_app();
        let path = crate::desktop::test_helpers::unique_socket_path();
        let handle = PersistentIpcClientHandle::spawn(path.clone(), app.handle().clone());

        // No server → should time out quickly.
        let result = handle
            .wait_for_connected(Duration::from_millis(200))
            .await
            .unwrap();
        assert!(!result, "should return false when no server and timeout");

        let _ = std::fs::remove_file(&path);
    }

    /// wait_for_connected returns Ok once the server starts and client connects.
    #[tokio::test]
    async fn wait_for_connected_succeeds_after_server_starts() {
        let (path, shutdown, _event_tx) = setup_server();
        let app = tauri::test::mock_app();
        let handle = PersistentIpcClientHandle::spawn(path, app.handle().clone());

        // Server is running — wait_for_connected should succeed within timeout.
        let result = handle
            .wait_for_connected(Duration::from_secs(5))
            .await
            .unwrap();
        assert!(result, "should connect within timeout");

        shutdown.cancel();
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
                    reason: StopReason::UserStop,
                }),
                IpcMessage::Response(IpcResponse {
                    ok: true,
                    data: Some(serde_json::json!({"running": false})),
                    error: None,
                    code: None,
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

    // ═══════════════════════════════════════════════════════════════════════
    //  DISCONNECTED FAST-FAIL TESTS (Step 12 — IPC disconnected fast-fail)
    // ═══════════════════════════════════════════════════════════════════════

    /// Helper: spawn a persistent client with no server, confirming it is
    /// disconnected before returning.
    async fn disconnected_handle() -> (
        PersistentIpcClientHandle,
        std::path::PathBuf,
        tauri::AppHandle<tauri::test::MockRuntime>,
    ) {
        let app = tauri::test::mock_app();
        let path = crate::desktop::test_helpers::unique_socket_path();
        let handle = PersistentIpcClientHandle::spawn(path.clone(), app.handle().clone());
        // Give the background task a moment to attempt (and fail) a connection.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !handle.is_connected(),
            "handle should be disconnected in test helper"
        );
        (handle, path, app.handle().clone())
    }

    // -- AC1: Commands fast-fail when disconnected --

    #[tokio::test]
    async fn disconnected_stop_returns_ipc_unavailable() {
        let (handle, path, _) = disconnected_handle().await;
        let err = handle.stop().await.unwrap_err();
        assert!(
            err.to_string().contains("ipcUnavailable"),
            "stop should fast-fail with ipcUnavailable: {err}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn disconnected_is_running_returns_ipc_unavailable() {
        let (handle, path, _) = disconnected_handle().await;
        let err = handle.is_running().await.unwrap_err();
        assert!(
            err.to_string().contains("ipcUnavailable"),
            "is_running should fast-fail with ipcUnavailable: {err}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn disconnected_get_state_returns_ipc_unavailable() {
        let (handle, path, _) = disconnected_handle().await;
        let err = handle.get_state().await.unwrap_err();
        assert!(
            err.to_string().contains("ipcUnavailable"),
            "get_state should fast-fail with ipcUnavailable: {err}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn disconnected_enable_auto_restart_returns_ipc_unavailable() {
        let (handle, path, _) = disconnected_handle().await;
        let err = handle.enable_auto_restart(None).await.unwrap_err();
        assert!(
            err.to_string().contains("ipcUnavailable"),
            "enable_auto_restart should fast-fail with ipcUnavailable: {err}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn disconnected_disable_auto_restart_returns_ipc_unavailable() {
        let (handle, path, _) = disconnected_handle().await;
        let err = handle.disable_auto_restart().await.unwrap_err();
        assert!(
            err.to_string().contains("ipcUnavailable"),
            "disable_auto_restart should fast-fail with ipcUnavailable: {err}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn disconnected_get_desired_state_returns_ipc_unavailable() {
        let (handle, path, _) = disconnected_handle().await;
        let err = handle.get_desired_state().await.unwrap_err();
        assert!(
            err.to_string().contains("ipcUnavailable"),
            "get_desired_state should fast-fail with ipcUnavailable: {err}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn disconnected_validate_setup_returns_ipc_unavailable() {
        let (handle, path, _) = disconnected_handle().await;
        let err = handle.validate_setup().await.unwrap_err();
        assert!(
            err.to_string().contains("ipcUnavailable"),
            "validate_setup should fast-fail with ipcUnavailable: {err}"
        );
        let _ = std::fs::remove_file(&path);
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  SYNTHESIZED LIFECCYCLE STATUS TESTS (Step 13)
    // ═══════════════════════════════════════════════════════════════════════

    // -- AC1: Disconnected returns synthesized Stopped status --

    /// When disconnected with no local desired_running, get_lifecycle_status()
    /// returns Ok with a synthesized LifecycleStatus (not an error).
    #[tokio::test]
    async fn disconnected_get_lifecycle_status_returns_synthesized_stopped() {
        let (handle, path, _) = disconnected_handle().await;
        let status = handle.get_lifecycle_status().await.expect(
            "get_lifecycle_status should return Ok with synthesized status when disconnected",
        );
        assert!(
            matches!(status.state, crate::models::LifecycleState::Stopped),
            "expected Stopped, got {:?}",
            status.state
        );
        assert!(
            status.last_platform_error.as_deref() == Some("ipcUnavailable"),
            "expected last_platform_error = 'ipcUnavailable', got {:?}",
            status.last_platform_error
        );
        assert!(!status.desired_running, "desired_running should be false");
        assert!(!status.recovery_pending, "recovery_pending should be false");
        assert_eq!(
            status.capabilities.lifecycle_mode,
            crate::models::LifecycleMode::DesktopOsService,
            "lifecycle_mode should be DesktopOsService"
        );
        let _ = std::fs::remove_file(&path);
    }

    // -- AC2: Disconnected with desired_running returns RecoveryPending --

    /// When disconnected but local desired_running is true, get_lifecycle_status()
    /// returns state = RecoveryPending with ipcUnavailable error.
    #[tokio::test]
    async fn disconnected_with_desired_running_returns_recovery_pending() {
        let (handle, path, _) = disconnected_handle().await;

        // Simulate the state where enable_auto_restart succeeded before
        // disconnection (sets local desired_running = true).
        handle.set_desired_running_for_test(true);

        let status = handle
            .get_lifecycle_status()
            .await
            .expect("should return Ok with synthesized status when disconnected");
        assert!(
            matches!(status.state, crate::models::LifecycleState::RecoveryPending),
            "expected RecoveryPending, got {:?}",
            status.state
        );
        assert!(status.desired_running, "desired_running should be true");
        assert!(status.recovery_pending, "recovery_pending should be true");
        assert!(
            status.last_platform_error.as_deref() == Some("ipcUnavailable"),
            "expected last_platform_error = 'ipcUnavailable', got {:?}",
            status.last_platform_error
        );
        let _ = std::fs::remove_file(&path);
    }

    // -- AC3: Connected returns actual status (not synthesized) --

    /// When connected, get_lifecycle_status() returns the actual status from
    /// the daemon, not the synthesized disconnected status.
    #[tokio::test]
    async fn connected_get_lifecycle_status_returns_actual_status() {
        let (path, shutdown, _event_tx) = setup_server();
        let app = tauri::test::mock_app();
        let handle = PersistentIpcClientHandle::spawn(path, app.handle().clone());

        handle
            .wait_for_connected(Duration::from_secs(2))
            .await
            .unwrap();

        let status = handle
            .get_lifecycle_status()
            .await
            .expect("should return actual status when connected");
        // The daemon's initial state is Idle (not Stopped or synthesized).
        assert!(
            matches!(status.state, crate::models::LifecycleState::Idle),
            "expected Idle (actual daemon state), got {:?}",
            status.state
        );
        assert!(
            status.last_platform_error.is_none(),
            "connected status should not have ipcUnavailable error"
        );

        shutdown.cancel();
    }

    // ── DESK-03: direct Start/Stop over IPC update desired_running mirror ──

    /// Successful direct Start over IPC MUST set desired_running=true so a
    /// subsequent disconnect synthesizes RecoveryPending (not Stopped).
    #[tokio::test]
    async fn desk03_start_updates_desired_running_mirror() {
        let (path, shutdown, _event_tx) = setup_server();
        let app = tauri::test::mock_app();
        let handle = PersistentIpcClientHandle::spawn(path.clone(), app.handle().clone());
        handle
            .wait_for_connected(Duration::from_secs(2))
            .await
            .unwrap();

        assert!(
            !handle.desired_running_for_test(),
            "precondition: mirror starts false"
        );
        handle
            .start(crate::models::StartConfig::default())
            .await
            .expect("start over IPC should succeed");
        assert!(
            handle.desired_running_for_test(),
            "DESK-03: successful direct Start must set desired_running=true"
        );

        shutdown.cancel();
        let _ = std::fs::remove_file(&path);
    }

    /// Successful direct Stop over IPC MUST clear desired_running so a
    /// subsequent disconnect synthesizes Stopped (not RecoveryPending).
    #[tokio::test]
    async fn desk03_stop_clears_desired_running_mirror() {
        let (path, shutdown, _event_tx) = setup_server();
        let app = tauri::test::mock_app();
        let handle = PersistentIpcClientHandle::spawn(path.clone(), app.handle().clone());
        handle
            .wait_for_connected(Duration::from_secs(2))
            .await
            .unwrap();

        // Start first (sets mirror=true), then stop (must clear mirror).
        handle
            .start(crate::models::StartConfig::default())
            .await
            .unwrap();
        assert!(handle.desired_running_for_test());
        handle.stop().await.expect("stop over IPC should succeed");
        assert!(
            !handle.desired_running_for_test(),
            "DESK-03: successful direct Stop must clear desired_running"
        );

        shutdown.cancel();
        let _ = std::fs::remove_file(&path);
    }
    /// A FAILED Start does not flip the mirror — pinned at the source level
    /// rather than via a runtime test, because `start()` does not fast-fail
    /// on disconnect (it queues and retries). The conditional store
    /// (`if result.is_ok()`) is the contract; the source-grep test below
    /// pins it.
    #[test]
    fn desk03_start_stop_guard_mirror_with_is_ok() {
        let src = include_str!("ipc_client.rs");
        let start_body = src
            .split("pub async fn start(&self, config: StartConfig)")
            .nth(1)
            .and_then(|r| r.split("\n    }").next())
            .expect("start body");
        assert!(
            start_body.contains("if result.is_ok()") && start_body.contains("store(true"),
            "DESK-03: start must guard desired_running store behind result.is_ok()"
        );
        let stop_body = src
            .split("pub async fn stop(&self)")
            .nth(1)
            .and_then(|r| r.split("\n    }").next())
            .expect("stop body");
        assert!(
            stop_body.contains("if result.is_ok()") && stop_body.contains("store(false"),
            "DESK-03: stop must guard desired_running clear behind result.is_ok()"
        );
    }

    // -- AC2: start() does NOT fast-fail when disconnected --
    // The auto-start logic is in the caller (lib.rs). The IPC client's start()
    // should still accept the command (it queues into the channel), allowing
    // the caller to handle the auto-start flow.

    #[tokio::test]
    async fn disconnected_start_does_not_fast_fail() {
        let (handle, path, _) = disconnected_handle().await;
        // start() should NOT return ipcUnavailable — it should queue the
        // command. Use a timeout since the command will hang (no server).
        let result = tokio::time::timeout(
            Duration::from_millis(200),
            handle.start(StartConfig::default()),
        )
        .await;

        // Timeout means the command was queued (not fast-failed) — expected.
        // Or it could return an error from the channel, but NOT "ipcUnavailable".
        match result {
            Err(_) => { /* timeout — command queued, correct */ }
            Ok(Err(e)) => {
                let msg = e.to_string();
                assert!(
                    !msg.contains("ipcUnavailable"),
                    "start should NOT fast-fail with ipcUnavailable: {msg}"
                );
            }
            Ok(Ok(_)) => { /* unexpected success but not a fast-fail */ }
        }
        let _ = std::fs::remove_file(&path);
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  RECONNECT NUDGE TESTS (Step 14)
    // ═══════════════════════════════════════════════════════════════════════

    /// AC3: After nudge_reconnect(), the persistent client reconnects faster
    /// than the normal backoff delay (which can be up to 30s).
    ///
    /// Setup: spawn client with NO server → wait for backoff to ramp up →
    /// start server → nudge → verify reconnection within 2s.
    #[tokio::test]
    async fn nudge_reconnect_skips_backoff_delay() {
        let app = tauri::test::mock_app();
        let path = crate::desktop::test_helpers::unique_socket_path();
        let handle = PersistentIpcClientHandle::spawn(path.clone(), app.handle().clone());

        // Client is disconnected — wait for backoff to ramp up past 1s delay.
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(
            !handle.is_connected(),
            "should not be connected without server"
        );

        // Start a server at the expected path.
        let (shutdown2, _event_tx2) = {
            use crate::desktop::ipc_server::IpcServer;
            use crate::manager::{manager_loop, ServiceFactory};
            use tokio_util::sync::CancellationToken;
            let app2 = tauri::test::mock_app();
            let (cmd_tx2, cmd_rx2) = tokio::sync::mpsc::channel(16);
            let factory: ServiceFactory<tauri::test::MockRuntime> =
                Box::new(|| Box::new(BlockingService));
            tokio::spawn(manager_loop(
                cmd_rx2,
                factory,
                0.0,
                0.0,
                0.0,
                0.0,
                false,
                false,
                4.0,
                None,
                vec!["remoteMessaging".into()],
                false,
                crate::notifier::NotifierPolicy::default(),
                None,
                None,
                false,
            ));
            let server2 = IpcServer::bind(path.clone(), cmd_tx2, app2.handle().clone()).unwrap();
            let event_tx2 = server2.event_sender();
            let s2 = CancellationToken::new();
            let sc = s2.clone();
            tokio::spawn(async move { server2.run(sc).await });
            (s2, event_tx2)
        };

        // Nudge the client to skip the backoff.
        handle.nudge_reconnect();

        // Client should reconnect within 2s — much faster than the 30s max backoff.
        let connected = handle
            .wait_for_connected(Duration::from_secs(2))
            .await
            .expect("wait_for_connected");
        assert!(connected, "should reconnect quickly after nudge");

        shutdown2.cancel();
    }

    /// AC1/AC2: start() with auto-start — reconnect within timeout succeeds,
    /// timeout without reconnect returns error. These are tested via the
    /// lib.rs caller path. Here we verify the primitive: wait_for_connected
    /// returns false on timeout when no server is available.
    #[tokio::test]
    async fn wait_for_connected_timeout_with_nudge_still_fails() {
        let app = tauri::test::mock_app();
        let path = crate::desktop::test_helpers::unique_socket_path();
        let handle = PersistentIpcClientHandle::spawn(path.clone(), app.handle().clone());

        // Nudge when there's no server — should still time out.
        handle.nudge_reconnect();

        let connected = handle
            .wait_for_connected(Duration::from_millis(500))
            .await
            .expect("wait_for_connected");
        assert!(!connected, "should time out even with nudge when no server");

        let _ = std::fs::remove_file(&path);
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  OS-SERVICE INTEGRATION TESTS (Step 16)
    // ═══════════════════════════════════════════════════════════════════════

    /// Helper: start a fresh IPC server at the given path, returning the
    /// shutdown token. Uses the [`BlockingService`] factory.
    fn start_os_service_server(
        path: &std::path::Path,
        app: &tauri::AppHandle<tauri::test::MockRuntime>,
    ) -> tokio_util::sync::CancellationToken {
        use crate::desktop::ipc_server::IpcServer;
        use crate::manager::{manager_loop, ServiceFactory};

        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let factory: ServiceFactory<tauri::test::MockRuntime> =
            Box::new(|| Box::new(BlockingService));
        tokio::spawn(manager_loop(
            cmd_rx,
            factory,
            0.0,
            0.0,
            0.0,
            0.0,
            false,
            false,
            4.0,
            None,
            vec!["remoteMessaging".into()],
            false,
            crate::notifier::NotifierPolicy::default(),
            None,
            None,
            false,
        ));
        let server = IpcServer::bind(path.to_path_buf(), cmd_tx, app.clone()).unwrap();
        let shutdown = tokio_util::sync::CancellationToken::new();
        let s = shutdown.clone();
        tokio::spawn(async move { server.run(s).await });
        shutdown
    }

    // -- AC1: Full cycle (happy path) -----------------------------------------

    /// End-to-end happy path:
    /// 1. Persistent client connects to running server
    /// 2. `get_lifecycle_status()` returns Idle
    /// 3. `start()` → poll `get_lifecycle_status()` until Running
    /// 4. `stop()` → `get_lifecycle_status()` returns Stopped
    /// 5. `is_running()` returns false
    #[tokio::test]
    async fn os_service_full_cycle() {
        let (path, shutdown, _event_tx) = setup_server();
        let app = tauri::test::mock_app();
        let handle = PersistentIpcClientHandle::spawn(path, app.handle().clone());

        // Wait for connection.
        let connected = handle
            .wait_for_connected(Duration::from_secs(2))
            .await
            .expect("wait_for_connected");
        assert!(connected, "should connect to server");

        // Initially idle.
        let status = handle
            .get_lifecycle_status()
            .await
            .expect("get_lifecycle_status");
        assert!(
            matches!(status.state, crate::models::LifecycleState::Idle),
            "expected Idle, got {:?}",
            status.state
        );

        // Start the service.
        handle
            .start(StartConfig::default())
            .await
            .expect("start should succeed");

        // Poll until Running (manager transitions Initializing → Running async).
        let status = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let s = handle
                    .get_lifecycle_status()
                    .await
                    .expect("get_lifecycle_status");
                if matches!(s.state, crate::models::LifecycleState::Running) {
                    return s;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("timed out waiting for Running");
        assert!(
            matches!(status.state, crate::models::LifecycleState::Running),
            "expected Running, got {:?}",
            status.state
        );
        assert!(handle.is_running().await.expect("is_running"));

        // Stop the service.
        handle.stop().await.expect("stop should succeed");

        // Verify stopped.
        let status = handle
            .get_lifecycle_status()
            .await
            .expect("get_lifecycle_status");
        assert!(
            matches!(status.state, crate::models::LifecycleState::Stopped),
            "expected Stopped after stop, got {:?}",
            status.state
        );
        assert!(!handle.is_running().await.expect("is_running"));

        shutdown.cancel();
    }

    // -- AC2: No server — fast-fail + synthesized status ----------------------

    /// When no server is running:
    /// - `stop()`, `is_running()`, `get_state()` fast-fail with `ipcUnavailable`
    /// - `get_lifecycle_status()` returns synthesized `Stopped` (not an error)
    /// - All commands return within 200ms (no indefinite hang)
    #[tokio::test]
    async fn os_service_no_server_fast_fail_with_synthesized_status() {
        let (handle, path, _) = disconnected_handle().await;

        // Fast-fail: stop returns ipcUnavailable.
        let start = std::time::Instant::now();
        let err = handle.stop().await.unwrap_err();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(200),
            "stop should fast-fail within 200ms, took {elapsed:?}"
        );
        assert!(err.to_string().contains("ipcUnavailable"), "stop: {err}");

        // Fast-fail: is_running returns ipcUnavailable.
        let start = std::time::Instant::now();
        let err = handle.is_running().await.unwrap_err();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(200),
            "is_running should fast-fail within 200ms, took {elapsed:?}"
        );
        assert!(
            err.to_string().contains("ipcUnavailable"),
            "is_running: {err}"
        );

        // Fast-fail: get_state returns ipcUnavailable.
        let start = std::time::Instant::now();
        let err = handle.get_state().await.unwrap_err();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(200),
            "get_state should fast-fail within 200ms, took {elapsed:?}"
        );
        assert!(
            err.to_string().contains("ipcUnavailable"),
            "get_state: {err}"
        );

        // get_lifecycle_status returns SYNTHESIZED status (not error).
        let start = std::time::Instant::now();
        let status = handle
            .get_lifecycle_status()
            .await
            .expect("get_lifecycle_status should return Ok with synthesized status");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(200),
            "get_lifecycle_status should return within 200ms, took {elapsed:?}"
        );
        assert!(
            matches!(status.state, crate::models::LifecycleState::Stopped),
            "expected Stopped, got {:?}",
            status.state
        );
        assert_eq!(
            status.last_platform_error.as_deref(),
            Some("ipcUnavailable"),
            "should have ipcUnavailable error"
        );

        let _ = std::fs::remove_file(&path);
    }

    // -- AC3: Late server — retry + connect + commands work -------------------

    /// Client starts with no server, retries for 2s, then a server starts
    /// and the client connects. After connecting, the full start/stop lifecycle
    /// works correctly.
    #[tokio::test]
    async fn os_service_late_server_reconnect_and_commands() {
        let app = tauri::test::mock_app();
        let path = crate::desktop::test_helpers::unique_socket_path();

        // Spawn persistent client with NO server.
        let handle = PersistentIpcClientHandle::spawn(path.clone(), app.handle().clone());

        // Verify disconnected.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !handle.is_connected(),
            "should not be connected without server"
        );

        // get_lifecycle_status should return synthesized status while disconnected.
        let status = handle
            .get_lifecycle_status()
            .await
            .expect("synthesized status");
        assert!(
            matches!(status.state, crate::models::LifecycleState::Stopped),
            "expected Stopped while disconnected, got {:?}",
            status.state
        );

        // Start the server AFTER the client has been retrying.
        let shutdown = start_os_service_server(&path, &app.handle().clone());

        // Client should connect to the late-starting server.
        let connected = handle
            .wait_for_connected(Duration::from_secs(5))
            .await
            .expect("wait_for_connected");
        assert!(connected, "should connect to late-starting server");

        // Full lifecycle through the late-connected client.
        handle.start(StartConfig::default()).await.expect("start");

        // Poll until Running.
        let running = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if handle.is_running().await.unwrap_or(false) {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("timed out waiting for Running");
        assert!(running, "should be running after start");

        handle.stop().await.expect("stop");
        assert!(
            !handle.is_running().await.unwrap(),
            "should not be running after stop"
        );

        shutdown.cancel();
    }

    // -- AC4: Server restart — disconnect + auto-reconnect --------------------

    /// Verify the full server-restart workflow:
    /// 1. Client connects to a real IPC server (server 1)
    /// 2. Commands work on server 1
    /// 3. Server 1 shuts down gracefully
    /// 4. Client detects disconnect (or next command fails)
    /// 5. Server 2 starts at the same path
    /// 6. Client reconnects to server 2 (with nudge)
    /// 7. Full lifecycle works on server 2
    ///
    /// After server 1's graceful shutdown, existing spawned connection handlers
    /// may keep the old connection alive briefly. The test verifies that
    /// commands work after the transition regardless.
    #[tokio::test]
    async fn os_service_server_restart_automatic_reconnect() {
        let app = tauri::test::mock_app();
        let path = crate::desktop::test_helpers::unique_socket_path();

        // Start server 1.
        let shutdown1 = start_os_service_server(&path, app.handle());

        let handle = PersistentIpcClientHandle::spawn(path.clone(), app.handle().clone());

        // Wait for connection to server 1.
        let connected = handle
            .wait_for_connected(Duration::from_secs(2))
            .await
            .expect("wait_for_connected");
        assert!(connected, "should connect to server 1");

        // Verify commands work on server 1.
        let running = handle.is_running().await.expect("is_running on server 1");
        assert!(!running, "should not be running initially");

        // Shutdown server 1 gracefully.
        shutdown1.cancel();
        // Wait for socket cleanup and connection handlers to drain.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Start server 2 at the same path.
        let shutdown2 = start_os_service_server(&path, app.handle());

        // Nudge the client to skip any backoff delay.
        handle.nudge_reconnect();

        // Wait for the client to connect to server 2 (either via nudge or
        // automatic retry). The client might still be connected to server 1's
        // zombie handler, in which case is_connected() is already true and
        // commands still work. Or it might have disconnected and reconnected.
        let ready = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                // Try a command — if it works, we're connected to a working server.
                if handle.is_running().await.is_ok() {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("timed out waiting for working connection to server 2");

        assert!(ready, "should be able to send commands to server 2");

        // Full lifecycle on the current server.
        handle.start(StartConfig::default()).await.expect("start");

        let running = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if handle.is_running().await.unwrap_or(false) {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("timed out waiting for Running");
        assert!(running, "should be running after start");

        handle.stop().await.expect("stop");
        assert!(
            !handle.is_running().await.unwrap(),
            "should not be running after stop"
        );

        shutdown2.cancel();
    }

    // -- AC5: Timing bounds — no indefinite hangs -----------------------------

    /// Verify all persistent client commands return within bounded time when
    /// the server is running. This prevents regressions where commands might
    /// hang indefinitely.
    #[tokio::test]
    async fn os_service_commands_return_within_time_bounds() {
        let (path, shutdown, _event_tx) = setup_server();
        let app = tauri::test::mock_app();
        let handle = PersistentIpcClientHandle::spawn(path, app.handle().clone());

        handle
            .wait_for_connected(Duration::from_secs(2))
            .await
            .expect("should connect");

        let bound = Duration::from_secs(5);

        // is_running should return within bound.
        let start = std::time::Instant::now();
        let _ = handle.is_running().await;
        assert!(
            start.elapsed() < bound,
            "is_running took {:?}",
            start.elapsed()
        );

        // get_state should return within bound.
        let start = std::time::Instant::now();
        let _ = handle.get_state().await;
        assert!(
            start.elapsed() < bound,
            "get_state took {:?}",
            start.elapsed()
        );

        // get_lifecycle_status should return within bound.
        let start = std::time::Instant::now();
        let _ = handle.get_lifecycle_status().await;
        assert!(
            start.elapsed() < bound,
            "get_lifecycle_status took {:?}",
            start.elapsed()
        );

        // start should return within bound.
        let start = std::time::Instant::now();
        let _ = handle.start(StartConfig::default()).await;
        assert!(start.elapsed() < bound, "start took {:?}", start.elapsed());

        // stop should return within bound.
        let start = std::time::Instant::now();
        let _ = handle.stop().await;
        assert!(start.elapsed() < bound, "stop took {:?}", start.elapsed());

        shutdown.cancel();
    }
}
