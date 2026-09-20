#!/usr/bin/env node
/**
 * Fake `nisse` binary for extension subprocess tests.
 *
 * Emits canned machine-readable output for the four Nisse tool command
 * shapes. Modes:
 *
 *   args[0] === "boom"              -> exit 2 with stderr
 *   args includes --sleep-forever   -> stay alive until killed
 *   env FAKE_NISSE_EMIT === "garbage" -> emit invalid JSON for review/query
 *
 * Review results include echo fields (stdin size, unknown future field)
 * so tests can assert stdin delivery and verbatim output passthrough.
 */

import * as fs from "node:fs";

const args = process.argv.slice(2);

function stdinText() {
	try {
		return fs.readFileSync(0, "utf8");
	} catch {
		return "";
	}
}

if (args[0] === "boom") {
	process.stderr.write("nisse: simulated failure\n");
	process.exit(2);
}

if (args.includes("--sleep-forever")) {
	process.stdout.write("waiting\n");
	setInterval(() => {}, 1 << 30);
} else if (args[0] === "review" && args[1] === "plan") {
	if (process.env.FAKE_NISSE_EMIT === "garbage") {
		process.stdout.write("this is not json\n");
		process.exit(0);
	}
	const stdin = stdinText();
	const result = {
		review_id: "01FAKE_REVIEW_ID",
		kind: "plan",
		created_at: "2026-01-01T00:00:00Z",
		snapshot: { mode: "working" },
		summary: "fake plan review",
		findings: [],
		echo_stdin_bytes: Buffer.byteLength(stdin, "utf8"),
		unknown_future_field: { note: "must survive verbatim passthrough" },
	};
	process.stdout.write(JSON.stringify(result) + "\n");
	process.exit(0);
} else if (args[0] === "review" && args[1] === "code") {
	const result = {
		review_id: "01FAKE_CODE_REVIEW_ID",
		kind: "code",
		created_at: "2026-01-01T00:00:00Z",
		snapshot: { mode: args.includes("--staged") ? "staged" : "working" },
		summary: "fake code review",
		findings: [],
	};
	process.stdout.write(JSON.stringify(result) + "\n");
	process.exit(0);
} else if (args[0] === "query") {
	if (process.env.FAKE_NISSE_EMIT === "garbage") {
		process.stdout.write("this is not json\n");
		process.exit(0);
	}
	const report = { query: args[1], snapshot: { mode: "working" }, truncated: false, text_bytes: 0, entries: [] };
	process.stdout.write(JSON.stringify(report) + "\n");
	process.exit(0);
} else if (args[0] === "feedback") {
	const reviewId = args[1];
	const findingId = args[2];
	process.stdout.write(
		`nisse feedback\nreview: ${reviewId}\nfinding: ${findingId}\noutcome: recorded (fake)\n`,
	);
	process.exit(0);
} else {
	process.stderr.write(`nisse: unexpected arguments: ${args.join(" ")}\n`);
	process.exit(64);
}
