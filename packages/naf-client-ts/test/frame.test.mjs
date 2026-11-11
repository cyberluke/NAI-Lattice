import test from 'node:test';
import assert from 'node:assert/strict';
import { encodeWireFrame, parseFrames, decodeWireFrame } from '../dist/index.js';
import { encodeEnvelope, decodeEnvelope, KindType } from '../dist/index.js';

test('frame round-trips an envelope through canonical framing', () => {
  const env = {
    request_id: 0,
    request_id_raw: new TextEncoder().encode('aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee'),
    idempotency_key: 'idem-001',
    producer_id: 'gastown.host',
    correlation_id: '',
    causation_id: '',
    deadline_ms: 0,
    priority: 0,
    payload_type: '',
    payload: new Uint8Array(0),
    kindType: KindType.Request,
    request: { method: 'gastown.admin.health', args: new TextEncoder().encode('{}') },
  };
  const wire = encodeWireFrame(env);
  // Wire has 4-byte length prefix + protobuf Frame.
  assert.ok(wire.length > 4);
  const { frames, rest, errors } = parseFrames(wire);
  assert.deepEqual(errors, []);
  assert.equal(rest.length, 0);
  assert.equal(frames.length, 1);
  const back = frames[0];
  assert.equal(back.kindType, KindType.Request);
  assert.equal(back.request?.method, 'gastown.admin.health');
  assert.equal(back.producer_id, 'gastown.host');
  assert.equal(back.idempotency_key, 'idem-001');
});

test('corrupt frame is rejected by canonical integrity', () => {
  const env = {
    request_id: 0,
    request_id_raw: new Uint8Array(0),
    idempotency_key: '',
    producer_id: 'p',
    correlation_id: '',
    causation_id: '',
    deadline_ms: 0,
    priority: 0,
    payload_type: '',
    payload: new Uint8Array(0),
    kindType: KindType.Request,
    request: { method: 'x', args: new Uint8Array(0) },
  };
  const wire = encodeWireFrame(env);
  // Flip a byte inside the body (not the length prefix).
  const idx = wire.length - 8;
  wire[idx] ^= 0xff;
  // parseFrames must report an error, not crash or return a frame.
  const { frames, errors } = parseFrames(wire);
  assert.ok(errors.length >= 1, 'corrupt frame must be rejected');
  assert.equal(frames.length, 0);
});

// Golden frame emitted by the TS client—referenced by the Rust golden test in
// lattice-protocol to prove TS↔Rust wire interop (NAF-001/NAF-010).
export const GOLDEN_HEX = (() => {
  const env = {
    request_id: 0,
    request_id_raw: new TextEncoder().encode('aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee'),
    idempotency_key: 'idem-001',
    producer_id: 'gastown.host',
    correlation_id: '',
    causation_id: '',
    deadline_ms: 0,
    priority: 0,
    payload_type: '',
    payload: new Uint8Array(0),
    kindType: KindType.Request,
    request: { method: 'gastown.admin.health', args: new TextEncoder().encode('{}') },
  };
  return Buffer.from(encodeWireFrame(env)).toString('hex');
})();

test('emits a stable golden frame', () => {
  console.log('GOLDEN_HEX=' + GOLDEN_HEX);
});