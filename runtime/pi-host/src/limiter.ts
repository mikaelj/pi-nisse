/**
 * Concurrency limiter for reviewer sessions (plan milestone 29).
 *
 * The Pi review host executes the reviewers of a `start_review` payload
 * concurrently, but concurrency must obey configuration: the Rust core
 * forwards `performance.max_concurrent_reviewers` through `host_config`,
 * and the host may never run more reviewer sessions at once than that
 * bound. Extra reviewers queue deterministically (submission order) and
 * start as slots free.
 *
 * The limiter is deliberately tiny and unbounded in queue length: the
 * frozen jury maximum is four reviewers per round, plus at most one
 * retry round, so the queue is always small and each session already
 * enforces its own spec timeout.
 */

/** Result of scheduling a job: resolves when the job has completed. */
export interface ScheduledJob {
  readonly done: Promise<void>;
}

/** A bounded-concurrency scheduler. */
export interface Limiter {
  /**
   * Schedule `job` for execution. The job runs as soon as a slot is
   * available, never more than `max` jobs at once. The returned promise
   * resolves when the job has settled (it never rejects — job failures
   * are the job's own responsibility to report).
   */
  run(job: () => Promise<void>): ScheduledJob;
  /** How many jobs are currently executing. */
  readonly active: number;
  /** How many jobs are waiting for a slot. */
  readonly pending: number;
}

/** Rejected limiter construction. */
export class LimiterError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "LimiterError";
  }
}

/** Upper bound accepted for the configured limit (mirrors Rust config). */
export const MAX_CONCURRENT_REVIEWERS_LIMIT = 16;

/**
 * Create a limiter that runs at most `max` jobs concurrently.
 * `max` must be an integer between 1 and
 * {@link MAX_CONCURRENT_REVIEWERS_LIMIT}.
 */
export function createLimiter(max: number): Limiter {
  if (!Number.isInteger(max) || max < 1 || max > MAX_CONCURRENT_REVIEWERS_LIMIT) {
    throw new LimiterError(
      `max concurrent reviewers must be an integer between 1 and ` +
        `${MAX_CONCURRENT_REVIEWERS_LIMIT}, got ${String(max)}`,
    );
  }
  let active = 0;
  const queue: (() => void)[] = [];
  const release = (): void => {
    active -= 1;
    const next = queue.shift();
    if (next !== undefined) {
      next();
    }
  };
  return {
    run(job: () => Promise<void>): ScheduledJob {
      const done = (async (): Promise<void> => {
        await new Promise<void>((resolve) => {
          const start = (): void => {
            active += 1;
            resolve();
          };
          if (active < max) {
            start();
            return;
          }
          queue.push(start);
        });
        try {
          await job();
        } catch {
          // The job's own failure reporting is the job's responsibility
          // (`runReviewer` answers `reviewer_failed`); the limiter only
          // owns the slot, and `done` never rejects.
        } finally {
          release();
        }
      })();
      return { done };
    },
    get active(): number {
      return active;
    },
    get pending(): number {
      return queue.length;
    },
  };
}
