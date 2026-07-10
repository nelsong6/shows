export type PinSyncSnapshot = Readonly<{
  desired: boolean | null;
  inFlight: boolean;
  sent: boolean | null;
}>;

export type PinRequest = Readonly<{
  generation: number;
  value: boolean;
}>;

export type PinSyncSettleResult = Readonly<{
  awaitAck: boolean;
  pump: boolean;
  rollback: boolean;
}>;

/**
 * Serializes stay-on-top intents without mistaking an old host heartbeat for
 * acknowledgement of a newer intent that has not been dispatched yet.
 */
export class PinSyncController {
  private desired: boolean | null = null;
  private inFlight = false;
  private lastObserved: boolean | null = null;
  private nextGeneration = 0;
  private sent: PinRequest | null = null;

  queue(desired: boolean): void {
    this.desired = desired;
  }

  dispatch(): PinRequest | null {
    if (this.inFlight || this.desired === null) return null;
    this.sent = {
      generation: ++this.nextGeneration,
      value: this.desired,
    };
    this.inFlight = true;
    return this.sent;
  }

  observe(observed: boolean): boolean | null {
    this.lastObserved = observed;
    if (this.desired === null) return observed;

    // A host value acknowledges the latest intent only after that exact value
    // has been sent. This guard is what preserves the queued second half of a
    // rapid false -> true -> false double-click when an old false heartbeat
    // arrives while the true request is still in flight.
    if (this.sent?.value === this.desired && observed === this.desired) {
      this.desired = null;
      return observed;
    }

    return null;
  }

  settle(request: PinRequest, succeeded: boolean): PinSyncSettleResult {
    if (!this.inFlight || this.sent?.generation !== request.generation) {
      throw new Error('pin request settled out of order');
    }

    this.inFlight = false;
    let rollback = false;
    if (!succeeded && this.desired === request.value) {
      this.desired = null;
      rollback = true;
    }

    return {
      awaitAck: succeeded && this.desired === request.value,
      rollback,
      pump: this.desired !== null && this.desired !== request.value,
    };
  }

  expireAcknowledgement(request: PinRequest): boolean | null {
    if (
      this.inFlight ||
      this.sent?.generation !== request.generation ||
      this.desired !== request.value ||
      this.lastObserved === null ||
      this.lastObserved === request.value
    ) {
      return null;
    }

    this.desired = null;
    return this.lastObserved;
  }

  snapshot(): PinSyncSnapshot {
    return {
      desired: this.desired,
      inFlight: this.inFlight,
      sent: this.sent?.value ?? null,
    };
  }
}
