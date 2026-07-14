use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, SyncSender, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rewind_domain::{CheckpointId, RunId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::recorder::{Observation, send_until_stopped};

/// Current local control protocol version.
pub const CONTROL_PROTOCOL_VERSION: u16 = 1;

/// Maximum request or response frame, including its terminating newline.
pub const MAX_CONTROL_FRAME_BYTES: usize = 4096;

/// One strictly decoded local control request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlRequest {
    /// Protocol version understood by the sender.
    pub version: u16,
    /// Requested operation.
    pub command: ControlCommand,
}

impl ControlRequest {
    /// Constructs a manual checkpoint request with an optional nonempty label.
    #[must_use]
    pub fn mark(label: Option<String>) -> Self {
        Self {
            version: CONTROL_PROTOCOL_VERSION,
            command: ControlCommand::Mark { label },
        }
    }
}

/// Version-one local control operations.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ControlCommand {
    /// Commit an authoritative manual checkpoint.
    Mark {
        /// Optional user-facing label. Empty labels are rejected.
        label: Option<String>,
    },
}

/// Failure to decode a bounded, versioned control frame.
#[derive(Debug, Error)]
pub enum ControlDecodeError {
    /// No request bytes were supplied.
    #[error("control frame is empty")]
    Empty,
    /// The request exceeded the fixed local protocol bound.
    #[error("control frame is {actual} bytes; maximum is {maximum}")]
    TooLarge {
        /// Received byte length.
        actual: usize,
        /// Protocol byte ceiling.
        maximum: usize,
    },
    /// The request was not strict JSON for the current wire type.
    #[error("control frame is malformed: {0}")]
    Json(#[from] serde_json::Error),
    /// The sender requested a protocol version this binary does not understand.
    #[error("unsupported control protocol version {found}; supported version is {supported}")]
    UnsupportedVersion {
        /// Received version.
        found: u16,
        /// Current version.
        supported: u16,
    },
    /// A marker label was present but empty.
    #[error("marker label must be absent or nonempty")]
    EmptyLabel,
}

/// Successful evidence returned by the active recorder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MarkOutcome {
    /// Active run that accepted the marker.
    pub run_id: RunId,
    /// Newly committed manual checkpoint.
    pub checkpoint_id: CheckpointId,
}

