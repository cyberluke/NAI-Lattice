"""Python NAF/1 client for the NAI Lattice host.

This module is a byte-compatible implementation of the canonical transport used
by ``packages/naf-client-ts`` and the Rust host (``lattice-protocol`` /
``lattice-client``). The protocol source of truth is
``protos/naf/protocol.proto`` in the NAI-Lattice repository.

* wire framing is ``u32le length`` + serialized protobuf ``Frame``;
* the frame carries CRC32C and BLAKE3-256 computed over the frame serialized
  with ``crc32c = 0`` and ``frame_blake3`` empty, then re-serialized with both
  checksums filled in;
* the protobuf codec is a minimal proto3 implementation that writes fields in
  field-number order and omits default values exactly like ``prost``, so the
  host's own re-serialization produces the same checksums.

Only the messages needed by a client are implemented. Unknown fields are
skipped on decode (``prost`` discards them too). The transport is a synchronous
Windows named-pipe handle opened with ``open(pipe, "r+b", buffering=0)``.

The client is an additive transport: it does not enable telemetry, does not
choose a storage backend, and never falls back to a local event store.
"""

from __future__ import annotations

import base64
import binascii
import hmac
import json
import os
import struct
import threading
import time
import uuid
from dataclasses import dataclass, replace
from pathlib import Path
from typing import Any, Callable, Protocol

from ._version import __version__

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

#: ``b"NAF1"`` interpreted as a little-endian u32 (same value as the Rust host).
FRAME_MAGIC = 0x4E414631
MAX_FRAME_BYTES = 128 * 1024 * 1024
NAF_VERSION = 1
KIND_REQUEST = 11
KIND_RESPONSE = 12
KIND_EVENT = 13

DEFAULT_REQUESTED_CAPABILITIES = (
    "event.append",
    "event.append_batch",
    "event.read_range",
    "artifact.put",
    "artifact.open",
    "artifact.verify",
)

#: Product-neutral namespace for events that describe the shared fabric itself
#: (client lifecycle, lane health, transport diagnostics). Downstream products
#: normally use their own dotted namespace for product events.
DEFAULT_EVENT_NAMESPACE = "nai.lattice.fabric"

#: Namespace used by hermetic fixtures and opt-in live smoke tests.
TEST_EVENT_NAMESPACE = "nai.lattice.test"

#: Advisory envelope deadline. The host does not currently enforce it, and a
#: blocking pipe read cannot be interrupted; this only mirrors the TS client.
DEFAULT_TIMEOUT_MS = 15_000
_MAX_INTERLEAVED_FRAMES = 1024

try:  # pragma: no cover - exercised by the explicit blake3 import path
    import blake3 as _blake3_module
except ImportError:  # pragma: no cover - tests pin blake3 into a temp target
    _blake3_module = None


class NafError(Exception):
    """Typed NAF failure.

    ``status``/``error``/``body`` carry the host ``Response`` when the failure
    came from a dispatch; ``code`` is the local transport/protocol class.
    """

    def __init__(
        self,
        message: str,
        *,
        code: str = "NAF_ERROR",
        status: int = 0,
        error: str = "",
        body: bytes = b"",
    ) -> None:
        super().__init__(message)
        self.code = code
        self.status = int(status)
        self.error = str(error)
        self.body = bytes(body)

    def to_dict(self) -> dict[str, Any]:
        return {
            "code": self.code,
            "status": self.status,
            "error": self.error,
            "detail": str(self),
            "bodyLength": len(self.body),
        }


# --------------------------------------------------------------------- CRC32C


def _build_crc32c_table() -> tuple[int, ...]:
    poly = 0x82F63B78  # reflected Castagnoli polynomial
    table: list[int] = []
    for n in range(256):
        c = n
        for _ in range(8):
            c = (c >> 1) ^ poly if c & 1 else c >> 1
        table.append(c & 0xFFFFFFFF)
    return tuple(table)


_CRC32C_TABLE = _build_crc32c_table()


def crc32c(data: bytes | bytearray | memoryview) -> int:
    """CRC-32C (Castagnoli) matching the Rust ``crc32c`` crate."""

    crc = 0xFFFFFFFF
    for byte in data:
        crc = (crc >> 8) ^ _CRC32C_TABLE[(crc ^ byte) & 0xFF]
    return (crc ^ 0xFFFFFFFF) & 0xFFFFFFFF


# ---------------------------------------------------------------------- BLAKE3


def blake3_digest(data: bytes | bytearray | memoryview) -> bytes:
    """32-byte BLAKE3-256 digest via the pinned ``blake3`` package."""

    if _blake3_module is None:
        raise NafError(
            "the 'blake3' package is required for NAF/1 frame integrity",
            code="BLAKE3_MISSING",
        )
    return _blake3_module.blake3(bytes(data)).digest()


# ----------------------------------------------------------- protobuf writer


