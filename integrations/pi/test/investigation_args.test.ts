/**
 * Unit tests for the investigation tool arg-building and temp-file
 * lifecycle (reviewInvestigationArgs).
 */

import assert from "node:assert/strict";
import { test } from "node:test";
import { existsSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { mkdtempSync, writeFileSync } from "node:fs";

import {
  reviewInvestigationArgs,
  type InvestigationInput,
} from "../extensions/nisse.ts";

function withCwd<T>(cwd: string, run: () => T): T {
  const previous = process.cwd();
  process.chdir(cwd);
  try {
    return run();
  } finally {
    process.chdir(previous);
  }
}

test("literal text inputs are written to temp files referenced by absolute @paths", async () => {
  const cwd = mkdtempSync(join(tmpdir(), "nisse-args-"));
  let captured: string[] = [];
  let settled = false;
  const input: InvestigationInput = {
    task: "Investigate the greeting module.",
    result: "Findings: 1. The module greets users.",
  };
  const promise = withCwd(cwd, () =>
    reviewInvestigationArgs(input, cwd, async (args) => {
      captured = args;
      settled = true;
      // While the subprocess is pending, the temp files must still exist
      // and carry the literal input.
      const taskArg = args[args.indexOf("--task") + 1];
      const resultArg = args[args.indexOf("--result") + 1];
      assert!(taskArg.startsWith("@"));
      assert!(resultArg.startsWith("@"));
      const taskPath = taskArg.slice(1);
      const resultPath = resultArg.slice(1);
      assert!(existsSync(taskPath), `task temp file must exist: ${taskPath}`);
      assert!(
        readFileSync(taskPath, "utf8") === input.task,
        "task temp content",
      );
      assert!(existsSync(resultPath), "result temp file must exist");
      return { ok: true };
    }),
  );
  await promise;
  assert(settled, "buildAndRun must have settled");
  assert!(captured.includes("--json"));
  // After settle, the temp files are cleaned up.
  const taskArg = captured[captured.indexOf("--task") + 1];
  assert(!existsSync(taskArg.slice(1)), "temp file must be removed after settle");
});

test("@file and existing-file inputs pass through unchanged", async () => {
  const cwd = mkdtempSync(join(tmpdir(), "nisse-args-"));
  const existing = join(cwd, "result.md");
  writeFileSync(existing, "the investigation output", "utf8");
  let captured: string[] = [];
  await withCwd(cwd, () =>
    reviewInvestigationArgs(
      { task: "@task.md", result: "result.md" },
      cwd,
      async (args) => {
        captured = args;
        return { ok: true };
      },
    ),
  );
  const taskArg = captured[captured.indexOf("--task") + 1];
  const resultArg = captured[captured.indexOf("--result") + 1];
  // User @ paths pass through exactly (no re-rooting into the temp dir).
  assert.equal(taskArg, "@task.md");
  // Bare existing paths pass through (resolved against the session cwd).
  assert.equal(resultArg, join(cwd, "result.md"));
  assert!(!existsSync(join(cwd, "nisse-task-")), "no temp file for @ input");
  assert!(
    statSync(existing).isFile(),
    "the referenced file must not be deleted"
  );
});

test("temp files are cleaned up even when the run rejects", async () => {
  const cwd = mkdtempSync(join(tmpdir(), "nisse-args-"));
  let tempFile: string | undefined;
  await assert.rejects(
    withCwd(cwd, () =>
      reviewInvestigationArgs(
        { task: "task text", result: "result text" },
        cwd,
        async (args) => {
          tempFile = args[args.indexOf("--task") + 1].slice(1);
          assert!(existsSync(tempFile));
          throw new Error("scripted failure");
        },
      ),
    ),
    /scripted failure/,
  );
  assert!(tempFile !== undefined && !existsSync(tempFile), "cleanup on failure");
});
