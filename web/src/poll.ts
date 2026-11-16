// The workspace poll's scheduler. Pure (timers are injected), so `node --test`
// covers it; main.ts supplies the task.
//
// A fixed `setInterval` fires whether or not the last poll has answered, so a
// slow server stacks up overlapping requests whose snapshots can land out of
// order (an older one overwriting a newer) and whose notification diffs fire
// twice. Here the next run is scheduled only once the current one has
// finished, and a `trigger()` (the refresh button, a mutation's follow-up)
// never starts a second request alongside one in flight: it asks for one more
// run after it instead, so the caller still sees state fetched after its
// mutation.

export interface Timers {
  set(fn: () => void, ms: number): unknown;
  clear(handle: unknown): void;
}

const browserTimers: Timers = {
  set: (fn, ms) => setTimeout(fn, ms),
  clear: (handle) => clearTimeout(handle as ReturnType<typeof setTimeout>),
};

export class Poller {
  private readonly task: () => Promise<void>;
  private intervalMs: number;
  private readonly timers: Timers;
  private running: Promise<void> | null = null;
  private again = false;
  private timer: unknown = null;
  private stopped = true;

  constructor(task: () => Promise<void>, intervalMs: number, timers: Timers = browserTimers) {
    this.task = task;
    this.intervalMs = intervalMs;
    this.timers = timers;
  }

  /** Run now, then every `intervalMs` after each run completes. */
  start(): Promise<void> {
    this.stopped = false;
    return this.trigger();
  }

  setIntervalMs(ms: number): void {
    if (this.intervalMs === ms) return;
    this.intervalMs = ms;
    if (this.timer !== null) {
      this.cancelTimer();
      this.timer = this.timers.set(() => {
        this.timer = null;
        this.trigger();
      }, ms);
    }
  }

  stop(): void {
    this.stopped = true;
    this.cancelTimer();
  }

  /**
   * Run as soon as possible. While a run is in flight this queues exactly one
   * more (however many times it is called) and resolves after that one.
   */
  trigger(): Promise<void> {
    if (this.running) {
      this.again = true;
      return this.running;
    }
    this.cancelTimer();
    this.running = this.loop();
    return this.running;
  }

  private async loop(): Promise<void> {
    try {
      do {
        this.again = false;
        try {
          await this.task();
        } catch {
          // The task reports its own failures; the schedule carries on.
        }
      } while (this.again);
    } finally {
      this.running = null;
      if (!this.stopped) {
        this.timer = this.timers.set(() => {
          this.timer = null;
          this.trigger();
        }, this.intervalMs);
      }
    }
  }

  private cancelTimer(): void {
    if (this.timer !== null) {
      this.timers.clear(this.timer);
      this.timer = null;
    }
  }
}
