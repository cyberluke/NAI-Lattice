"""Tests for the generic Python NAF/1 client.

Hermetic by default: framing, protobuf encoding, endpoint discovery, and client
request shapes run against in-memory transports. The optional live smoke runs
only when ``NAI_LATTICE_LIVE=1`` and talks to the running Lattice host.

The canonical TS->Rust handshake golden below is copied from
``crates/lattice-protocol/tests/ts_handshake_golden.rs``; decoding it and
re-encoding it byte-for-byte proves Python<->TS/Rust wire compatibility without
a live host.
"""

from __future__ import annotations

import base64
import json
import os
import struct
import uuid
from dataclasses import replace
from pathlib import Path

import pytest

blake3_module = pytest.importorskip("blake3")

import nai_lattice_client as naf  # noqa: E402

_TS_HANDSHAKE_GOLDEN_HEX = (
    "e300000008b18c85f204100118ad0132ad01122461616161616161612d626262622d636363632d646464642d656565656565656565656565"
    "220a736d6f6b652e746573744a0d6e61662e68616e647368616b655a6a0a0b5f5f68616e647368616b65125b0a046b696c6f1205302e312e30"
    "18d20920012801320c6576656e742e617070656e643214676173746f776e2e61646d696e2e6865616c74683a19406e61692d6c6174746963652f63"
    "6c69656e7440302e312e304201784a0301020338bd9896e90242201e7470e7f7fc63dec48a6234107839b6beea2e0837162065d2800c9e814b11f7"
)


def _field_bytes(field: int, payload: bytes) -> bytes:
    assert 0 < field < 16 and len(payload) < 128
    return bytes([(field << 3) | 2, len(payload)]) + payload


def _varint_field(field: int, value: int) -> bytes:
    assert 0 < field < 16 and 0 <= value < 128
    return bytes([(field << 3) | 0, value])


_CAP_EVENT = _field_bytes(1, b"event.append") + _field_bytes(2, b"1")
_CAP_ARTIFACT = _field_bytes(1, b"artifact.put") + _field_bytes(2, b"1")
_HEALTH = _field_bytes(1, b"healthy") + _field_bytes(3, b"fabric")
_HANDSHAKE_RESPONSE_BODY = (
    _field_bytes(1, b"0.1.0")
    + _field_bytes(2, b"dev")
    + _varint_field(3, 1)
    + _field_bytes(4, _CAP_EVENT)
    + _field_bytes(4, _CAP_ARTIFACT)
    + _field_bytes(5, b"inst-1")
    + _varint_field(6, 1)
    + _field_bytes(7, _HEALTH)
)


class FakeTransport:
    """In-memory NAF host: answers the handshake and typed test methods."""

    def __init__(self, handshake_status: int = 0, handshake_error: str = "") -> None:
        self._incoming = bytearray()
        self.sent: list[naf.Envelope] = []
        self.closed = False
        self._handshake_status = handshake_status
        self._handshake_error = handshake_error

    def write_all(self, data: bytes) -> None:
        frame = naf.decode_wire_frame(data)
        envelope = naf.decode_envelope(frame.protobuf_event)
        self.sent.append(envelope)
        method = envelope.request.method if envelope.request is not None else ""
        status = 0
        error = ""
        if method == "__handshake":
            status = self._handshake_status
            error = self._handshake_error
            body = _HANDSHAKE_RESPONSE_BODY
        elif method == "fail":
            status, error, body = 409, "conflict", b'{"reason":"conflict"}'
        else:
            args = json.loads(envelope.request.args.decode("utf-8"))
            if method == "event.append":
                body = json.dumps({"event_id": "evt-1", "duplicated": False}).encode()
            elif method == "event.read_range":
                body = json.dumps(
                    {
                        "events": [
                            {
                                "event_id": "evt-1",
                                "namespace": args.get("namespace"),
                            }
                        ],
                        "next_after": 1,
                    }
                ).encode()
            elif method == "artifact.put":
                raw = base64.b64decode(args["data"])
                digest = blake3_module.blake3(raw).hexdigest()
                body = json.dumps(
                    {
                        "artifact_uri": f"artifact://blake3/{digest}",
                        "deduplicated": False,
                    }
                ).encode()
            elif method == "artifact.open":
                body = json.dumps(
                    {
                        "artifact_uri": args["artifact_uri"],
                        "data": base64.b64encode(b"hello").decode("ascii"),
                        "size": 5,
                    }
                ).encode()
            elif method == "artifact.verify":
                body = json.dumps(
                    {
                        "verified": args["expected_hash"]
                        == blake3_module.blake3(b"hello").hexdigest()
                    }
                ).encode()
            else:
                body = b"{}"
        response = naf.Envelope(
            kind=naf.KIND_RESPONSE,
            response=naf.Response(status=status, error=error, body=body),
        )
        self._incoming.extend(naf.encode_wire_frame(response))

    def read_exact(self, size: int) -> bytes:
        if len(self._incoming) < size:
            raise EOFError("fake transport underflow")
        chunk = bytes(self._incoming[:size])
        del self._incoming[:size]
        return chunk

    def close(self) -> None:
        self.closed = True


