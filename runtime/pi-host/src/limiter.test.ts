/**
 * Tests for the reviewer-session concurrency limiter (plan milestone 29:
 * reviewer concurrency obeys config).
 */

import assert from "node:assert/strict";
import test from "node:test";

import { createLimiter, type Limiter } from "./limiter.ts";

/** Live concurrency bookkeeping shared by the jobs of one test. */
interface Counter {
  concurrent: number;
  maxObserved: number;
}

/** A controllable job: records its start, waits for an explicit release. */
interface Gate {
  readonly job: () => Promise<void>;
  readonly started: Promise<void>;
  release(): void;
  readonly released: Promise<void>;
}

function gatedJob(counter: Counter): Gate {
  let start!: () => void;
  let finish!: () => void;
  const started = new Promise<void>((resolve) => {
    start = resolve;
  });
  const released = new Promise<void>((resolve) => {
    finish = resolve;
  });
  const gate: Gate = {
    started,
    released,
    release(): void {
      counter.concurrent -= 1;
      finish();
    },
    job: () =>
      new Promise<void>((resolve) => {
        counter.concurrent += 1;
        counter.maxObserved = Math.max(counter.maxObserved, counter.concurrent);
        start();
        // `release()` resolves the job by resolving `released`'s
        // controller — wired below.
        void released.then(() => resolve());
      }),
  };
  return gate;
}

/** Schedule one gated job on `limiter`. */
function runGate(limiter: Limiter, counter: Counter): Gate {
  const gate = gatedJob(counter);
  void limiter.run(gate.job).done;
  return gate;
}

test("limiter rejects invalid limits", () => {
  for (const bad of [0, -1, 1.5, 17, Number.NaN]) {
    assert.throws(() => createLimiter(bad), /max concurrent reviewers/);
  }
  for (const good of [1, 2, 4, 16]) {
    assert.ok(createLimiter(good) !== undefined);
  }
});

test("limiter runs at most the configured number of jobs concurrently", async () => {
  const limiter = createLimiter(2);
  const counter: Counter = { concurrent: 0, maxObserved: 0 };

  const first = runGate(limiter, counter);
  const second = runGate(limiter, counter);
  const third = runGate(limiter, counter);
  const fourth = runGate(limiter, counter);

  await Promise.all([first.started, second.started]);
  // Two slots taken: the rest must wait.
  assert.equal(counter.concurrent, 2);
  assert.equal(limiter.active, 2);
  assert.equal(limiter.pending, 2);

  first.release();
  await first.released;
  await third.started;
  assert.equal(counter.concurrent, 2);
  assert.equal(limiter.active, 2);
  assert.equal(limiter.pending, 1);

  second.release();
  third.release();
  await fourth.started;
  fourth.release();
  await Promise.all([second.released, third.released, fourth.released]);
  // One macrotask tick: the limiter releases slots in a `finally` after
  // each job promise resolves.
  await new Promise((resolve) => setTimeout(resolve, 0));

  assert.equal(counter.maxObserved, 2, "never more than two jobs at once");
  assert.equal(limiter.active, 0);
  assert.equal(limiter.pending, 0);
});

test("limiter of one serializes jobs in submission order", async () => {
  const limiter = createLimiter(1);
  const counter: Counter = { concurrent: 0, maxObserved: 0 };
  const order: number[] = [];

  [1, 2, 3].map((id) => {
    const gate = runGate(limiter, counter);
    void gate.started.then(() => {
      order.push(id);
      gate.release();
    });
    return gate;
  });
  await new Promise((resolve) => setTimeout(resolve, 20));
  assert.deepEqual(order, [1, 2, 3]);
  assert.equal(counter.maxObserved, 1);
});

test("limiter slot is released when a job throws", async () => {
  const limiter = createLimiter(1);
  const counter: Counter = { concurrent: 0, maxObserved: 0 };

  // A failing job must not leak its slot: the `done` promise resolves
  // (never rejects) and the next job still runs.
  const failing = limiter.run(async (): Promise<void> => {
    counter.concurrent += 1;
    counter.maxObserved = Math.max(counter.maxObserved, counter.concurrent);
    counter.concurrent -= 1;
    throw new Error("scripted job failure");
  });
  const next = runGate(limiter, counter);
  await failing.done;
  await next.started;
  next.release();
  await next.released;
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(counter.maxObserved, 1);
  assert.equal(limiter.active, 0);
});
