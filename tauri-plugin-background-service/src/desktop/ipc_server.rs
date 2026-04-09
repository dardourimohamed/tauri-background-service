//! Desktop IPC server for the headless sidecar process.
//!
//! The IpcServer binds to a Unix domain socket and translates incoming
//! [`IpcRequest`] messages into [`ManagerCommand`] messages for the local actor
//! loop. Command outcomes produce [`IpcResponse`] and [`IpcEvent`] messages
//! sent back to connected clients.
//!
//! Events are broadcast to **all** connected clients via a [`broadcast`] channel,
//! not just the one that triggered the state change.

use std::path::PathBuf;

use tauri::{AppHandle, Runtime};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::desktop::ipc::{encode_frame, IpcEvent, IpcRequest, IpcResponse, MAX_FRAME_SIZE};
use crate::error::ServiceError;
use crate::manager::ManagerCommand;

/// Error type for reading IPC frames from a stream.
#[non_exhaustive]
enum ReadError {
    /// An I/O error (connection lost, etc.).
    Io(#[allow(dead_code)] std::io::Error),
    /// The JSON payload could not be deserialized as a valid [`IpcRequest`].
    Json(String),
    /// The frame payload exceeded [`MAX_FRAME_SIZE`].
    TooLarge(#[allow(dead_code)] usize),
}

/// Incoming message from the reader task.
enum Incoming {
    /// A valid IPC request.
    Request(IpcRequest),
    /// A recoverable error (malformed frame). Reader keeps running.
    Error(String),
    /// The connection was lost or a fatal error occurred.
    Done,
}

/// IPC server for the headless sidecar process.
///
/// Binds to a Unix domain socket, accepts client connections, and translates
/// incoming [`IpcRequest`] messages into [`ManagerCommand`] dispatches to the
/// local service manager actor. Responses and events are written back to the
/// client as [`IpcResponse`] and [`IpcEvent`] frames.
///
/// Events are broadcast to **all** connected clients, not just the one that
/// triggered the state change.
pub(crate) struct IpcServer<R: Runtime> {
    listener: UnixListener,
    cmd_tx: mpsc::Sender<ManagerCommand<R>>,
    app: AppHandle<R>,
    event_tx: broadcast::Sender<IpcEvent>,
    socket_path: PathBuf,
}

impl<R: Runtime> IpcServer<R> {
    /// Bind to the given socket path and return a new [`IpcServer`].
    ///
    /// Removes any stale socket file at the given path before binding.
    /// Refuses to bind if the path is a symlink (prevents symlink race attacks).
    pub fn bind(
        path: PathBuf,
        cmd_tx: mpsc::Sender<ManagerCommand<R>>,
        app: AppHandle<R>,
    ) -> Result<Self, ServiceError> {
        // Check for symlinks (including dangling ones) and remove stale sockets.
        // Use symlink_metadata directly — do NOT gate on path.exists(), which
        // follows symlinks and returns false for dangling ones.
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(ServiceError::Ipc(
                        "refusing to bind: socket path is a symlink".into(),
                    ));
                }
                // Remove stale socket file from a previous run.
                std::fs::remove_file(&path)
                    .map_err(|e| ServiceError::Ipc(format!("remove stale socket: {e}")))?;
            }
            Err(_) => {
                // Path does not exist — proceed to bind.
            }
        }
        let listener = UnixListener::bind(&path)
            .map_err(|e| ServiceError::Ipc(format!("bind failed: {e}")))?;
        let (event_tx, _) = broadcast::channel(32);
        Ok(Self {
            listener,
            cmd_tx,
            app,
            event_tx,
            socket_path: path,
        })
    }

    /// Get a clone of the broadcast sender for relaying events from the
    /// headless actor to connected IPC clients.
    pub fn event_sender(&self) -> broadcast::Sender<IpcEvent> {
        self.event_tx.clone()
    }

    /// Run the accept loop, spawning a task per client connection.
    ///
    /// This method consumes `self` and runs until either:
    /// - The `shutdown` token is cancelled (graceful shutdown)
    /// - The listener encounters a fatal error
    ///
    /// On exit, the socket file is removed from disk.
    pub async fn run(self, shutdown: CancellationToken) {
        let socket_path = self.socket_path.clone();
        loop {
            tokio::select! {
                accept_result = self.listener.accept() => {
                    match accept_result {
                        Ok((stream, _addr)) => {
                            let cmd_tx = self.cmd_tx.clone();
                            let app = self.app.clone();
                            let event_tx = self.event_tx.clone();
                            tokio::spawn(handle_connection(stream, cmd_tx, app, event_tx));
                        }
                        Err(e) => {
                            log::warn!("IPC accept error: {e}");
                            break;
                        }
                    }
                }
                _ = shutdown.cancelled() => {
                    log::info!("IPC server shutting down");
                    break;
                }
            }
        }
        // Clean up socket file on shutdown.
        let _ = std::fs::remove_file(&socket_path);
    }
}

