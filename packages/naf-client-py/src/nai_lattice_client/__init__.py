"""``nai-lattice-client`` — generic Python NAF/1 client for NAI-Lattice.

The protocol source of truth is ``protos/naf/protocol.proto``. The transport is
the per-user Windows named pipe published by ``nai-fabric-host`` through
``%LOCALAPPDATA%\\NAI\\Lattice\\v1\\endpoint.json``.

This package is product-neutral: it has no default application identity and no
product-specific event namespace. Callers must pass ``app_id`` explicitly.
"""

from __future__ import annotations

from ._version import __version__
from .client import (
    DEFAULT_EVENT_NAMESPACE,
    DEFAULT_REQUESTED_CAPABILITIES,
    DEFAULT_TIMEOUT_MS,
    FRAME_MAGIC,
    KIND_EVENT,
    KIND_REQUEST,
    KIND_RESPONSE,
    MAX_FRAME_BYTES,
    NAF_VERSION,
    TEST_EVENT_NAMESPACE,
    Capability,
    Endpoint,
    Envelope,
    Frame,
    HandshakeRequest,
    HandshakeResponse,
    Health,
    NafClient,
    NafError,
    Request,
    Response,
    Transport,
    blake3_digest,
    crc32c,
    decode_envelope,
    decode_frame_message,
    decode_handshake_request,
    decode_handshake_response,
    decode_wire_frame,
    default_client_version,
    default_endpoint_path,
    discover_endpoint,
    encode_envelope,
    encode_frame_message,
    encode_handshake_request,
    encode_wire_frame,
    legacy_pipe_name,
)

__all__ = [
    "DEFAULT_EVENT_NAMESPACE",
    "DEFAULT_REQUESTED_CAPABILITIES",
    "DEFAULT_TIMEOUT_MS",
    "FRAME_MAGIC",
    "KIND_EVENT",
    "KIND_REQUEST",
    "KIND_RESPONSE",
    "MAX_FRAME_BYTES",
    "NAF_VERSION",
    "NafClient",
    "NafError",
    "TEST_EVENT_NAMESPACE",
    "Capability",
    "Endpoint",
    "Envelope",
    "Frame",
    "HandshakeRequest",
    "HandshakeResponse",
    "Health",
    "Request",
    "Response",
    "Transport",
    "__version__",
    "blake3_digest",
    "crc32c",
    "decode_envelope",
    "decode_frame_message",
    "decode_handshake_request",
    "decode_handshake_response",
    "decode_wire_frame",
    "default_client_version",
    "default_endpoint_path",
    "discover_endpoint",
    "encode_envelope",
    "encode_frame_message",
    "encode_handshake_request",
    "encode_wire_frame",
    "legacy_pipe_name",
]
