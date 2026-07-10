import assert from 'node:assert/strict';
import test from 'node:test';

import {PinSyncController} from '../.test-dist/pinSync.js';

test('a matching dispatched host value acknowledges the intent', () => {
  const sync = new PinSyncController();
  assert.equal(sync.observe(false), false);

  sync.queue(true);
  const request = sync.dispatch();
  assert.equal(request.value, true);
  assert.equal(sync.observe(false), null);
  assert.equal(sync.observe(true), true);
  assert.deepEqual(sync.settle(request, true), {awaitAck: false, pump: false, rollback: false});
  assert.deepEqual(sync.snapshot(), {desired: null, inFlight: false, sent: true});
});

test('an old heartbeat cannot erase the queued half of a rapid double-click', () => {
  const sync = new PinSyncController();
  assert.equal(sync.observe(false), false);

  sync.queue(true);
  const pinRequest = sync.dispatch();
  assert.equal(pinRequest.value, true);
  sync.queue(false);

  // This is the race: false is the latest desired value, but it is only queued.
  // The heartbeat describes the old host state and must not acknowledge it.
  assert.equal(sync.observe(false), null);
  assert.deepEqual(sync.settle(pinRequest, true), {awaitAck: false, pump: true, rollback: false});

  const unpinRequest = sync.dispatch();
  assert.equal(unpinRequest.value, false);
  assert.equal(sync.observe(true), null);
  assert.equal(sync.observe(false), false);
  assert.deepEqual(sync.settle(unpinRequest, true), {awaitAck: false, pump: false, rollback: false});
  assert.deepEqual(sync.snapshot(), {desired: null, inFlight: false, sent: false});
});

test('a third click may supersede the queued opposite intent', () => {
  const sync = new PinSyncController();
  sync.queue(true);
  const request = sync.dispatch();
  assert.equal(request.value, true);
  sync.queue(false);
  sync.queue(true);

  assert.equal(sync.observe(true), true);
  assert.deepEqual(sync.settle(request, true), {awaitAck: false, pump: false, rollback: false});
  assert.deepEqual(sync.snapshot(), {desired: null, inFlight: false, sent: true});
});

test('an acknowledgement timeout repairs a successfully returned lost command', () => {
  const sync = new PinSyncController();
  assert.equal(sync.observe(false), false);
  sync.queue(true);
  const request = sync.dispatch();

  assert.deepEqual(sync.settle(request, true), {awaitAck: true, pump: false, rollback: false});
  assert.equal(sync.observe(false), null);
  assert.equal(sync.expireAcknowledgement(request), false);
  assert.deepEqual(sync.snapshot(), {desired: null, inFlight: false, sent: true});
});

test('an old timeout cannot roll back a newer same-value generation', () => {
  const sync = new PinSyncController();
  assert.equal(sync.observe(false), false);
  sync.queue(true);
  const first = sync.dispatch();
  assert.deepEqual(sync.settle(first, true), {awaitAck: true, pump: false, rollback: false});

  sync.queue(false);
  const unpin = sync.dispatch();
  assert.deepEqual(sync.settle(unpin, true), {awaitAck: true, pump: false, rollback: false});
  sync.queue(true);
  const second = sync.dispatch();

  assert.equal(sync.expireAcknowledgement(first), null);
  assert.equal(second.value, true);
});

test('failure rolls back only when it affects the latest intent', () => {
  const current = new PinSyncController();
  current.queue(true);
  const currentRequest = current.dispatch();
  assert.equal(currentRequest.value, true);
  assert.deepEqual(current.settle(currentRequest, false), {awaitAck: false, pump: false, rollback: true});

  const superseded = new PinSyncController();
  superseded.queue(true);
  const supersededRequest = superseded.dispatch();
  assert.equal(supersededRequest.value, true);
  superseded.queue(false);
  assert.deepEqual(superseded.settle(supersededRequest, false), {awaitAck: false, pump: true, rollback: false});
  assert.equal(superseded.dispatch().value, false);
});