/// Background task that reads [`IpcRequest`] frames from a stream and sends
/// them through an [`mpsc`] channel. This isolates the non-cancel-safe read
/// operations from the select loop in [`handle_connection`].
async fn request_reader(mut stream: tokio::net::unix::OwnedReadHalf, tx: mpsc::Sender<Incoming>) {
    loop {
        match read_request(&mut stream).await {
            Ok(req) => {
                if tx.send(Incoming::Request(req)).await.is_err() {
                    break;
                }
            }
            Err(ReadError::Json(msg)) => {
                if tx.send(Incoming::Error(msg)).await.is_err() {
                    break;
                }
            }
            Err(_) => {
                let _ = tx.send(Incoming::Done).await;
                break;
            }
        }
    }
}

/// Handle a single client connection.
///
/// Splits the stream into read and write halves. A reader task forwards
/// [`IpcRequest`] frames through an mpsc channel. The main loop uses
/// `tokio::select!` to handle both incoming requests and broadcast events.
/// Events are sourced exclusively from the broadcast channel (fed by the
/// headless event relay), not from request handling.
async fn handle_connection<R: Runtime>(
    stream: tokio::net::UnixStream,
    cmd_tx: mpsc::Sender<ManagerCommand<R>>,
    app: AppHandle<R>,
    event_tx: broadcast::Sender<IpcEvent>,
) {
    // Peer credential check: only same-user connections are allowed.
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        let peer_uid = unsafe {
            // SAFETY: getsockopt(SO_PEERCRED) on a connected Unix domain socket
            // is well-defined POSIX behavior. The fd is valid (just accepted),
            // the output buffer is a properly aligned &mut libc::ucred, and the
            // len parameter is initialized to the struct size.
            let mut creds: libc::ucred = std::mem::zeroed();
            let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
            let ret = libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut creds as *mut _ as *mut _,
                &mut len,
            );
            if ret == -1 {
                log::warn!("IPC: failed to get peer credentials, rejecting connection");
                return;
            }
            creds.uid
        };
        // SAFETY: getuid() never fails and has no side effects — it returns
        // the real UID of the calling process per POSIX.
        let my_uid = unsafe { libc::getuid() };
        if peer_uid != my_uid {
            log::warn!("IPC connection rejected: uid mismatch ({peer_uid} != {my_uid})");
            return;
        }
    }

    let mut event_rx = event_tx.subscribe();
    let (stream_read, mut stream_write) = stream.into_split();
    let (incoming_tx, mut incoming_rx) = mpsc::channel::<Incoming>(16);

    let reader_handle = tokio::spawn(request_reader(stream_read, incoming_tx));

    loop {
        tokio::select! {
            incoming = incoming_rx.recv() => {
                match incoming {
                    Some(Incoming::Request(request)) => {
                        let response = handle_request(
                            request, &cmd_tx, &app,
                        )
                        .await;
                        let resp_frame = match encode_frame(&response) {
                            Ok(f) => f,
                            Err(e) => {
                                log::warn!("IPC encode response error: {e}");
                                break;
                            }
                        };
                        if stream_write.write_all(&resp_frame).await.is_err() {
                            break;
                        }
                    }
                    Some(Incoming::Error(msg)) => {
                        let resp = IpcResponse {
                            ok: false,
                            data: None,
                            error: Some(msg),
                        };
                        let frame = match encode_frame(&resp) {
                            Ok(f) => f,
                            Err(e) => {
                                log::warn!("IPC encode error response: {e}");
                                break;
                            }
                        };
                        if stream_write.write_all(&frame).await.is_err() {
                            break;
                        }
                    }
                    Some(Incoming::Done) | None => break,
                }
            }
            event_result = event_rx.recv() => {
                match event_result {
                    Ok(event) => {
                        let frame = match encode_frame(&event) {
                            Ok(f) => f,
                            Err(e) => {
                                log::warn!("IPC encode event error: {e}");
                                break;
                            }
                        };
                        if stream_write.write_all(&frame).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        log::warn!("IPC client lagged {n} events");
                    }
                    Err(_) => break,
                }
            }
        }
    }

    reader_handle.abort();
}