/// Failure to contact or receive a valid response from an active recorder.
#[derive(Debug, Error)]
pub enum ControlClientError {
    /// The request itself violated the protocol.
    #[error("invalid marker request: {0}")]
    Encode(#[from] ControlDecodeError),
    /// Connecting, sending, or receiving failed.
    #[error("control socket {operation} failed for {path}: {source}")]
    Io {
        /// Socket operation.
        operation: &'static str,
        /// Endpoint path.
        path: PathBuf,
        /// Underlying failure.
        #[source]
        source: io::Error,
    },
    /// The recorder rejected the request without committing a checkpoint.
    #[error("active recorder rejected marker: {0}")]
    Rejected(String),
    /// A nominally successful response omitted required durable identities.
    #[error("active recorder returned an incomplete response")]
    IncompleteResponse,
}

/// Strictly decodes one newline-terminated or bare JSON control frame.
pub fn decode_control_frame(bytes: &[u8]) -> Result<ControlRequest, ControlDecodeError> {
    if bytes.is_empty() {
        return Err(ControlDecodeError::Empty);
    }
    if bytes.len() > MAX_CONTROL_FRAME_BYTES {
        return Err(ControlDecodeError::TooLarge {
            actual: bytes.len(),
            maximum: MAX_CONTROL_FRAME_BYTES,
        });
    }
    let body = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    if body.is_empty() {
        return Err(ControlDecodeError::Empty);
    }
    let request: ControlRequest = serde_json::from_slice(body)?;
    validate_request(&request)?;
    Ok(request)
}

/// Encodes one deterministic newline-terminated control frame.
pub fn encode_control_frame(request: &ControlRequest) -> Result<Vec<u8>, ControlDecodeError> {
    validate_request(request)?;
    let mut bytes = serde_json::to_vec(request)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_CONTROL_FRAME_BYTES {
        return Err(ControlDecodeError::TooLarge {
            actual: bytes.len(),
            maximum: MAX_CONTROL_FRAME_BYTES,
        });
    }
    Ok(bytes)
}

/// Returns the single active-recorder endpoint beneath a store.
#[must_use]
pub fn control_socket_path(store_root: impl AsRef<Path>) -> PathBuf {
    store_root.as_ref().join("control/active.sock")
}

/// Requests a manual checkpoint from an active recorder and waits for commit.
pub fn request_marker(
    socket_path: impl AsRef<Path>,
    label: Option<String>,
) -> Result<MarkOutcome, ControlClientError> {
    let path = socket_path.as_ref();
    let frame = encode_control_frame(&ControlRequest::mark(label))?;
    let mut stream = UnixStream::connect(path).map_err(|source| ControlClientError::Io {
        operation: "connect",
        path: path.to_path_buf(),
        source,
    })?;
    stream
        .write_all(&frame)
        .map_err(|source| ControlClientError::Io {
            operation: "send request",
            path: path.to_path_buf(),
            source,
        })?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(|source| ControlClientError::Io {
            operation: "finish request",
            path: path.to_path_buf(),
            source,
        })?;
    let response = read_wire_frame(&mut stream).map_err(|source| ControlClientError::Io {
        operation: "read response",
        path: path.to_path_buf(),
        source,
    })?;
    let response: ControlResponse =
        serde_json::from_slice(response.strip_suffix(b"\n").unwrap_or(&response))
            .map_err(ControlDecodeError::Json)?;
    if response.version != CONTROL_PROTOCOL_VERSION {
        return Err(ControlDecodeError::UnsupportedVersion {
            found: response.version,
            supported: CONTROL_PROTOCOL_VERSION,
        }
        .into());
    }
    if !response.accepted {
        return Err(ControlClientError::Rejected(
            response
                .message
                .unwrap_or_else(|| "request was not accepted".to_owned()),
        ));
    }
    match (response.run_id, response.checkpoint_id) {
        (Some(run_id), Some(checkpoint_id)) => Ok(MarkOutcome {
            run_id,
            checkpoint_id,
        }),
        _ => Err(ControlClientError::IncompleteResponse),
    }
}

fn validate_request(request: &ControlRequest) -> Result<(), ControlDecodeError> {
    if request.version != CONTROL_PROTOCOL_VERSION {
        return Err(ControlDecodeError::UnsupportedVersion {
            found: request.version,
            supported: CONTROL_PROTOCOL_VERSION,
        });
    }
    match &request.command {
        ControlCommand::Mark { label } if label.as_deref() == Some("") => {
            Err(ControlDecodeError::EmptyLabel)
        }
        ControlCommand::Mark { .. } => Ok(()),
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlResponse {
    version: u16,
    accepted: bool,
    run_id: Option<RunId>,
    checkpoint_id: Option<CheckpointId>,
    message: Option<String>,
}

impl ControlResponse {
    fn accepted(run_id: RunId, checkpoint_id: CheckpointId) -> Self {
        Self {
            version: CONTROL_PROTOCOL_VERSION,
            accepted: true,
            run_id: Some(run_id),
            checkpoint_id: Some(checkpoint_id),
            message: None,
        }
    }

    fn rejected(run_id: RunId, message: impl Into<String>) -> Self {
        Self {
            version: CONTROL_PROTOCOL_VERSION,
            accepted: false,
            run_id: Some(run_id),
            checkpoint_id: None,
            message: Some(message.into()),
        }
    }
}

pub(crate) struct ControlServer {
    socket_path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ControlServer {
    pub(crate) fn start(
        store_root: &Path,
        run_id: RunId,
        sender: SyncSender<Observation>,
    ) -> Result<Self, String> {
        let socket_path = control_socket_path(store_root);
        let directory = socket_path
            .parent()
            .ok_or_else(|| "control socket has no parent directory".to_owned())?;
        rewind_platform::create_private_dir(directory).map_err(|error| error.to_string())?;
        remove_stale_socket(&socket_path).map_err(|error| error.to_string())?;
        let listener = UnixListener::bind(&socket_path).map_err(|error| error.to_string())?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))
            .map_err(|error| error.to_string())?;
        listener
            .set_nonblocking(true)
            .map_err(|error| error.to_string())?;

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("rewind-control".to_owned())
            .spawn(move || serve(listener, run_id, sender, &thread_stop))
            .map_err(|error| error.to_string())?;
        Ok(Self {
            socket_path,
            stop,
            thread: Some(thread),
        })
    }

    pub(crate) fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    pub(crate) fn stop(&mut self) -> Result<(), &'static str> {
        self.request_stop();
        if let Some(thread) = self.thread.take() {
            thread.join().map_err(|_| "control")?;
        }
        match fs::remove_file(&self.socket_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(_) => Ok(()),
        }
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn serve(
    listener: UnixListener,
    run_id: RunId,
    sender: SyncSender<Observation>,
    stop: &AtomicBool,
) {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let response = handle_connection(&mut stream, run_id, &sender, stop);
                if let Ok(mut bytes) = serde_json::to_vec(&response) {
                    bytes.push(b'\n');
                    let _ = stream.write_all(&bytes);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                let _ = send_until_stopped(
                    &sender,
                    Observation::ProducerFailed {
                        producer: "control socket",
                        message: error.to_string(),
                    },
                    stop,
                );
                break;
            }
        }
    }
}

fn handle_connection(
    stream: &mut UnixStream,
    run_id: RunId,
    sender: &SyncSender<Observation>,
    stop: &AtomicBool,
) -> ControlResponse {
    let frame = match read_wire_frame(stream) {
        Ok(frame) => frame,
        Err(error) => return ControlResponse::rejected(run_id, error.to_string()),
    };
    let request = match decode_control_frame(&frame) {
        Ok(request) => request,
        Err(error) => return ControlResponse::rejected(run_id, error.to_string()),
    };
    match request.command {
        ControlCommand::Mark { label } => {
            let (reply_sender, reply_receiver) = sync_channel(1);
            if !send_until_stopped(
                sender,
                Observation::Marker {
                    label,
                    reply: reply_sender,
                },
                stop,
            ) {
                return ControlResponse::rejected(run_id, "recorder is shutting down");
            }
            loop {
                match reply_receiver.recv_timeout(Duration::from_millis(100)) {
                    Ok(Ok(checkpoint_id)) => {
                        return ControlResponse::accepted(run_id, checkpoint_id);
                    }
                    Ok(Err(message)) => return ControlResponse::rejected(run_id, message),
                    Err(RecvTimeoutError::Timeout) if !stop.load(Ordering::Acquire) => {}
                    Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                        return ControlResponse::rejected(run_id, "recorder is shutting down");
                    }
                }
            }
        }
    }
}