class _Writer:
    __slots__ = ("_buf",)

    def __init__(self) -> None:
        self._buf = bytearray()

    def finish(self) -> bytes:
        return bytes(self._buf)

    def varint(self, value: int) -> None:
        if value < 0 or value >= 1 << 64:
            raise ValueError(f"varint out of range: {value}")
        while value >= 0x80:
            self._buf.append((value & 0x7F) | 0x80)
            value >>= 7
        self._buf.append(value)

    def tag(self, field: int, wire: int) -> None:
        if field <= 0:
            raise ValueError(f"invalid protobuf field {field}")
        self.varint((field << 3) | wire)

    def u32(self, value: int) -> None:
        self.varint(value & 0xFFFFFFFF)

    def i64(self, value: int) -> None:
        if value < 0:
            value += 1 << 64
        self.varint(value)

    def raw(self, payload: bytes) -> None:
        self.varint(len(payload))
        self._buf.extend(payload)

    def string(self, value: str) -> None:
        self.raw(value.encode("utf-8"))

    def message(self, field: int, payload: bytes) -> None:
        self.tag(field, 2)
        self.raw(payload)


# ----------------------------------------------------------- protobuf reader


class _Reader:
    __slots__ = ("_buf", "_pos")

    def __init__(self, buf: bytes) -> None:
        self._buf = buf
        self._pos = 0

    @property
    def eof(self) -> bool:
        return self._pos >= len(self._buf)

    def varint(self) -> int:
        result = 0
        shift = 0
        for _ in range(10):
            if self._pos >= len(self._buf):
                raise NafError("truncated protobuf varint", code="PROTOCOL_ERROR")
            byte = self._buf[self._pos]
            self._pos += 1
            result |= (byte & 0x7F) << shift
            if not byte & 0x80:
                if result >= 1 << 64:
                    raise NafError("protobuf varint exceeds 64 bits", code="PROTOCOL_ERROR")
                return result
            shift += 7
        raise NafError("protobuf varint is too long", code="PROTOCOL_ERROR")

    def field(self) -> tuple[int, int]:
        tag = self.varint()
        field = tag >> 3
        wire = tag & 7
        if field == 0:
            raise NafError("protobuf field number 0", code="PROTOCOL_ERROR")
        return field, wire

    def raw(self) -> bytes:
        length = self.varint()
        start = self._pos
        end = start + length
        if end > len(self._buf):
            raise NafError("protobuf length-delimited field past end", code="PROTOCOL_ERROR")
        self._pos = end
        return self._buf[start:end]

    def string(self) -> str:
        try:
            return self.raw().decode("utf-8")
        except UnicodeDecodeError as exc:
            raise NafError("protobuf string is not UTF-8", code="PROTOCOL_ERROR") from exc

    def skip(self, wire: int) -> None:
        if wire == 0:
            self.varint()
        elif wire == 1:
            self._pos += 8
        elif wire == 2:
            self._pos += self.varint()
        elif wire == 5:
            self._pos += 4
        else:
            raise NafError(f"unsupported protobuf wire type {wire}", code="PROTOCOL_ERROR")
        if self._pos > len(self._buf):
            raise NafError("protobuf skip past end", code="PROTOCOL_ERROR")


# -------------------------------------------------------------------- messages


@dataclass
class Frame:
    magic: int = FRAME_MAGIC
    format_version: int = 0
    frame_length: int = 0
    runtime_sequence: int = 0
    event_id: str = ""
    protobuf_event: bytes = b""
    crc32c: int = 0
    frame_blake3: bytes = b""


@dataclass
class Request:
    method: str = ""
    args: bytes = b""


@dataclass
class Response:
    status: int = 0
    error: str = ""
    body: bytes = b""


@dataclass
class Envelope:
    request_id: int = 0
    request_id_raw: bytes = b""
    idempotency_key: str = ""
    producer_id: str = ""
    correlation_id: str = ""
    causation_id: str = ""
    deadline_ms: int = 0
    priority: int = 0
    payload_type: str = ""
    payload: bytes = b""
    kind: int | None = None
    request: Request | None = None
    response: Response | None = None


@dataclass(frozen=True)
class Capability:
    name: str = ""
    version: str = ""
    detail: str = ""


@dataclass(frozen=True)
class Health:
    state: str = ""
    detail: str = ""
    lanes: tuple[str, ...] = ()


@dataclass(frozen=True)
class HandshakeRequest:
    client_app_id: str = ""
    client_version: str = ""
    client_pid: int = 0
    naf_min: int = NAF_VERSION
    naf_max: int = NAF_VERSION
    requested_capabilities: tuple[str, ...] = ()
    package_identity: str = ""
    build_sha: str = ""
    nonce: bytes = b""


@dataclass(frozen=True)
class HandshakeResponse:
    runtime_version: str = ""
    runtime_build_sha: str = ""
    selected_naf_version: int = 0
    capabilities: tuple[Capability, ...] = ()
    runtime_instance_id: str = ""
    storage_schema_version: int = 0
    health: Health | None = None


@dataclass(frozen=True)
class Endpoint:
    canonical_pipe: str
    legacy_pipe: str
    source: str
    manifest_path: str = ""


# --------------------------------------------------------------- Frame codec