/// Read a length-prefixed [`IpcRequest`] from the stream.
async fn read_request<R: tokio::io::AsyncRead + Unpin>(
    stream: &mut R,
) -> Result<IpcRequest, ReadError> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .map_err(ReadError::Io)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(ReadError::TooLarge(len));
    }
    let mut payload = vec![0u8; len];
    stream
        .read_exact(&mut payload)
        .await
        .map_err(ReadError::Io)?;
    serde_json::from_slice(&payload).map_err(|e| ReadError::Json(e.to_string()))
}

/// Forward an [`IpcRequest`] to the actor and return the response.
///
/// Events are NOT emitted here — the headless event relay (in `headless.rs`)
/// subscribes to actor-emitted `PluginEvent`s and forwards them as `IpcEvent`s
/// to the broadcast channel. This avoids duplicate events when both this
/// handler and the relay would send the same event.
async fn handle_request<R: Runtime>(
    request: IpcRequest,
    cmd_tx: &mpsc::Sender<ManagerCommand<R>>,
    app: &AppHandle<R>,
) -> IpcResponse {
    match request {
        IpcRequest::Start { config } => {
            let (reply, rx) = tokio::sync::oneshot::channel();
            if cmd_tx
                .send(ManagerCommand::Start {
                    config,
                    reply,
                    app: app.clone(),
                })
                .await
                .is_err()
            {
                return error_response("manager shut down");
            }
            match rx.await {
                Ok(Ok(())) => IpcResponse {
                    ok: true,
                    data: None,
                    error: None,
                },
                Ok(Err(e)) => IpcResponse {
                    ok: false,
                    data: None,
                    error: Some(e.to_string()),
                },
                Err(_) => error_response("manager dropped reply"),
            }
        }
        IpcRequest::Stop => {
            let (reply, rx) = tokio::sync::oneshot::channel();
            if cmd_tx.send(ManagerCommand::Stop { reply }).await.is_err() {
                return error_response("manager shut down");
            }
            match rx.await {
                Ok(Ok(())) => IpcResponse {
                    ok: true,
                    data: None,
                    error: None,
                },
                Ok(Err(e)) => IpcResponse {
                    ok: false,
                    data: None,
                    error: Some(e.to_string()),
                },
                Err(_) => error_response("manager dropped reply"),
            }
        }
        IpcRequest::IsRunning => {
            let (reply, rx) = tokio::sync::oneshot::channel();
            if cmd_tx
                .send(ManagerCommand::IsRunning { reply })
                .await
                .is_err()
            {
                return error_response("manager shut down");
            }
            match rx.await {
                Ok(running) => IpcResponse {
                    ok: true,
                    data: Some(serde_json::json!({ "running": running })),
                    error: None,
                },
                Err(_) => error_response("manager dropped reply"),
            }
        }
    }
}