fn read_wire_frame(stream: &mut UnixStream) -> io::Result<Vec<u8>> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut frame = Vec::with_capacity(256);
    let mut chunk = [0_u8; 512];
    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        let remaining = MAX_CONTROL_FRAME_BYTES.saturating_add(1) - frame.len();
        frame.extend_from_slice(&chunk[..read.min(remaining)]);
        if frame.len() > MAX_CONTROL_FRAME_BYTES || frame.last() == Some(&b'\n') {
            break;
        }
    }
    if frame.len() > MAX_CONTROL_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control frame exceeds 4096 bytes",
        ));
    }
    Ok(frame)
}

fn remove_stale_socket(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(path),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "refusing to replace a non-socket control path",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_is_strict_versioned_and_bounded() {
        let request = ControlRequest::mark(Some("before refactor".to_owned()));
        let encoded = encode_control_frame(&request).unwrap();
        assert_eq!(decode_control_frame(&encoded).unwrap(), request);
        assert!(matches!(
            decode_control_frame(&vec![b'x'; MAX_CONTROL_FRAME_BYTES + 1]),
            Err(ControlDecodeError::TooLarge { .. })
        ));
        assert!(
            decode_control_frame(
                br#"{"version":1,"command":{"type":"mark","label":"","extra":1}}"#
            )
            .is_err()
        );
        assert!(matches!(
            decode_control_frame(br#"{"version":2,"command":{"type":"mark","label":null}}"#),
            Err(ControlDecodeError::UnsupportedVersion { .. })
        ));
    }
}
