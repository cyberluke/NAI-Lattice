//! NAF/1 IPC client (Lattice spec §7, §8).
//!
//! Connects to the per-user named pipe, negotiates the handshake, and
//! dispatches length-prefixed NAF requests. Uses an async transport trait so
//! tests can provide an in-process stream; the production transport is the
//! Windows named pipe (`\\.\pipe\NAI.AmbientFabric.NAF1.<sid-hash>`).

use bytes::BytesMut;
use prost::Message;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use nai_lattice_protocol::pb;
use nai_lattice_protocol::ProtocolError;

pub mod codec;
pub mod transport;

#[cfg(windows)]
pub mod win_pipe;

use crate::codec::{encode_frame, WireFrame};

/// Errors raised by the client.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("handshake failed: {0}")]
    Handshake(String),
    #[error("rpc failure: {0}")]
    Rpc(String),
    #[error("runtime unavailable: {0}")]
    RuntimeUnavailable(String),
}

/// A connected, handshaken NAF/1 client.
pub struct NafClient<S> {
    stream: S,
    buf: BytesMut,
    /// Negotiated NAF version.
    pub naf_version: u32,
    /// Runtime instance id.
    pub runtime_instance_id: String,
}

impl<S> NafClient<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Perform the NAF/1 handshake over an already-connected stream.
    pub async fn connect(mut stream: S, request: pb::HandshakeRequest) -> Result<Self, ClientError> {
        let bytes = codec::encode_handshake_frame(&request)?;
        stream.write_all(&bytes).await?;
        stream.flush().await?;

        let mut read_buf = BytesMut::new();
        let resp: WireFrame = codec::read_frame(&mut stream, &mut read_buf).await?;
        let env: pb::Envelope = prost::Message::decode(resp.payload.as_ref())
            .map_err(|e| ClientError::Handshake(e.to_string()))?;
        let handshake: pb::HandshakeResponse = match env.kind {
            Some(pb::envelope::Kind::Response(r)) => {
                if r.status != 0 {
                    return Err(ClientError::Handshake(format!(
                        "runtime rejected handshake: {} {r:?}",
                        r.status
                    )));
                }
                pb::HandshakeResponse::decode(r.body.as_slice())
                    .map_err(|e| ClientError::Handshake(e.to_string()))?
            }
            _ => return Err(ClientError::Handshake("unexpected handshake envelope".into())),
        };
        if handshake.selected_naf_version < request.naf_min {
            return Err(ClientError::Handshake(format!(
                "runtime selected {} < requested {}",
                handshake.selected_naf_version, request.naf_min
            )));
        }
        match handshake.health.as_ref().map(|h| h.state.as_str()) {
            Some("healthy") | None => {}
            Some(other) => {
                return Err(ClientError::RuntimeUnavailable(format!(
                    "runtime health: {other}"
                )))
            }
        }
        Ok(Self {
            stream,
            buf: BytesMut::new(),
            naf_version: handshake.selected_naf_version,
            runtime_instance_id: handshake.runtime_instance_id.clone(),
        })
    }

    /// Dispatch a typed method (`event.append`, `artifact.put`, ...) with an
    /// optional idempotency key. Returns the raw JSON-encoded response body.
    pub async fn dispatch(
        &mut self,
        method: &str,
        args_json: &[u8],
        idempotency_key: Option<&str>,
    ) -> Result<Vec<u8>, ClientError> {
        let env = pb::Envelope {
            request_id: 0,
            request_id_raw: uuid::Uuid::now_v7().as_bytes().to_vec(),
            idempotency_key: idempotency_key.unwrap_or_default().to_string(),
            producer_id: "nai.os".to_string(),
            correlation_id: String::new(),
            causation_id: String::new(),
            deadline_ms: 0,
            priority: 0,
            payload_type: "naf.request".to_string(),
            payload: Vec::new(),
            kind: Some(pb::envelope::Kind::Request(pb::Request {
                method: method.to_string(),
                args: args_json.to_vec(),
            })),
        };
        let frame = encode_frame(&env)?;
        self.stream.write_all(&frame).await?;
        self.stream.flush().await?;

        let resp: WireFrame = codec::read_frame(&mut self.stream, &mut self.buf).await?;
        let resp_env: pb::Envelope = prost::Message::decode(resp.payload.as_ref())
            .map_err(|e| ClientError::Protocol(ProtocolError::Decode(e)))?;
        match resp_env.kind {
            Some(pb::envelope::Kind::Response(r)) => {
                if r.status != 0 {
                    return Err(ClientError::Rpc(format!("status {}: {}", r.status, r.error)));
                }
                Ok(r.body)
            }
            _ => Err(ClientError::Rpc("unexpected response envelope".into())),
        }
    }
}