# ------------------------------------------------------------------ primitives


def test_crc32c_known_vector() -> None:
    assert naf.crc32c(b"123456789") == 0xE3069283


def test_blake3_known_vectors() -> None:
    assert blake3_module.blake3(b"").hexdigest() == (
        "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
    )
    assert blake3_module.blake3(b"abc").hexdigest() == (
        "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
    )
    assert naf.blake3_digest(b"abc") == blake3_module.blake3(b"abc").digest()


def test_proto3_defaults_are_omitted() -> None:
    # No set fields, no oneof -> empty message (prost behavior).
    assert naf.encode_envelope(naf.Envelope()) == b""
    # A minimal Frame is magic only; all other defaults disappear.
    assert naf.encode_frame_message(naf.Frame(magic=naf.FRAME_MAGIC)) == (
        b"\x08\xb1\x8c\x85\xf2\x04"
    )
    # An envelope with only a request keeps field 11 (oneof presence).
    envelope = naf.Envelope(
        kind=naf.KIND_REQUEST,
        request=naf.Request(method="x"),
    )
    encoded = naf.encode_envelope(envelope)
    assert encoded[0] == (11 << 3) | 2
    assert naf.decode_envelope(encoded) == envelope


# ---------------------------------------------------------------- frame codec


def test_frame_roundtrip_and_stable_reencode() -> None:
    envelope = naf.Envelope(
        request_id_raw=b"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
        idempotency_key="idem-001",
        producer_id="nai-lattice-client-tests",
        payload_type="naf.request",
        kind=naf.KIND_REQUEST,
        request=naf.Request(
            method="event.append",
            args=b'{"namespace":"nai.lattice.test"}',
        ),
    )
    wire = naf.encode_wire_frame(envelope)
    frame = naf.decode_wire_frame(wire)
    assert frame.magic == naf.FRAME_MAGIC
    assert frame.format_version == 1
    assert frame.runtime_sequence == 0
    assert frame.event_id == ""
    assert frame.frame_length == len(frame.protobuf_event)
    assert frame.crc32c != 0
    assert len(frame.frame_blake3) == 32
    decoded = naf.decode_envelope(frame.protobuf_event)
    assert decoded == envelope
    # The canonical body with checksums zeroed must recompute identically.
    canonical = naf.encode_frame_message(
        replace(frame, crc32c=0, frame_blake3=b"")
    )
    assert naf.crc32c(canonical) == frame.crc32c
    assert naf.blake3_digest(canonical) == frame.frame_blake3
    assert naf.encode_wire_frame(decoded) == wire