def encode_frame_message(frame: Frame) -> bytes:
    """Serialize ``Frame`` in prost field order with proto3 default omission."""

    w = _Writer()
    # wire.ts writes magic unconditionally; it is never zero on a real frame.
    w.tag(1, 0)
    w.u32(frame.magic)
    if frame.format_version != 0:
        w.tag(2, 0)
        w.u32(frame.format_version)
    if frame.frame_length != 0:
        w.tag(3, 0)
        w.varint(frame.frame_length)
    if frame.runtime_sequence != 0:
        w.tag(4, 0)
        w.varint(frame.runtime_sequence)
    if frame.event_id:
        w.tag(5, 2)
        w.string(frame.event_id)
    if frame.protobuf_event:
        w.tag(6, 2)
        w.raw(frame.protobuf_event)
    if frame.crc32c != 0:
        w.tag(7, 0)
        w.u32(frame.crc32c)
    if frame.frame_blake3:
        w.tag(8, 2)
        w.raw(frame.frame_blake3)
    return w.finish()


def decode_frame_message(buf: bytes) -> Frame:
    r = _Reader(buf)
    frame = Frame(magic=0, format_version=0, frame_length=0, runtime_sequence=0)
    while not r.eof:
        field, wire = r.field()
        if wire == 0:
            value = r.varint()
            if field == 1:
                frame.magic = value & 0xFFFFFFFF
            elif field == 2:
                frame.format_version = value & 0xFFFFFFFF
            elif field == 3:
                frame.frame_length = value
            elif field == 4:
                frame.runtime_sequence = value
            elif field == 7:
                frame.crc32c = value & 0xFFFFFFFF
        elif wire == 2:
            value = r.raw()
            if field == 5:
                try:
                    frame.event_id = value.decode("utf-8")
                except UnicodeDecodeError as exc:
                    raise NafError(
                        "frame event_id is not UTF-8", code="PROTOCOL_ERROR"
                    ) from exc
            elif field == 6:
                frame.protobuf_event = value
            elif field == 8:
                frame.frame_blake3 = value
        else:
            r.skip(wire)
    return frame


def encode_wire_frame(
    envelope: Envelope,
    *,
    format_version: int = 1,
    runtime_sequence: int = 0,
    event_id: str = "",
) -> bytes:
    """Encode one length-prefixed NAF/1 wire frame with CRC32C + BLAKE3."""

    payload = encode_envelope(envelope)
    frame = Frame(
        magic=FRAME_MAGIC,
        format_version=format_version,
        frame_length=len(payload),
        runtime_sequence=runtime_sequence,
        event_id=event_id,
        protobuf_event=payload,
        crc32c=0,
        frame_blake3=b"",
    )
    canonical = encode_frame_message(frame)
    frame = replace(
        frame,
        crc32c=crc32c(canonical),
        frame_blake3=blake3_digest(canonical),
    )
    encoded = encode_frame_message(frame)
    if len(encoded) > MAX_FRAME_BYTES:
        raise NafError(f"frame too large: {len(encoded)}", code="FRAME_ERROR")
    return struct.pack("<I", len(encoded)) + encoded


def decode_wire_frame(wire: bytes) -> Frame:
    """Decode and integrity-verify one NAF/1 wire frame."""

    if len(wire) < 4:
        raise NafError("truncated wire frame (no length)", code="FRAME_ERROR")
    (length,) = struct.unpack_from("<I", wire, 0)
    if length > MAX_FRAME_BYTES:
        raise NafError(
            f"frame too large: {length} > {MAX_FRAME_BYTES}", code="FRAME_ERROR"
        )
    if len(wire) < 4 + length:
        raise NafError("truncated frame body", code="FRAME_ERROR")
    frame = decode_frame_message(wire[4 : 4 + length])
    if frame.magic != FRAME_MAGIC:
        raise NafError(f"bad frame magic 0x{frame.magic:08X}", code="FRAME_ERROR")
    canonical = encode_frame_message(replace(frame, crc32c=0, frame_blake3=b""))
    if crc32c(canonical) != frame.crc32c:
        raise NafError("crc32c mismatch", code="FRAME_ERROR")
    digest = blake3_digest(canonical)
    if len(frame.frame_blake3) != len(digest) or not hmac.compare_digest(
        digest, frame.frame_blake3
    ):
        raise NafError("frame blake3 mismatch", code="FRAME_ERROR")
    return frame


# ------------------------------------------------------------- Envelope codec


def _encode_request(request: Request) -> bytes:
    w = _Writer()
    if request.method:
        w.tag(1, 2)
        w.string(request.method)
    if request.args:
        w.tag(2, 2)
        w.raw(request.args)
    return w.finish()


def _decode_request(buf: bytes) -> Request:
    r = _Reader(buf)
    request = Request()
    while not r.eof:
        field, wire = r.field()
        if wire == 2:
            value = r.raw()
            if field == 1:
                try:
                    request.method = value.decode("utf-8")
                except UnicodeDecodeError as exc:
                    raise NafError(
                        "request method is not UTF-8", code="PROTOCOL_ERROR"
                    ) from exc
            elif field == 2:
                request.args = value
        else:
            r.skip(wire)
    return request


