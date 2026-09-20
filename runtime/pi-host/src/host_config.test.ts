/**
 * Tests for strict `host_config` payload validation, focused on the
 * reviewer-concurrency bound (plan milestone 29: reviewer concurrency
 * obeys config).
 */

import assert from "node:assert/strict";
import test from "node:test";

import { ValidationError, validateHostConfig } from "./structured.ts";

test("host_config accepts model profiles alone", () => {
  const config = validateHostConfig({
    models: { standard: { model: "acme/local-small" } },
  });
  assert.equal(config.models.size, 1);
  assert.equal(config.maxConcurrentReviewers, undefined);
});

test("host_config carries the optional reviewer concurrency bound", () => {
  const config = validateHostConfig({
    models: {},
    max_concurrent_reviewers: 2,
  });
  assert.equal(config.maxConcurrentReviewers, 2);
  assert.equal(config.models.size, 0);
});

test("host_config rejects invalid reviewer concurrency bounds", () => {
  for (const bad of [0, -1, 1.5, 17, "2", null]) {
    assert.throws(
      () =>
        validateHostConfig({
          models: {},
          max_concurrent_reviewers: bad,
        }),
      ValidationError,
      `expected ${String(bad)} to be rejected`,
    );
  }
});

test("host_config still rejects malformed model tables", () => {
  assert.throws(() => validateHostConfig({ models: "nope" }), ValidationError);
  assert.throws(
    () => validateHostConfig({ models: { "": { model: "m" } } }),
    ValidationError,
  );
  assert.throws(
    () => validateHostConfig({ models: { standard: { model: "" } } }),
    ValidationError,
  );
});