def test_ts_handshake_golden_decodes_and_reencodes() -> None:
    golden = bytes.fromhex(_TS_HANDSHAKE_GOLDEN_HEX)
    frame = naf.decode_wire_frame(golden)
    envelope = naf.decode_envelope(frame.protobuf_event)
    assert envelope.producer_id == "smoke.test"
    assert envelope.payload_type == "naf.handshake"
    assert envelope.kind == naf.KIND_REQUEST
    assert envelope.request is not None
    assert envelope.request.method == "__handshake"

    handshake = naf.decode_handshake_request(envelope.request.args)
    assert handshake.client_app_id == "kilo"
    assert handshake.client_version == "0.1.0"
    assert handshake.client_pid == 1234
    assert handshake.naf_min == 1
    assert handshake.naf_max == 1
    assert handshake.requested_capabilities == (
        "event.append",
        "gastown.admin.health",
    )
    assert handshake.package_identity == "@nai-lattice/client@0.1.0"
    assert handshake.build_sha == "x"
    assert handshake.nonce == b"\x01\x02\x03"
    assert naf.encode_handshake_request(handshake) == envelope.request.args

    # Byte-identical re-encode proves CRC/BLAKE3/protobuf field fidelity.
    assert naf.encode_wire_frame(envelope) == golden


def test_handshake_response_decoding() -> None:
    response = naf.decode_handshake_response(_HANDSHAKE_RESPONSE_BODY)
    assert response.runtime_version == "0.1.0"
    assert response.runtime_build_sha == "dev"
    assert response.selected_naf_version == 1
    assert [capability.name for capability in response.capabilities] == [
        "event.append",
        "artifact.put",
    ]
    assert response.runtime_instance_id == "inst-1"
    assert response.storage_schema_version == 1
    assert response.health is not None
    assert response.health.state == "healthy"
    assert response.health.lanes == ("fabric",)


def test_bad_magic_is_rejected() -> None:
    frame = naf.decode_wire_frame(bytes.fromhex(_TS_HANDSHAKE_GOLDEN_HEX))
    body = naf.encode_frame_message(replace(frame, magic=0x12345678))
    wire = struct.pack("<I", len(body)) + body
    with pytest.raises(naf.NafError, match="magic"):
        naf.decode_wire_frame(wire)


def test_crc32c_mismatch_is_rejected() -> None:
    frame = naf.decode_wire_frame(bytes.fromhex(_TS_HANDSHAKE_GOLDEN_HEX))
    tampered = replace(frame, protobuf_event=frame.protobuf_event + b"\x00")
    body = naf.encode_frame_message(tampered)
    wire = struct.pack("<I", len(body)) + body
    with pytest.raises(naf.NafError, match="crc32c"):
        naf.decode_wire_frame(wire)


def test_blake3_mismatch_is_rejected() -> None:
    frame = naf.decode_wire_frame(bytes.fromhex(_TS_HANDSHAKE_GOLDEN_HEX))
    canonical = naf.encode_frame_message(replace(frame, crc32c=0, frame_blake3=b""))
    tampered = replace(
        frame,
        crc32c=naf.crc32c(canonical),
        frame_blake3=b"\x00" * 32,
    )
    body = naf.encode_frame_message(tampered)
    wire = struct.pack("<I", len(body)) + body
    with pytest.raises(naf.NafError, match="blake3"):
        naf.decode_wire_frame(wire)


# ----------------------------------------------------------- endpoint discovery


def test_endpoint_discovery_from_manifest(tmp_path: Path) -> None:
    manifest_path = tmp_path / "endpoint.json"
    manifest_path.write_text(
        json.dumps(
            {
                "canonical_pipe": r"\\.\pipe\NAI.AmbientFabric.NAF1.canonical",
                "legacy_alias": r"\\.\pipe\NAI.AmbientFabric.NAF1.legacy",
                "pid": 1234,
            }
        ),
        encoding="utf-8",
    )
    endpoint = naf.discover_endpoint(manifest_path)
    assert endpoint == naf.Endpoint(
        canonical_pipe=r"\\.\pipe\NAI.AmbientFabric.NAF1.canonical",
        legacy_pipe=r"\\.\pipe\NAI.AmbientFabric.NAF1.legacy",
        source="manifest",
        manifest_path=str(manifest_path),
    )