def _encode_response(response: Response) -> bytes:
    w = _Writer()
    if response.status != 0:
        w.tag(1, 0)
        w.u32(response.status)
    if response.error:
        w.tag(2, 2)
        w.string(response.error)
    if response.body:
        w.tag(3, 2)
        w.raw(response.body)
    return w.finish()


def _decode_response(buf: bytes) -> Response:
    r = _Reader(buf)
    response = Response()
    while not r.eof:
        field, wire = r.field()
        if wire == 0:
            value = r.varint()
            if field == 1:
                response.status = value & 0xFFFFFFFF
        elif wire == 2:
            value = r.raw()
            if field == 2:
                try:
                    response.error = value.decode("utf-8")
                except UnicodeDecodeError as exc:
                    raise NafError(
                        "response error is not UTF-8", code="PROTOCOL_ERROR"
                    ) from exc
            elif field == 3:
                response.body = value
        else:
            r.skip(wire)
    return response


def encode_envelope(envelope: Envelope) -> bytes:
    """Serialize ``Envelope`` with proto3 default omission and oneof presence."""

    w = _Writer()
    if envelope.request_id != 0:
        w.tag(1, 0)
        w.varint(envelope.request_id)
    if envelope.request_id_raw:
        w.tag(2, 2)
        w.raw(envelope.request_id_raw)
    if envelope.idempotency_key:
        w.tag(3, 2)
        w.string(envelope.idempotency_key)
    if envelope.producer_id:
        w.tag(4, 2)
        w.string(envelope.producer_id)
    if envelope.correlation_id:
        w.tag(5, 2)
        w.string(envelope.correlation_id)
    if envelope.causation_id:
        w.tag(6, 2)
        w.string(envelope.causation_id)
    if envelope.deadline_ms != 0:
        w.tag(7, 0)
        w.varint(envelope.deadline_ms)
    if envelope.priority != 0:
        w.tag(8, 0)
        w.u32(envelope.priority)
    if envelope.payload_type:
        w.tag(9, 2)
        w.string(envelope.payload_type)
    if envelope.payload:
        w.tag(10, 2)
        w.raw(envelope.payload)
    if envelope.kind == KIND_REQUEST and envelope.request is not None:
        w.message(11, _encode_request(envelope.request))
    elif envelope.kind == KIND_RESPONSE and envelope.response is not None:
        w.message(12, _encode_response(envelope.response))
    elif envelope.kind == KIND_EVENT:
        raise ValueError("NAF event envelopes are decode-only in this client")
    return w.finish()


def decode_envelope(buf: bytes) -> Envelope:
    r = _Reader(buf)
    envelope = Envelope()
    while not r.eof:
        field, wire = r.field()
        if wire == 0:
            value = r.varint()
            if field == 1:
                envelope.request_id = value
            elif field == 7:
                envelope.deadline_ms = value
            elif field == 8:
                envelope.priority = value & 0xFFFFFFFF
        elif wire == 2:
            value = r.raw()
            if field == 2:
                envelope.request_id_raw = value
            elif field == 3:
                envelope.idempotency_key = _utf8(value, "idempotency_key")
            elif field == 4:
                envelope.producer_id = _utf8(value, "producer_id")
            elif field == 5:
                envelope.correlation_id = _utf8(value, "correlation_id")
            elif field == 6:
                envelope.causation_id = _utf8(value, "causation_id")
            elif field == 9:
                envelope.payload_type = _utf8(value, "payload_type")
            elif field == 10:
                envelope.payload = value
            elif field == 11:
                envelope.kind = KIND_REQUEST
                envelope.request = _decode_request(value)
            elif field == 12:
                envelope.kind = KIND_RESPONSE
                envelope.response = _decode_response(value)
            elif field == 13:
                envelope.kind = KIND_EVENT
        else:
            r.skip(wire)
    return envelope


def _utf8(value: bytes, field: str) -> str:
    try:
        return value.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise NafError(f"envelope {field} is not UTF-8", code="PROTOCOL_ERROR") from exc


# ------------------------------------------------------------ Handshake codec


def encode_handshake_request(handshake: HandshakeRequest) -> bytes:
    w = _Writer()
    if handshake.client_app_id:
        w.tag(1, 2)
        w.string(handshake.client_app_id)
    if handshake.client_version:
        w.tag(2, 2)
        w.string(handshake.client_version)
    # wire.ts writes pid/naf_min/naf_max unconditionally.
    w.tag(3, 0)
    w.i64(handshake.client_pid)
    w.tag(4, 0)
    w.u32(handshake.naf_min)
    w.tag(5, 0)
    w.u32(handshake.naf_max)
    for capability in handshake.requested_capabilities:
        w.tag(6, 2)
        w.string(capability)
    if handshake.package_identity:
        w.tag(7, 2)
        w.string(handshake.package_identity)
    if handshake.build_sha:
        w.tag(8, 2)
        w.string(handshake.build_sha)
    if handshake.nonce:
        w.tag(9, 2)
        w.raw(handshake.nonce)
    return w.finish()