/// Helper to build an error-only response.
fn error_response(msg: &str) -> IpcResponse {
    IpcResponse {
        ok: false,
        data: None,
        error: Some(msg.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop::test_helpers::{
        connect, read_event, read_response, send_request, setup_server_raw, unique_socket_path,
        BlockingService, ImmediateSuccessService,
    };
    use std::time::Duration;
    use tokio::net::UnixStream;

    fn setup_server_with_factory(
        factory: crate::manager::ServiceFactory<tauri::test::MockRuntime>,
    ) -> (
        IpcServer<tauri::test::MockRuntime>,
        PathBuf,
        CancellationToken,
    ) {
        setup_server_raw(factory)
    }

    fn setup_server() -> (
        IpcServer<tauri::test::MockRuntime>,
        PathBuf,
        CancellationToken,
    ) {
        setup_server_raw(Box::new(|| Box::new(BlockingService)))
    }

    // ── AC1: Server accepts connections ────────────────────────────────

    #[tokio::test]
    async fn ipc_server_accepts_connection() {
        let (server, path, shutdown) = setup_server();
        let s = shutdown.clone();
        let handle = tokio::spawn(async move { server.run(s).await });

        let result = UnixStream::connect(&path).await;
        assert!(result.is_ok(), "client should connect");

        shutdown.cancel();
        let _ = handle.await;
    }

    // ── AC2: Start command works ───────────────────────────────────────

    #[tokio::test]
    async fn ipc_server_start_command() {
        let (server, path, shutdown) = setup_server();
        let s = shutdown.clone();
        let handle = tokio::spawn(async move { server.run(s).await });

        let mut stream = connect(&path).await;
        send_request(
            &mut stream,
            &IpcRequest::Start {
                config: crate::models::StartConfig::default(),
            },
        )
        .await;

        let response = read_response(&mut stream).await;
        assert!(response.ok, "Start should succeed: {:?}", response.error);

        shutdown.cancel();
        let _ = handle.await;
    }

    // ── AC3: Stop command works ────────────────────────────────────────

    #[tokio::test]
    async fn ipc_server_stop_command() {
        let (server, path, shutdown) = setup_server();
        let s = shutdown.clone();
        let handle = tokio::spawn(async move { server.run(s).await });

        let mut stream = connect(&path).await;

        // Start first
        send_request(
            &mut stream,
            &IpcRequest::Start {
                config: crate::models::StartConfig::default(),
            },
        )
        .await;
        let resp = read_response(&mut stream).await;
        assert!(resp.ok);

        // Stop
        send_request(&mut stream, &IpcRequest::Stop).await;
        let resp = read_response(&mut stream).await;
        assert!(resp.ok, "Stop should succeed: {:?}", resp.error);

        shutdown.cancel();
        let _ = handle.await;
    }

    // ── AC4: Events are streamed (via relay broadcast channel) ──────────

    #[tokio::test]
    async fn ipc_server_streams_started_event() {
        let (server, path, shutdown) =
            setup_server_with_factory(Box::new(|| Box::new(ImmediateSuccessService)));
        let event_tx = server.event_sender();
        let s = shutdown.clone();
        let handle = tokio::spawn(async move { server.run(s).await });

        let mut stream = connect(&path).await;
        send_request(
            &mut stream,
            &IpcRequest::Start {
                config: crate::models::StartConfig::default(),
            },
        )
        .await;

        // Read response first
        let resp = read_response(&mut stream).await;
        assert!(resp.ok);

        // Simulate event relay broadcasting Started
        let _ = event_tx.send(IpcEvent::Started);

        // Read event — should be Started (from relay broadcast)
        let event = tokio::time::timeout(Duration::from_millis(500), read_event(&mut stream))
            .await
            .expect("timed out waiting for Started event");
        assert!(
            matches!(event, IpcEvent::Started),
            "Expected Started event, got {:?}",
            event
        );

        shutdown.cancel();
        let _ = handle.await;
    }

    // ── AC5: Malformed frames handled gracefully ───────────────────────

    #[tokio::test]
    async fn ipc_server_rejects_malformed_frame() {
        let (server, path, shutdown) = setup_server();
        let s = shutdown.clone();
        let handle = tokio::spawn(async move { server.run(s).await });

        let mut stream = connect(&path).await;

        // Send a valid length prefix + invalid JSON
        let payload = b"not valid json!!!";
        let mut frame = Vec::with_capacity(4 + payload.len());
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(payload);
        stream.write_all(&frame).await.unwrap();

        // Read error response
        let resp = read_response(&mut stream).await;
        assert!(!resp.ok, "should be error response");
        assert!(resp.error.is_some(), "should have error message");

        // Connection should still be open — send a valid request
        send_request(&mut stream, &IpcRequest::IsRunning).await;
        let resp2 = read_response(&mut stream).await;
        assert!(
            resp2.ok,
            "connection should still work after malformed frame"
        );

        shutdown.cancel();
        let _ = handle.await;
    }

    // ── AC6: Client disconnect handled ─────────────────────────────────

    #[tokio::test]
    async fn ipc_server_handles_client_disconnect() {
        let (server, path, shutdown) = setup_server();
        let s = shutdown.clone();
        let handle = tokio::spawn(async move { server.run(s).await });

        // Connect and immediately drop
        {
            let _stream = connect(&path).await;
        }

        // Give the server a moment to process the disconnect
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Server should still accept new connections
        let result = UnixStream::connect(&path).await;
        assert!(
            result.is_ok(),
            "server should still accept connections after client disconnect"
        );

        shutdown.cancel();
        let _ = handle.await;
    }

    // ── Additional: IsRunning returns correct state ────────────────────

    #[tokio::test]
    async fn ipc_server_is_running_returns_false_initially() {
        let (server, path, shutdown) = setup_server();
        let s = shutdown.clone();
        let handle = tokio::spawn(async move { server.run(s).await });

        let mut stream = connect(&path).await;
        send_request(&mut stream, &IpcRequest::IsRunning).await;
        let resp = read_response(&mut stream).await;
        assert!(resp.ok);
        assert_eq!(resp.data.unwrap()["running"], false);

        shutdown.cancel();
        let _ = handle.await;
    }

    // ── Additional: Stop when not running returns error ────────────────

    #[tokio::test]
    async fn ipc_server_stop_when_not_running() {
        let (server, path, shutdown) = setup_server();
        let s = shutdown.clone();
        let handle = tokio::spawn(async move { server.run(s).await });

        let mut stream = connect(&path).await;
        send_request(&mut stream, &IpcRequest::Stop).await;
        let resp = read_response(&mut stream).await;
        assert!(!resp.ok, "stop when not running should fail");
        assert!(resp.error.unwrap().contains("not running"));

        shutdown.cancel();
        let _ = handle.await;
    }

    // ── Additional: Stopped event on stop (via relay) ─────────────────

    #[tokio::test]
    async fn ipc_server_stopped_event_on_stop() {
        let (server, path, shutdown) = setup_server();
        let event_tx = server.event_sender();
        let s = shutdown.clone();
        let handle = tokio::spawn(async move { server.run(s).await });

        let mut stream = connect(&path).await;

        // Start
        send_request(
            &mut stream,
            &IpcRequest::Start {
                config: crate::models::StartConfig::default(),
            },
        )
        .await;
        let resp = read_response(&mut stream).await;
        assert!(resp.ok);

        // Simulate relay broadcasting Started
        let _ = event_tx.send(IpcEvent::Started);
        let _ = tokio::time::timeout(Duration::from_millis(500), read_event(&mut stream)).await;

        // Stop
        send_request(&mut stream, &IpcRequest::Stop).await;
        let resp = read_response(&mut stream).await;
        assert!(resp.ok);

        // Simulate relay broadcasting Stopped
        let _ = event_tx.send(IpcEvent::Stopped {
            reason: "cancelled".into(),
        });
        let event = tokio::time::timeout(Duration::from_millis(500), read_event(&mut stream))
            .await
            .expect("timed out waiting for Stopped event");
        assert!(
            matches!(event, IpcEvent::Stopped { .. }),
            "Expected Stopped event, got {:?}",
            event
        );

        shutdown.cancel();
        let _ = handle.await;
    }

    // ── Additional: Multiple clients can connect ───────────────────────

    #[tokio::test]
    async fn ipc_server_multiple_clients() {
        let (server, path, shutdown) = setup_server();
        let event_tx = server.event_sender();
        let s = shutdown.clone();
        let handle = tokio::spawn(async move { server.run(s).await });

        let mut stream1 = connect(&path).await;
        let mut stream2 = connect(&path).await;

        // Start via client 1
        send_request(
            &mut stream1,
            &IpcRequest::Start {
                config: crate::models::StartConfig::default(),
            },
        )
        .await;
        let resp1 = read_response(&mut stream1).await;
        assert!(resp1.ok);

        // Simulate relay broadcasting Started — both clients should receive it
        let _ = event_tx.send(IpcEvent::Started);
        let _ = tokio::time::timeout(Duration::from_millis(500), read_event(&mut stream1)).await;
        let _ = tokio::time::timeout(Duration::from_millis(500), read_event(&mut stream2)).await;

        // Client 2 can query is_running
        send_request(&mut stream2, &IpcRequest::IsRunning).await;
        let resp2 = read_response(&mut stream2).await;
        assert!(resp2.ok);
        assert_eq!(resp2.data.unwrap()["running"], true);

        shutdown.cancel();
        let _ = handle.await;
    }

    // ── TR4: Graceful shutdown via CancellationToken ───────────────────

    #[tokio::test]
    async fn ipc_server_graceful_shutdown() {
        let (server, path, shutdown) = setup_server();
        let s = shutdown.clone();
        let handle = tokio::spawn(async move { server.run(s).await });

        // Server is running — client can connect
        let result = UnixStream::connect(&path).await;
        assert!(result.is_ok(), "should connect before shutdown");

        // Trigger graceful shutdown
        shutdown.cancel();

        // run() should return cleanly
        let _ = handle.await;

        // Socket file should be cleaned up
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !path.exists(),
            "socket file should be removed after graceful shutdown"
        );
    }

    // ── TR6: Events broadcast to all connected clients (via relay) ──────

    #[tokio::test]
    async fn ipc_server_broadcasts_events_to_all_clients() {
        let (server, path, shutdown) = setup_server();
        let event_tx = server.event_sender();
        let s = shutdown.clone();
        let handle = tokio::spawn(async move { server.run(s).await });

        // Connect two clients
        let mut stream1 = connect(&path).await;
        let mut stream2 = connect(&path).await;

        // Start via client 1
        send_request(
            &mut stream1,
            &IpcRequest::Start {
                config: crate::models::StartConfig::default(),
            },
        )
        .await;
        let resp1 = read_response(&mut stream1).await;
        assert!(resp1.ok);

        // Simulate relay broadcasting Started — all clients should receive it
        let _ = event_tx.send(IpcEvent::Started);

        // Client 1 should get Started event (broadcast)
        let event1 = tokio::time::timeout(Duration::from_millis(500), read_event(&mut stream1))
            .await
            .expect("client 1 timed out waiting for Started event");
        assert!(
            matches!(event1, IpcEvent::Started),
            "Client 1: expected Started, got {:?}",
            event1
        );

        // Client 2 should ALSO get Started event (broadcast)
        let event2 = tokio::time::timeout(Duration::from_millis(500), read_event(&mut stream2))
            .await
            .expect("client 2 timed out waiting for broadcast Started event");
        assert!(
            matches!(event2, IpcEvent::Started),
            "Client 2: expected broadcast Started, got {:?}",
            event2
        );

        shutdown.cancel();
        let _ = handle.await;
    }

    // ── Additional: No duplicate events ────────────────────────────────

    /// Verify that the requesting client receives each event exactly once.
    /// Events come from the relay broadcast channel only — not from the
    /// request handler.
    #[tokio::test]
    async fn ipc_server_no_duplicate_events() {
        let (server, path, shutdown) = setup_server();
        let event_tx = server.event_sender();
        let s = shutdown.clone();
        let handle = tokio::spawn(async move { server.run(s).await });

        let mut stream = connect(&path).await;

        // Start — only response, no event from handler
        send_request(
            &mut stream,
            &IpcRequest::Start {
                config: crate::models::StartConfig::default(),
            },
        )
        .await;
        let resp = read_response(&mut stream).await;
        assert!(resp.ok);

        // Relay broadcasts exactly one Started event
        let _ = event_tx.send(IpcEvent::Started);

        let event = tokio::time::timeout(Duration::from_millis(500), read_event(&mut stream))
            .await
            .expect("timed out waiting for Started event");
        assert!(matches!(event, IpcEvent::Started));

        // Verify NO second event arrives (would indicate duplication)
        let result =
            tokio::time::timeout(Duration::from_millis(100), read_event(&mut stream)).await;
        assert!(
            result.is_err(),
            "should not receive a duplicate Started event"
        );

        shutdown.cancel();
        let _ = handle.await;
    }

    // ── Step 7: Peer credential check (same-UID) ────────────────────

    /// Verify that a same-user connection passes the SO_PEERCRED check.
    /// On Linux this exercises the getsockopt(SO_PEERCRED) path; on other
    /// platforms the peer-cred block is compiled out so the test just
    /// confirms the connection works.
    #[tokio::test]
    async fn peer_cred_check() {
        let (server, path, shutdown) = setup_server();
        let s = shutdown.clone();
        let handle = tokio::spawn(async move { server.run(s).await });

        // Connect as the same user the server is running as.
        let mut stream = connect(&path).await;

        // Send a simple IsRunning request — if the peer-cred check rejected
        // us, the server would have closed the stream and this read would fail.
        send_request(&mut stream, &IpcRequest::IsRunning).await;
        let resp = read_response(&mut stream).await;
        assert!(resp.ok, "same-UID connection should pass peer-cred check");

        shutdown.cancel();
        let _ = handle.await;
    }

    // ── Additional: Bind removes stale socket ──────────────────────────

    #[tokio::test]
    async fn ipc_server_bind_removes_stale_socket() {
        let path = unique_socket_path();
        let app = tauri::test::mock_app();
        let (cmd_tx, _cmd_rx) = mpsc::channel(16);

        // Create a stale file at the socket path
        std::fs::write(&path, b"stale").unwrap();
        assert!(path.exists());

        // Bind should succeed by removing the stale file
        let result = IpcServer::bind(path.clone(), cmd_tx, app.handle().clone());
        assert!(result.is_ok(), "bind should remove stale socket");

        // Clean up
        let _ = std::fs::remove_file(&path);
    }

    // ── Additional: Bind rejects symlink ────────────────────────────────

    #[tokio::test]
    async fn ipc_server_bind_rejects_symlink() {
        let target = unique_socket_path();
        let link = unique_socket_path();

        // Create a regular file and a symlink pointing to it
        std::fs::write(&target, b"target").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let app = tauri::test::mock_app();
        let (cmd_tx, _cmd_rx) = mpsc::channel(16);

        let result = IpcServer::bind(link.clone(), cmd_tx, app.handle().clone());
        assert!(result.is_err(), "bind should reject symlink");
        let err = result.err().unwrap().to_string();
        assert!(
            err.contains("symlink"),
            "Error should mention symlink: {err}"
        );

        // Clean up
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_file(&target);
    }

    #[tokio::test]
    async fn ipc_server_bind_rejects_dangling_symlink() {
        let link = unique_socket_path();

        // Create a symlink pointing to a non-existent target (dangling)
        let target = unique_socket_path();
        assert!(!target.exists(), "target must not exist for dangling test");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(!link.exists(), "dangling symlink must report !exists()");

        let app = tauri::test::mock_app();
        let (cmd_tx, _cmd_rx) = mpsc::channel(16);

        let result = IpcServer::bind(link.clone(), cmd_tx, app.handle().clone());
        assert!(result.is_err(), "bind should reject dangling symlink");
        let err = result.err().unwrap().to_string();
        assert!(
            err.contains("symlink"),
            "Error should mention symlink: {err}"
        );

        // Clean up
        let _ = std::fs::remove_file(&link);
    }

    // ── L1 fix: No duplicate events when relay is active ─────────────────

    /// Verify that when the event relay (as in headless_main) sends events,
    /// clients receive each event exactly once — not twice.
    ///
    /// In production, the headless event relay subscribes to PluginEvents
    /// and forwards them as IpcEvents. If handle_request_with_event also
    /// broadcasts, clients see duplicates.
    #[tokio::test]
    async fn ipc_server_no_duplicate_events_with_relay() {
        let (server, path, shutdown) = setup_server();
        let event_tx = server.event_sender();
        let s = shutdown.clone();
        let handle = tokio::spawn(async move { server.run(s).await });

        let mut stream = connect(&path).await;

        // Simulate the event relay from headless.rs: it also sends Started
        // to the broadcast channel when the actor emits a PluginEvent.
        let relay_tx = event_tx.clone();
        tokio::spawn(async move {
            // Small delay to simulate the relay firing after the service
            // task emits PluginEvent::Started (which happens after init()
            // succeeds, slightly after the command response).
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = relay_tx.send(IpcEvent::Started);
        });

        // Client sends Start
        send_request(
            &mut stream,
            &IpcRequest::Start {
                config: crate::models::StartConfig::default(),
            },
        )
        .await;
        let resp = read_response(&mut stream).await;
        assert!(resp.ok, "Start should succeed");

        // Read first Started event
        let event1 = tokio::time::timeout(Duration::from_millis(500), read_event(&mut stream))
            .await
            .expect("timed out waiting for first Started event");
        assert!(
            matches!(event1, IpcEvent::Started),
            "Expected Started, got {event1:?}"
        );

        // Verify NO second event arrives within a generous window.
        // If handle_request_with_event also broadcasts Started, we'd see a
        // duplicate here.
        let result =
            tokio::time::timeout(Duration::from_millis(200), read_event(&mut stream)).await;
        assert!(
            result.is_err(),
            "should not receive a duplicate Started event when relay is active"
        );

        shutdown.cancel();
        let _ = handle.await;
    }
}