def test_endpoint_fallback_uses_lowercased_username(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("LOCALAPPDATA", str(tmp_path))
    monkeypatch.setenv("USERNAME", "SomeUser")
    endpoint = naf.discover_endpoint()
    digest = blake3_module.blake3(b"someuser").hexdigest()[:6]
    expected = rf"\\.\pipe\NAI.AmbientFabric.NAF1.{digest}"
    assert endpoint.source == "legacy-fallback"
    assert endpoint.canonical_pipe == expected
    assert endpoint.legacy_pipe == expected


def test_default_client_version_matches_package_version() -> None:
    assert naf.default_client_version() == naf.__version__ == "0.2.1"


def test_generic_namespace_constants() -> None:
    assert naf.DEFAULT_EVENT_NAMESPACE == "nai.lattice.fabric"
    assert naf.TEST_EVENT_NAMESPACE == "nai.lattice.test"


def test_app_id_is_required_and_producer_defaults_to_it() -> None:
    with pytest.raises(TypeError):
        naf.NafClient()  # type: ignore[call-arg]
    with pytest.raises(ValueError):
        naf.NafClient(app_id="   ")
    client = naf.NafClient(app_id="nai-lattice-client-tests")
    assert client.app_id == "nai-lattice-client-tests"
    assert client.producer_id == "nai-lattice-client-tests"
    explicit = naf.NafClient(
        app_id="nai-lattice-client-tests",
        producer_id="nai-lattice-client-tests.worker",
    )
    assert explicit.producer_id == "nai-lattice-client-tests.worker"


# ------------------------------------------------------------------ client API


def test_client_request_shapes_and_typed_errors() -> None:
    transport = FakeTransport()
    opened: list[str] = []

    def factory(path: str) -> FakeTransport:
        opened.append(path)
        return transport

    client = naf.NafClient(
        app_id="nai-lattice-client-tests",
        pipe_path=r"\\.\pipe\fake",
        transport_factory=factory,
    )
    client.connect()
    assert opened == [r"\\.\pipe\fake"]
    assert client.connected
    assert client.naf_version == 1
    assert client.runtime_instance_id == "inst-1"
    assert [capability.name for capability in client.capabilities] == [
        "event.append",
        "artifact.put",
    ]

    handshake_envelope = transport.sent[0]
    assert handshake_envelope.payload_type == "naf.handshake"
    assert handshake_envelope.producer_id == "nai-lattice-client-tests"
    assert handshake_envelope.request is not None
    assert handshake_envelope.request.method == "__handshake"
    handshake = naf.decode_handshake_request(handshake_envelope.request.args)
    assert handshake.client_app_id == "nai-lattice-client-tests"
    assert handshake.client_version == naf.default_client_version()
    assert handshake.naf_min == handshake.naf_max == 1
    assert handshake.requested_capabilities == naf.DEFAULT_REQUESTED_CAPABILITIES

    result = client.event_append(
        naf.TEST_EVENT_NAMESPACE,
        "naf.smoke",
        payload={"n": 1},
        correlation_id="corr-1",
        idempotency_key="idem-1",
    )
    assert result == {"event_id": "evt-1", "duplicated": False}
    event_envelope = transport.sent[-1]
    assert event_envelope.idempotency_key == "idem-1"
    assert event_envelope.payload_type == "naf.request"
    assert event_envelope.request is not None
    assert json.loads(event_envelope.request.args) == {
        "namespace": "nai.lattice.test",
        "event_type": "naf.smoke",
        "durability": "batched",
        "payload": {"n": 1},
        "correlation_id": "corr-1",
    }

    read = client.event_read_range(namespace=naf.TEST_EVENT_NAMESPACE, limit=5)
    assert read["next_after"] == 1
    assert read["events"][0]["event_id"] == "evt-1"
    read_envelope = transport.sent[-1]
    assert read_envelope.request is not None
    assert json.loads(read_envelope.request.args) == {
        "namespace": "nai.lattice.test",
        "limit": 5,
    }

    put = client.artifact_put(b"hello", "text/plain", "nai.lattice.test.artifact")
    digest = blake3_module.blake3(b"hello").hexdigest()
    assert put == {
        "artifact_uri": f"artifact://blake3/{digest}",
        "deduplicated": False,
    }
    put_envelope = transport.sent[-1]
    assert put_envelope.request is not None
    put_args = json.loads(put_envelope.request.args)
    assert put_args["data"] == base64.b64encode(b"hello").decode("ascii")
    assert put_args["media_type"] == "text/plain"
    assert put_args["logical_type"] == "nai.lattice.test.artifact"

    assert client.artifact_open(put["artifact_uri"]) == b"hello"
    open_envelope = transport.sent[-1]
    assert open_envelope.request is not None
    assert json.loads(open_envelope.request.args)["artifact_uri"] == put["artifact_uri"]

    # Pass the canonical URI: the client normalizes it to the hex hash.
    assert client.artifact_verify(put["artifact_uri"]) is True
    verify_envelope = transport.sent[-1]
    assert verify_envelope.request is not None
    assert json.loads(verify_envelope.request.args)["expected_hash"] == digest

    with pytest.raises(naf.NafError) as rejected:
        client.dispatch("fail", {})
    assert rejected.value.code == "RPC_ERROR"
    assert rejected.value.status == 409
    assert rejected.value.error == "conflict"
    assert rejected.value.body == b'{"reason":"conflict"}'

    client.close()
    assert transport.closed
    assert client.connected is False


def test_handshake_rejection_is_typed() -> None:
    transport = FakeTransport(
        handshake_status=400,
        handshake_error="capability not offered: event.append",
    )
    client = naf.NafClient(
        app_id="nai-lattice-client-tests",
        pipe_path=r"\\.\pipe\fake",
        transport_factory=lambda _path: transport,
    )
    with pytest.raises(naf.NafError) as rejected:
        client.connect()
    assert rejected.value.code == "HANDSHAKE_REJECTED"
    assert rejected.value.status == 400
    assert "capability not offered" in str(rejected.value)
    assert transport.closed


# ------------------------------------------------------------------ live smoke


@pytest.mark.skipif(
    os.environ.get("NAI_LATTICE_LIVE") != "1",
    reason="set NAI_LATTICE_LIVE=1 to run the live Lattice NAF/1 smoke",
)
def test_live_event_and_artifact_roundtrip() -> None:
    suffix = uuid.uuid4().hex
    data = f"NAI-Lattice Python NAF/1 live smoke {suffix}".encode("utf-8")
    digest = blake3_module.blake3(data).hexdigest()
    with naf.NafClient(app_id="nai-lattice-client-tests") as client:
        assert client.naf_version == 1
        event = client.event_append(
            naf.TEST_EVENT_NAMESPACE,
            "naf.client.smoke",
            payload={
                "suffix": suffix,
                "source": "nai_lattice_client.tests.test_client",
            },
            correlation_id=suffix,
            # Use the durable path so the append is not left in the host's
            # group-commit queue (the runtime drains batched frames only on an
            # explicit flush).
            durability="durable",
            idempotency_key=f"nai-lattice-py-smoke-{suffix}",
        )
        event_id = event.get("event_id")
        assert isinstance(event_id, str) and event_id

        # Reuse the idempotency key: the host must return the same event id.
        duplicate = client.event_append(
            naf.TEST_EVENT_NAMESPACE,
            "naf.client.smoke",
            payload={
                "suffix": suffix,
                "source": "nai_lattice_client.tests.test_client",
            },
            correlation_id=suffix,
            durability="durable",
            idempotency_key=f"nai-lattice-py-smoke-{suffix}",
        )
        assert duplicate.get("duplicated") is True
        assert duplicate.get("event_id") == event_id

        put = client.artifact_put(data, "text/plain", "nai.lattice.test.smoke")
        artifact_uri = put.get("artifact_uri")
        assert artifact_uri == f"artifact://blake3/{digest}"
        assert client.artifact_open(artifact_uri) == data
        assert client.artifact_verify(digest) is True
        print(
            "LIVE_NAF "
            f"suffix={suffix} event_id={event_id} "
            f"artifact_uri={artifact_uri} hash={digest}"
        )