def decode_handshake_request(buf: bytes) -> HandshakeRequest:
    r = _Reader(buf)
    app_id = ""
    version = ""
    pid = 0
    naf_min = 0
    naf_max = 0
    capabilities: list[str] = []
    package_identity = ""
    build_sha = ""
    nonce = b""
    while not r.eof:
        field, wire = r.field()
        if wire == 0:
            value = r.varint()
            if field == 3:
                pid = _to_int64(value)
            elif field == 4:
                naf_min = value & 0xFFFFFFFF
            elif field == 5:
                naf_max = value & 0xFFFFFFFF
        elif wire == 2:
            value = r.raw()
            if field == 1:
                app_id = _utf8(value, "client_app_id")
            elif field == 2:
                version = _utf8(value, "client_version")
            elif field == 6:
                capabilities.append(_utf8(value, "requested_capability"))
            elif field == 7:
                package_identity = _utf8(value, "package_identity")
            elif field == 8:
                build_sha = _utf8(value, "build_sha")
            elif field == 9:
                nonce = value
        else:
            r.skip(wire)
    return HandshakeRequest(
        client_app_id=app_id,
        client_version=version,
        client_pid=pid,
        naf_min=naf_min,
        naf_max=naf_max,
        requested_capabilities=tuple(capabilities),
        package_identity=package_identity,
        build_sha=build_sha,
        nonce=nonce,
    )


def _to_int64(value: int) -> int:
    return value - (1 << 64) if value >= 1 << 63 else value


def _decode_capability(buf: bytes) -> Capability:
    r = _Reader(buf)
    name = ""
    version = ""
    detail = ""
    while not r.eof:
        field, wire = r.field()
        if wire == 2:
            value = r.raw()
            if field == 1:
                name = _utf8(value, "capability.name")
            elif field == 2:
                version = _utf8(value, "capability.version")
            elif field == 3:
                detail = _utf8(value, "capability.detail")
        else:
            r.skip(wire)
    return Capability(name=name, version=version, detail=detail)


def _decode_health(buf: bytes) -> Health:
    r = _Reader(buf)
    state = ""
    detail = ""
    lanes: list[str] = []
    while not r.eof:
        field, wire = r.field()
        if wire == 2:
            value = r.raw()
            if field == 1:
                state = _utf8(value, "health.state")
            elif field == 2:
                detail = _utf8(value, "health.detail")
            elif field == 3:
                lanes.append(_utf8(value, "health.lane"))
        else:
            r.skip(wire)
    return Health(state=state, detail=detail, lanes=tuple(lanes))


def decode_handshake_response(buf: bytes) -> HandshakeResponse:
    r = _Reader(buf)
    runtime_version = ""
    runtime_build_sha = ""
    selected = 0
    capabilities: list[Capability] = []
    instance_id = ""
    storage_schema = 0
    health: Health | None = None
    while not r.eof:
        field, wire = r.field()
        if wire == 0:
            value = r.varint()
            if field == 3:
                selected = value & 0xFFFFFFFF
            elif field == 6:
                storage_schema = value & 0xFFFFFFFF
        elif wire == 2:
            value = r.raw()
            if field == 1:
                runtime_version = _utf8(value, "runtime_version")
            elif field == 2:
                runtime_build_sha = _utf8(value, "runtime_build_sha")
            elif field == 4:
                capabilities.append(_decode_capability(value))
            elif field == 5:
                instance_id = _utf8(value, "runtime_instance_id")
            elif field == 7:
                health = _decode_health(value)
        else:
            r.skip(wire)
    return HandshakeResponse(
        runtime_version=runtime_version,
        runtime_build_sha=runtime_build_sha,
        selected_naf_version=selected,
        capabilities=tuple(capabilities),
        runtime_instance_id=instance_id,
        storage_schema_version=storage_schema,
        health=health,
    )


# ---------------------------------------------------------- endpoint discovery


def default_endpoint_path() -> Path:
    return Path(
        os.environ.get("LOCALAPPDATA", ""),
        "NAI",
        "Lattice",
        "v1",
        "endpoint.json",
    )


def legacy_pipe_name() -> str:
    """Username-derived legacy alias served by the same host singleton."""

    user = os.environ.get("USERNAME", "unknown").lower()
    digest = blake3_digest(user.encode("utf-8")).hex()
    return rf"\\.\pipe\NAI.AmbientFabric.NAF1.{digest[:6]}"


def discover_endpoint(endpoint_path: str | os.PathLike[str] | None = None) -> Endpoint:
    """Resolve the canonical pipe from ``endpoint.json``, else the legacy alias."""

    path = Path(endpoint_path) if endpoint_path is not None else default_endpoint_path()
    manifest: dict[str, Any] | None = None
    try:
        parsed = json.loads(path.read_text(encoding="utf-8"))
        if isinstance(parsed, dict):
            manifest = parsed
    except (OSError, ValueError):
        manifest = None
    if manifest is not None:
        canonical = manifest.get("canonical_pipe")
        if isinstance(canonical, str) and canonical:
            alias = manifest.get("legacy_alias")
            legacy = alias if isinstance(alias, str) and alias else canonical
            return Endpoint(
                canonical_pipe=canonical,
                legacy_pipe=legacy,
                source="manifest",
                manifest_path=str(path),
            )
    fallback = legacy_pipe_name()
    return Endpoint(
        canonical_pipe=fallback,
        legacy_pipe=fallback,
        source="legacy-fallback",
        manifest_path=str(path),
    )


