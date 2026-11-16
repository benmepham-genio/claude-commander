import assert from "node:assert/strict";
import { describe, test } from "node:test";
import { Poller, type Timers } from "../src/poll.ts";

/** Timers under the test's control: nothing fires until `fire()`. */
function fakeTimers() {
  const pending = new Map<number, { fn: () => void; ms: number }>();
  let next = 1;
  const timers: Timers = {
    set: (fn, ms) => {
      const id = next++;
      pending.set(id, { fn, ms });
      return id;
    },
    clear: (id) => {
      pending.delete(id as number);
    },
  };
  return {
    timers,
    pending,
    /** Fire every pending timer once. */
    fire() {
      const due = [...pending.entries()];
      pending.clear();
      for (const [, t] of due) t.fn();
    },
  };
}

/** A task whose runs complete only when the test says so. */
function gatedTask() {
  const runs: { resolve: () => void; reject: (e: unknown) => void }[] = [];
  let active = 0;
  let maxActive = 0;
  const task = () =>
    new Promise<void>((resolve, reject) => {
      active++;
      maxActive = Math.max(maxActive, active);
      const done = () => {
        active--;
      };
      runs.push({
        resolve: () => {
          done();
          resolve();
        },
        reject: (e) => {
          done();
          reject(e);
        },
      });
    });
  return {
    task,
    runs,
    get maxActive() {
      return maxActive;
    },
  };
}

const tick = () => new Promise((r) => setImmediate(r));

describe("Poller", () => {
  test("the next run is scheduled only after the current one finishes", async () => {
    const t = fakeTimers();
    const g = gatedTask();
    const p = new Poller(g.task, 1500, t.timers);
    p.start();
    assert.equal(g.runs.length, 1);
    // Still in flight: no timer yet, so a slow request can't be overlapped.
    assert.equal(t.pending.size, 0);
    g.runs[0]?.resolve();
    await tick();
    assert.equal(t.pending.size, 1);
    assert.equal([...t.pending.values()][0]?.ms, 1500);
    t.fire();
    assert.equal(g.runs.length, 2);
    assert.equal(g.maxActive, 1);
  });

  test("a trigger while in flight never overlaps; it runs once more afterwards", async () => {
    const t = fakeTimers();
    const g = gatedTask();
    const p = new Poller(g.task, 1500, t.timers);
    p.start();
    const a = p.trigger();
    const b = p.trigger();
    assert.equal(g.runs.length, 1, "no second request while one is in flight");
    g.runs[0]?.resolve();
    await tick();
    // The follow-up run sees whatever the triggering mutation changed.
    assert.equal(g.runs.length, 2);
    let settled = false;
    Promise.all([a, b]).then(() => {
      settled = true;
    });
    await tick();
    assert.equal(settled, false, "a trigger resolves only after its follow-up run");
    g.runs[1]?.resolve();
    await tick();
    assert.equal(settled, true);
    assert.equal(g.maxActive, 1);
    assert.equal(t.pending.size, 1, "exactly one timer after the burst");
  });

  test("a trigger between runs replaces the pending timer", async () => {
    const t = fakeTimers();
    const g = gatedTask();
    const p = new Poller(g.task, 1500, t.timers);
    p.start();
    g.runs[0]?.resolve();
    await tick();
    assert.equal(t.pending.size, 1);
    p.trigger();
    assert.equal(t.pending.size, 0, "the timer is cancelled, not left to double up");
    assert.equal(g.runs.length, 2);
  });

  test("a failing run still schedules the next", async () => {
    const t = fakeTimers();
    const g = gatedTask();
    const p = new Poller(g.task, 1500, t.timers);
    const started = p.start();
    g.runs[0]?.reject(new Error("offline"));
    await started;
    assert.equal(t.pending.size, 1);
  });

  test("stop cancels the schedule", async () => {
    const t = fakeTimers();
    const g = gatedTask();
    const p = new Poller(g.task, 1500, t.timers);
    p.start();
    p.stop();
    g.runs[0]?.resolve();
    await tick();
    assert.equal(t.pending.size, 0);
  });
});

test("the fallback cadence can slow down without delaying explicit refresh", async () => {
  const f = fakeTimers();
  let calls = 0;
  const poller = new Poller(
    async () => {
      calls++;
    },
    1500,
    f.timers,
  );
  await poller.start();
  poller.setIntervalMs(30_000);
  assert.equal([...f.pending.values()][0]?.ms, 30_000);
  await poller.trigger();
  assert.equal(calls, 2);
  poller.stop();
});
test("unchanged cadence cannot postpone scheduled reconciliation", async () => {
  const f = fakeTimers();
  const poller = new Poller(async () => {}, 30_000, f.timers);
  await poller.start();
  const original = [...f.pending.keys()][0];
  poller.setIntervalMs(30_000);
  poller.setIntervalMs(30_000);
  assert.equal([...f.pending.keys()][0], original);
  poller.stop();
});