def default_client_version() -> str:
    """Return the installed distribution version (``__version__``)."""

    return __version__


# ------------------------------------------------------------------ transport


class Transport(Protocol):
    def read_exact(self, size: int) -> bytes: ...

    def write_all(self, data: bytes) -> None: ...

    def close(self) -> None: ...


class _PipeTransport:
    """Raw blocking named-pipe handle (``open(pipe, "r+b", buffering=0)``)."""

    def __init__(self, pipe_path: str) -> None:
        self._file = open(pipe_path, "r+b", buffering=0)  # noqa: SIM115 - owned handle

    def read_exact(self, size: int) -> bytes:
        chunks: list[bytes] = []
        remaining = size
        while remaining > 0:
            chunk = self._file.read(remaining)
            if not chunk:
                raise EOFError("NAF pipe closed")
            chunks.append(chunk)
            remaining -= len(chunk)
        return b"".join(chunks)

    def write_all(self, data: bytes) -> None:
        view = memoryview(data)
        while view:
            written = self._file.write(view)
            if not written:
                raise EOFError("NAF pipe write returned 0")
            view = view[written:]

    def close(self) -> None:
        try:
            self._file.close()
        except OSError:
            pass


# --------------------------------------------------------------------- client


class NafClient:
    """Synchronous NAF/1 client over the Lattice named pipe."""

    def __init__(
        self,
        *,
        app_id: str,
        pipe_path: str | None = None,
        endpoint_path: str | os.PathLike[str] | None = None,
        client_version: str | None = None,
        producer_id: str | None = None,
        requested_capabilities: tuple[str, ...] | list[str] | None = None,
        package_identity: str = "",
        build_sha: str = "",
        nonce: bytes = b"",
        timeout_ms: int = DEFAULT_TIMEOUT_MS,
        transport_factory: Callable[[str], Transport] | None = None,
    ) -> None:
        if not isinstance(app_id, str) or not app_id.strip():
            raise ValueError("app_id must be a non-empty string")
        self.app_id = app_id
        self.client_version = client_version or default_client_version()
        self.producer_id = producer_id if producer_id is not None else app_id
        self.requested_capabilities = tuple(
            requested_capabilities or DEFAULT_REQUESTED_CAPABILITIES
        )
        self.package_identity = package_identity
        self.build_sha = build_sha
        self.nonce = bytes(nonce)
        self.timeout_ms = int(timeout_ms)
        self._pipe_override = pipe_path
        self._endpoint_path = endpoint_path
        self._transport_factory = transport_factory or _PipeTransport
        self._lock = threading.RLock()
        self._transport: Transport | None = None
        self._connected = False
        self._closed = False

        self.endpoint: Endpoint | None = None
        self.pipe_path: str | None = None
        self.runtime_version = ""
        self.runtime_build_sha = ""
        self.runtime_instance_id = ""
        self.naf_version: int | None = None
        self.storage_schema_version = 0
        self.capabilities: tuple[Capability, ...] = ()
        self.health: Health | None = None

    @property
    def connected(self) -> bool:
        return self._connected

    # -- lifecycle ---------------------------------------------------------

    def connect(self) -> None:
        with self._lock:
            if self._connected:
                return
            if self._closed:
                raise NafError("NAF client is closed", code="CLOSED")
            if self._pipe_override:
                pipe = self._pipe_override
                self.endpoint = None
            else:
                self.endpoint = discover_endpoint(self._endpoint_path)
                pipe = self.endpoint.canonical_pipe
            try:
                transport = self._transport_factory(pipe)
            except OSError as exc:
                raise NafError(
                    f"cannot open NAF pipe {pipe}: {exc}", code="PIPE_UNAVAILABLE"
                ) from exc
            self._transport = transport
            self.pipe_path = pipe
            try:
                self._handshake()
            except BaseException:
                self._drop_transport()
                raise
            self._connected = True

    def close(self) -> None:
        with self._lock:
            if self._closed:
                return
            self._closed = True
            self._connected = False
            self._drop_transport()

    def __enter__(self) -> "NafClient":
        self.connect()
        return self

    def __exit__(self, *_exc_info: Any) -> None:
        self.close()

    def _drop_transport(self) -> None:
        transport = self._transport
        self._transport = None
        self._connected = False
        if transport is not None:
            try:
                transport.close()
            except OSError:
                pass

    # -- wire I/O ----------------------------------------------------------

    def _handshake(self) -> None:
        request = HandshakeRequest(
            client_app_id=self.app_id,
            client_version=self.client_version,
            client_pid=os.getpid(),
            naf_min=NAF_VERSION,
            naf_max=NAF_VERSION,
            requested_capabilities=self.requested_capabilities,
            package_identity=self.package_identity,
            build_sha=self.build_sha,
            nonce=self.nonce,
        )
        envelope = Envelope(
            producer_id=self.producer_id,
            payload_type="naf.handshake",
            kind=KIND_REQUEST,
            request=Request(
                method="__handshake",
                args=encode_handshake_request(request),
            ),
        )
        self._write(encode_wire_frame(envelope))
        response_envelope = self._read_response_envelope()
        if response_envelope.response is None:
            raise NafError(
                "unexpected handshake envelope", code="HANDSHAKE_REJECTED"
            )
        response = response_envelope.response
        if response.status != 0:
            raise NafError(
                f"handshake rejected: {response.error or response.status}",
                code="HANDSHAKE_REJECTED",
                status=response.status,
                error=response.error,
                body=response.body,
            )
        try:
            handshake = decode_handshake_response(response.body)
        except NafError as exc:
            raise NafError(
                "invalid handshake response body", code="HANDSHAKE_REJECTED"
            ) from exc
        if handshake.selected_naf_version != NAF_VERSION:
            raise NafError(
                f"unsupported NAF version {handshake.selected_naf_version}",
                code="HANDSHAKE_REJECTED",
            )
        self.naf_version = handshake.selected_naf_version
        self.runtime_version = handshake.runtime_version
        self.runtime_build_sha = handshake.runtime_build_sha
        self.runtime_instance_id = handshake.runtime_instance_id
        self.storage_schema_version = handshake.storage_schema_version
        self.capabilities = handshake.capabilities
        self.health = handshake.health

    def _write(self, wire: bytes) -> None:
        transport = self._transport
        if transport is None:
            raise NafError("NAF client is not connected", code="NOT_CONNECTED")
        try:
            transport.write_all(wire)
        except (OSError, EOFError) as exc:
            self._drop_transport()
            raise NafError(f"NAF pipe write failed: {exc}", code="PIPE_CLOSED") from exc

    def _read_frame(self) -> Frame:
        transport = self._transport
        if transport is None:
            raise NafError("NAF client is not connected", code="NOT_CONNECTED")
        try:
            prefix = transport.read_exact(4)
            (length,) = struct.unpack("<I", prefix)
            if length > MAX_FRAME_BYTES:
                raise NafError(
                    f"frame too large: {length} > {MAX_FRAME_BYTES}",
                    code="FRAME_ERROR",
                )
            body = transport.read_exact(length)
        except (OSError, EOFError) as exc:
            self._drop_transport()
            raise NafError(f"NAF pipe read failed: {exc}", code="PIPE_CLOSED") from exc
        try:
            return decode_wire_frame(prefix + body)
        except NafError:
            self._drop_transport()
            raise

    def _read_response_envelope(self) -> Envelope:
        for _ in range(_MAX_INTERLEAVED_FRAMES):
            frame = self._read_frame()
            envelope = decode_envelope(frame.protobuf_event)
            if envelope.kind == KIND_RESPONSE and envelope.response is not None:
                return envelope
            if envelope.kind == KIND_EVENT:
                continue
            raise NafError(
                "unexpected NAF envelope kind", code="PROTOCOL_ERROR"
            )
        raise NafError(
            "too many interleaved NAF frames before a response",
            code="PROTOCOL_ERROR",
        )

    # -- dispatch ----------------------------------------------------------

    def dispatch(
        self,
        method: str,
        args: dict[str, Any] | None = None,
        idempotency_key: str | None = None,
    ) -> Any:
        """Send one request and return the JSON-decoded response body."""

        if not isinstance(method, str) or not method:
            raise ValueError("method must be a non-empty string")
        with self._lock:
            if not self._connected or self._transport is None:
                raise NafError("NAF client is not connected", code="NOT_CONNECTED")
            envelope = Envelope(
                request_id_raw=str(uuid.uuid4()).encode("ascii"),
                idempotency_key=idempotency_key or "",
                producer_id=self.producer_id,
                deadline_ms=int(time.time() * 1000) + self.timeout_ms * 2,
                payload_type="naf.request",
                kind=KIND_REQUEST,
                request=Request(
                    method=method,
                    args=self._encode_args(method, args),
                ),
            )
            self._write(encode_wire_frame(envelope))
            response = self._read_response_envelope().response
            if response is None:  # pragma: no cover - guarded by the reader
                raise NafError("missing NAF response", code="BAD_RESPONSE")
            if response.status != 0:
                raise NafError(
                    f"{method} failed with status {response.status}: "
                    f"{response.error or 'unspecified error'}",
                    code="RPC_ERROR",
                    status=response.status,
                    error=response.error,
                    body=response.body,
                )
            return self._decode_body(method, response.body)

    def _encode_args(self, method: str, args: dict[str, Any] | None) -> bytes:
        try:
            return json.dumps(
                args if args is not None else {},
                ensure_ascii=False,
                allow_nan=False,
                separators=(",", ":"),
            ).encode("utf-8")
        except (TypeError, ValueError) as exc:
            raise NafError(
                f"{method} arguments are not JSON-serializable: {exc}",
                code="BAD_ARGS",
            ) from exc

    def _decode_body(self, method: str, body: bytes) -> Any:
        if not body:
            return {}
        try:
            return json.loads(body.decode("utf-8"))
        except (UnicodeDecodeError, ValueError) as exc:
            raise NafError(
                f"{method} returned a non-JSON body",
                code="BAD_RESPONSE",
                body=body,
            ) from exc

    @staticmethod
    def _as_dict(method: str, value: Any, body: bytes = b"") -> dict[str, Any]:
        if not isinstance(value, dict):
            raise NafError(
                f"{method} returned {type(value).__name__}, expected an object",
                code="BAD_RESPONSE",
                body=body,
            )
        return value

    # -- typed methods -----------------------------------------------------

    def event_append(
        self,
        namespace: str,
        event_type: str,
        *,
        entity_id: str | None = None,
        payload: Any = None,
        correlation_id: str | None = None,
        durability: str = "batched",
        idempotency_key: str | None = None,
        timestamp_utc_ms: int | None = None,
    ) -> dict[str, Any]:
        if not isinstance(namespace, str) or not namespace:
            raise ValueError("namespace must be a non-empty string")
        if not isinstance(event_type, str) or not event_type:
            raise ValueError("event_type must be a non-empty string")
        if durability not in ("batched", "durable"):
            raise ValueError("durability must be 'batched' or 'durable'")
        args: dict[str, Any] = {
            "namespace": namespace,
            "event_type": event_type,
            "durability": durability,
        }
        if entity_id is not None:
            args["entity_id"] = entity_id
        if payload is not None:
            args["payload"] = payload
        if correlation_id is not None:
            args["correlation_id"] = correlation_id
        if timestamp_utc_ms is not None:
            args["timestamp_utc_ms"] = int(timestamp_utc_ms)
        result = self.dispatch("event.append", args, idempotency_key)
        return self._as_dict("event.append", result)

    def event_read_range(
        self,
        *,
        after: int | None = None,
        limit: int | None = None,
        namespace: str | None = None,
        event_type: str | None = None,
        entity_id: str | None = None,
        correlation_id: str | None = None,
        tail: bool | None = None,
    ) -> dict[str, Any]:
        args: dict[str, Any] = {}
        if after is not None:
            args["after"] = int(after)
        if limit is not None:
            args["limit"] = int(limit)
        if namespace is not None:
            args["namespace"] = namespace
        if event_type is not None:
            args["event_type"] = event_type
        if entity_id is not None:
            args["entity_id"] = entity_id
        if correlation_id is not None:
            args["correlation_id"] = correlation_id
        if tail is not None:
            args["tail"] = bool(tail)
        result = self.dispatch("event.read_range", args)
        return self._as_dict("event.read_range", result)

    def artifact_put(
        self,
        data: bytes | bytearray | memoryview,
        media_type: str,
        logical_type: str,
    ) -> dict[str, Any]:
        if not isinstance(data, (bytes, bytearray, memoryview)):
            raise TypeError("data must be bytes-like")
        if not isinstance(media_type, str) or not media_type:
            raise ValueError("media_type must be a non-empty string")
        if not isinstance(logical_type, str) or not logical_type:
            raise ValueError("logical_type must be a non-empty string")
        encoded = base64.b64encode(bytes(data)).decode("ascii")
        result = self.dispatch(
            "artifact.put",
            {
                "data": encoded,
                "media_type": media_type,
                "logical_type": logical_type,
            },
        )
        return self._as_dict("artifact.put", result)

    def artifact_open(self, artifact_uri: str) -> bytes:
        if not isinstance(artifact_uri, str) or not artifact_uri:
            raise ValueError("artifact_uri must be a non-empty string")
        result = self._as_dict(
            "artifact.open", self.dispatch("artifact.open", {"artifact_uri": artifact_uri})
        )
        encoded = result.get("data")
        if not isinstance(encoded, str):
            raise NafError(
                "artifact.open response is missing base64 data",
                code="BAD_RESPONSE",
            )
        try:
            return base64.b64decode(encoded, validate=True)
        except (binascii.Error, ValueError) as exc:
            raise NafError(
                "artifact.open returned invalid base64", code="BAD_RESPONSE"
            ) from exc

    def artifact_verify(self, expected_hash: str) -> bool:
        if not isinstance(expected_hash, str) or not expected_hash:
            raise ValueError("expected_hash must be a non-empty string")
        # Convenience: accept the canonical artifact URI returned by put/open.
        prefix = "artifact://blake3/"
        if expected_hash.startswith(prefix):
            expected_hash = expected_hash[len(prefix) :]
        result = self._as_dict(
            "artifact.verify",
            self.dispatch("artifact.verify", {"expected_hash": expected_hash}),
        )
        verified = result.get("verified")
        if not isinstance(verified, bool):
            raise NafError(
                "artifact.verify response is missing a boolean 'verified'",
                code="BAD_RESPONSE",
            )
        return verified
