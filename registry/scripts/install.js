#!/usr/bin/env node
// Epic Harness plugin bootstrap and cross-platform hook runner.
// Uses only Node.js built-ins; no package setup is needed.

"use strict";

import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const BINARY = "epic-harness";
const ADAPTER_ROOT = fileURLToPath(new URL("../../", import.meta.url));
const SESSION_START_RUNNER_TIMEOUT_MS = 30_000;
// All manifest hooks must own a finite input lifetime. SessionStart retains
// its legacy overrides because its host may keep stdin open for the response.
const HOOK_INPUT_TIMEOUT_MS = 5_000;
const HOOK_INPUT_MAX_BYTES = 1_048_576;
const HOOK_CHILD_TIMEOUT_MS = 30_000;
const HOOK_CHILD_TEARDOWN_GRACE_MS = 1_000;
const HOOK_RUNNER_TIMEOUT_MS = 30_000;
// Codex gives SessionEnd three seconds. Keep 500 ms for host scheduling and
// process shutdown after the runner has completed its own work.
const SESSION_END_RUNNER_TIMEOUT_MS = 2_500;
const STRUCTURED_OUTPUT_EVENTS = new Set([
  "SessionStart",
  "SubagentStop",
  "PreCompact",
  "SessionEnd",
]);
const REQUIRED_STRUCTURED_OUTPUT_EVENTS = new Set([
  "SessionStart",
  "SessionEnd",
]);
const HOOK_COMMANDS = new Map([
  ["SessionStart", new Set(["resume"])],
  ["PreToolUse", new Set(["guard"])],
  ["PostToolUse", new Set(["observe", "polish"])],
  ["PostToolUseFailure", new Set(["observe"])],
  ["SubagentStart", new Set(["observe"])],
  ["SubagentStop", new Set(["observe"])],
  ["PreCompact", new Set(["snapshot"])],
  ["SessionEnd", new Set(["reflect"])],
]);

function log(message) {
  process.stderr.write(`[epic-harness plugin] ${message}\n`);
}

function positiveIntegerEnvironment(name, fallback) {
  const value = process.env[name];
  if (value === undefined) return fallback;
  if (!/^[1-9]\d*$/.test(value)) return fallback;
  const parsed = Number(value);
  return Number.isSafeInteger(parsed) ? parsed : fallback;
}

function hookInputLimits(event) {
  const generic = {
    inputMaxBytes: positiveIntegerEnvironment(
      "EPIC_HOOK_INPUT_MAX_BYTES",
      HOOK_INPUT_MAX_BYTES,
    ),
    inputTimeoutMs: positiveIntegerEnvironment(
      "EPIC_HOOK_INPUT_TIMEOUT_MS",
      HOOK_INPUT_TIMEOUT_MS,
    ),
  };
  if (event !== "SessionStart") return generic;
  return {
    inputMaxBytes: positiveIntegerEnvironment(
      "EPIC_HOOK_SESSIONSTART_INPUT_MAX_BYTES",
      generic.inputMaxBytes,
    ),
    inputTimeoutMs: positiveIntegerEnvironment(
      "EPIC_HOOK_SESSIONSTART_INPUT_TIMEOUT_MS",
      generic.inputTimeoutMs,
    ),
  };
}

function hookChildLimits(event) {
  const generic = {
    childTimeoutMs: positiveIntegerEnvironment(
      "EPIC_HOOK_CHILD_TIMEOUT_MS",
      HOOK_CHILD_TIMEOUT_MS,
    ),
    teardownGraceMs: positiveIntegerEnvironment(
      "EPIC_HOOK_CHILD_TEARDOWN_GRACE_MS",
      HOOK_CHILD_TEARDOWN_GRACE_MS,
    ),
  };
  if (event !== "SessionStart") return generic;
  return {
    ...generic,
    childTimeoutMs: positiveIntegerEnvironment(
      "EPIC_HOOK_SESSIONSTART_CHILD_TIMEOUT_MS",
      generic.childTimeoutMs,
    ),
  };
}

function hookRunnerTimeoutMs(event) {
  const timeoutMs = positiveIntegerEnvironment(
    "EPIC_HOOK_RUNNER_TIMEOUT_MS",
    HOOK_RUNNER_TIMEOUT_MS,
  );
  return event === "SessionEnd"
    ? Math.min(timeoutMs, SESSION_END_RUNNER_TIMEOUT_MS)
    : timeoutMs;
}

function sessionStartRunnerTimeoutMs() {
  return positiveIntegerEnvironment(
    "EPIC_HOOK_SESSIONSTART_RUNNER_TIMEOUT_MS",
    SESSION_START_RUNNER_TIMEOUT_MS,
  );
}

function sessionStartLimits() {
  return {
    ...hookChildLimits("SessionStart"),
    ...hookInputLimits("SessionStart"),
  };
}

function runChild(command, args, {
  captureStdout = false,
  captureStderr = false,
  input,
  label,
  teardownGraceMs = HOOK_CHILD_TEARDOWN_GRACE_MS,
  deadlineAt,
  timeoutMs,
} = {}) {
  let effectiveTimeoutMs = timeoutMs;
  if (deadlineAt !== undefined) {
    const remainingMs = deadlineAt - Date.now() - teardownGraceMs;
    if (remainingMs <= 0) {
      return Promise.resolve({
        error: new Error(`${label ?? command} runner deadline expired before start`),
        status: null,
        stderr: "",
        stdout: "",
      });
    }
    effectiveTimeoutMs = Math.min(
      timeoutMs ?? remainingMs,
      remainingMs,
    );
  }
  return new Promise((resolve) => {
    let child;
    let settled = false;
    let timedOut = false;
    let stdout = "";
    let stderr = "";
    let timer;
    let teardownTimer;
    const finish = (result) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      clearTimeout(teardownTimer);
      resolve(result);
    };

    try {
      child = spawn(command, args, {
        shell: false,
        stdio: [
          input === undefined ? "ignore" : "pipe",
          captureStdout ? "pipe" : "ignore",
          captureStderr ? "pipe" : "inherit",
        ],
        detached: process.platform !== "win32" && effectiveTimeoutMs !== undefined,
        windowsHide: true,
      });
    } catch (error) {
      finish({ error, status: null, stderr, stdout });
      return;
    }

    if (captureStdout) {
      child.stdout.setEncoding("utf8");
      child.stdout.on("data", (chunk) => { stdout += chunk; });
    }
    if (captureStderr) {
      child.stderr.setEncoding("utf8");
      child.stderr.on("data", (chunk) => { stderr += chunk; });
    }
    child.once("error", (error) => {
      finish({ error, status: null, stderr, stdout });
    });
    child.once("close", (status, signal) => {
      if (timedOut) {
        finish({
          error: new Error(`${label ?? command} timed out after ${effectiveTimeoutMs} ms`),
          status,
          stderr,
          stdout,
        });
        return;
      }
      finish({ signal, status, stderr, stdout });
    });
    if (effectiveTimeoutMs !== undefined) {
      timer = setTimeout(() => {
        timedOut = true;
        if (process.platform === "win32" && child.pid !== undefined) {
          const taskkill = spawn(
            join(process.env.SystemRoot ?? "C:\\Windows", "System32", "taskkill.exe"),
            ["/pid", String(child.pid), "/t", "/f"],
            { shell: false, stdio: "ignore", windowsHide: true },
          );
          taskkill.once("error", () => child.kill("SIGKILL"));
          taskkill.once("close", () => child.kill("SIGKILL"));
        } else if (Number.isSafeInteger(child.pid) && child.pid > 0) {
          try {
            // A detached POSIX child is a process-group leader. Killing its
            // negative PID reaches descendants that inherited hook pipes.
            process.kill(-child.pid, "SIGKILL");
          } catch {
            child.kill("SIGKILL");
          }
        } else {
          child.kill("SIGKILL");
        }
        teardownTimer = setTimeout(() => {
          // Do not let a surviving inherited pipe keep the runner alive after
          // termination was requested. The child is already being killed.
          child.stdin?.destroy();
          child.stdout?.destroy();
          child.stderr?.destroy();
          child.unref();
          finish({
            error: new Error(`${label ?? command} timed out after ${effectiveTimeoutMs} ms`),
            status: null,
            stderr,
            stdout,
          });
        }, teardownGraceMs);
      }, effectiveTimeoutMs);
    }
    if (input !== undefined) {
      child.stdin.once("error", (error) => {
        if (error.code !== "EPIPE") {
          finish({ error, status: null, stderr, stdout });
        }
      });
      child.stdin.end(input);
    }
  });
}

class HookRunError extends Error {
  constructor(message, exitCode) {
    super(message);
    this.exitCode = exitCode;
  }
}

function parseVersionContract(output) {
  const trimmed = output.trim();
  const match = /^epic-harness\s+(\d+\.\d+\.\d+)\s+runtime-revision\s+([1-9]\d*)\s+build-identity\s+(sha256:[0-9a-f]{64})$/.exec(
    trimmed,
  );
  return match
    ? { version: match[1], revision: match[2], buildIdentity: match[3] }
    : null;
}

async function getBinaryRuntime(timeoutMs, deadlineAt) {
  const result = await runChild(BINARY, ["version"], {
    captureStderr: true,
    captureStdout: true,
    deadlineAt,
    label: `${BINARY} version probe`,
    timeoutMs,
  });
  if (result.error?.code === "ENOENT") return null;
  if (result.error) throw result.error;
  if (result.status !== 0) return null;

  return parseVersionContract([result.stdout, result.stderr].filter(Boolean).join("\n"));
}

function readJsonFile(path, label) {
  try {
    const value = JSON.parse(readFileSync(path, "utf8"));
    if (value === null || typeof value !== "object" || Array.isArray(value)) {
      throw new Error("must contain a JSON object");
    }
    return value;
  } catch (error) {
    throw new Error(`cannot read ${label} ${path}: ${error.message}`);
  }
}

function getPluginRoot() {
  const pluginRoot = resolve(ADAPTER_ROOT);
  for (const name of ["CLAUDE_PLUGIN_ROOT", "PLUGIN_ROOT"]) {
    const locator = process.env[name];
    if (!locator) continue;
    if (resolve(locator) !== pluginRoot) {
      throw new Error(
        `${name} locates ${resolve(locator)}, but this adapter is installed at ${pluginRoot}`,
      );
    }
  }
  return pluginRoot;
}

function getPluginRuntime() {
  const pluginRoot = getPluginRoot();

  const revisionPath = join(pluginRoot, "runtime-revision.txt");
  let revision;
  try {
    revision = readFileSync(revisionPath, "utf8").trim();
  } catch (error) {
    throw new Error(`cannot read runtime revision ${revisionPath}: ${error.message}`);
  }
  if (!/^[1-9]\d*$/.test(revision)) {
    throw new Error(`runtime revision must be a positive integer: ${revision}`);
  }

  const bundlePath = join(pluginRoot, "registry", "scripts", "bundle-manifest.json");
  const bundle = readJsonFile(bundlePath, "bundle manifest");
  if (!/^\d+\.\d+\.\d+$/.test(bundle.release_version ?? "")) {
    throw new Error(`bundle manifest has an invalid release_version: ${bundle.release_version}`);
  }
  if (!/^[1-9]\d*$/.test(bundle.runtime_revision ?? "")) {
    throw new Error(
      `bundle manifest has an invalid runtime_revision: ${bundle.runtime_revision}`,
    );
  }
  if (!/^sha256:[0-9a-f]{64}$/.test(bundle.build_identity ?? "")) {
    throw new Error(
      `bundle manifest has an invalid build_identity: ${bundle.build_identity}`,
    );
  }
  if (bundle.runtime_revision !== revision) {
    throw new Error(
      `runtime revision ${revision} does not match bundle revision ${bundle.runtime_revision}`,
    );
  }
  return {
    pluginRoot,
    version: bundle.release_version,
    revision,
    buildIdentity: bundle.build_identity,
  };
}

function runtimeLabel(runtime) {
  return `${runtime.version} runtime-revision ${runtime.revision} build-identity ${runtime.buildIdentity}`;
}

function verificationFailure(message, runtime) {
  return new HookRunError(
    `runtime verification failed: ${message}; check the epic-harness binary and plugin bundle at ${runtime?.pluginRoot ?? ADAPTER_ROOT}`,
    1,
  );
}

async function verifyBinaryRuntime(runtime, timeoutMs, deadlineAt) {
  const current = await getBinaryRuntime(timeoutMs, deadlineAt);
  if (!current) {
    throw verificationFailure(`${BINARY} was not found or emitted an invalid version contract`, runtime);
  }
  if (
    current.version !== runtime.version ||
    current.revision !== runtime.revision ||
    current.buildIdentity !== runtime.buildIdentity
  ) {
    throw verificationFailure(
      `expected ${runtimeLabel(runtime)}, found ${runtimeLabel(current)}`,
      runtime,
    );
  }
}

async function verifyRuntime(timeoutMs, deadlineAt) {
  let runtime;
  try {
    runtime = getPluginRuntime();
  } catch (error) {
    throw verificationFailure(error.message);
  }
  await verifyBinaryRuntime(runtime, timeoutMs, deadlineAt);
  return runtime;
}

function validatedGuardDeny(output) {
  const trimmed = output.trim();
  if (!trimmed) return null;

  let parsed;
  try {
    parsed = JSON.parse(trimmed);
  } catch {
    return null;
  }
  const hookSpecificOutput = parsed?.hookSpecificOutput;
  if (
    parsed === null ||
    typeof parsed !== "object" ||
    Array.isArray(parsed) ||
    hookSpecificOutput === null ||
    typeof hookSpecificOutput !== "object" ||
    Array.isArray(hookSpecificOutput) ||
    hookSpecificOutput.hookEventName !== "PreToolUse" ||
    hookSpecificOutput.permissionDecision !== "deny" ||
    typeof hookSpecificOutput.permissionDecisionReason !== "string" ||
    !hookSpecificOutput.permissionDecisionReason.trim()
  ) {
    return null;
  }
  return trimmed;
}

function completeJsonValueEnd(input) {
  let index = 0;
  while (index < input.length && /\s/.test(input[index])) index += 1;
  if (index === input.length) return null;

  const first = input[index];
  if (first === "{" || first === "[") {
    const opening = first;
    const stack = [opening === "{" ? "}" : "]"];
    let inString = false;
    let escaped = false;

    for (index += 1; index < input.length; index += 1) {
      const character = input[index];
      if (inString) {
        if (escaped) {
          escaped = false;
        } else if (character === "\\") {
          escaped = true;
        } else if (character === "\"") {
          inString = false;
        }
        continue;
      }
      if (character === "\"") {
        inString = true;
      } else if (character === "{") {
        stack.push("}");
      } else if (character === "[") {
        stack.push("]");
      } else if (character === "}" || character === "]") {
        if (stack.pop() !== character) return null;
        if (stack.length === 0) return index + 1;
      }
    }
    return null;
  }

  if (first === "\"") {
    let escaped = false;
    for (index += 1; index < input.length; index += 1) {
      const character = input[index];
      if (escaped) {
        escaped = false;
      } else if (character === "\\") {
        escaped = true;
      } else if (character === "\"") {
        return index + 1;
      }
    }
    return null;
  }

  for (const literal of ["true", "false", "null"]) {
    if (input.startsWith(literal, index)) return index + literal.length;
  }
  const number = /-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?/.exec(
    input.slice(index),
  );
  return number ? index + number[0].length : null;
}

function completeSessionStartInput(input) {
  const valueEnd = completeJsonValueEnd(input);
  if (valueEnd === null) return false;
  try {
    JSON.parse(input.slice(0, valueEnd));
  } catch {
    return false;
  }
  if (input.slice(valueEnd).trim()) {
    throw new Error("SessionStart input has trailing non-whitespace data");
  }
  return true;
}

function readHookInput(event, deadlineAt) {
  return new Promise((resolve, reject) => {
    let input = "";
    let inputBytes = 0;
    let settled = false;
    const limits = hookInputLimits(event);
    const remainingMs = Math.max(1, deadlineAt - Date.now());
    const inputTimeoutMs = Math.min(limits.inputTimeoutMs, remainingMs);
    let timer;
    let sessionStartCompletionScheduled = false;
    const finish = () => {
      if (!settled) {
        settled = true;
        clearTimeout(timer);
        resolve(input);
      }
    };
    const fail = (error) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      process.stdin.pause();
      process.stdin.destroy();
      reject(error);
    };

    process.stdin.setEncoding("utf8");
    process.stdin.on("data", (chunk) => {
      if (settled) return;
      inputBytes += Buffer.byteLength(chunk, "utf8");
      if (inputBytes > limits.inputMaxBytes) {
        fail(
          new Error(
            `${event} input exceeded ${limits.inputMaxBytes} byte limit`,
          ),
        );
        return;
      }
      input += chunk;
      if (event === "SessionStart") {
        // Codex keeps SessionStart stdin open while waiting for this command.
        // Only this event may dispatch before EOF.
        let complete;
        try {
          complete = completeSessionStartInput(input);
        } catch (error) {
          fail(error);
          return;
        }
        if (!complete) {
          // The JSON may be split across chunks; the bounded timer handles an
          // incomplete held-open payload while EOF retains the legacy path.
          return;
        }
        if (sessionStartCompletionScheduled) return;
        sessionStartCompletionScheduled = true;
        // Do not wait for EOF, but give chunks that are already queued behind
        // the completed JSON one event-loop turn to reach the trailing-data
        // check before resume can run.
        setImmediate(() => {
          sessionStartCompletionScheduled = false;
          if (settled) return;
          let completeAfterDrain;
          try {
            completeAfterDrain = completeSessionStartInput(input);
          } catch (error) {
            fail(error);
            return;
          }
          if (!completeAfterDrain) return;
          process.stdin.pause();
          process.stdin.destroy();
          finish();
        });
      }
    });
    process.stdin.once("end", () => {
      if (event !== "SessionStart" && input.trim()) {
        try {
          JSON.parse(input);
        } catch (error) {
          fail(new Error(`invalid JSON input for ${event}: ${error.message}`));
          return;
        }
      }
      finish();
    });
    process.stdin.once("error", fail);
    timer = setTimeout(() => {
      fail(
        new Error(
          `${event} input timed out after ${inputTimeoutMs} ms`,
        ),
      );
    }, inputTimeoutMs);
  });
}

async function runHook(event, subcommand, outerDeadlineAt) {
  if (!HOOK_COMMANDS.get(event)?.has(subcommand)) {
    throw new Error(`unsupported hook command: ${event} ${subcommand}`);
  }

  const captureStdout =
    event === "PreToolUse" || STRUCTURED_OUTPUT_EVENTS.has(event);
  const deadlineAt = Math.min(
    outerDeadlineAt ?? Infinity,
    Date.now() + hookRunnerTimeoutMs(event),
  );
  const input = await readHookInput(event, deadlineAt);
  const limits =
    event === "SessionStart" ? sessionStartLimits() : hookChildLimits(event);
  const remainingMs = deadlineAt - Date.now();
  let childTimeoutMs = Math.min(
    limits.childTimeoutMs,
    remainingMs - limits.teardownGraceMs,
  );
  if (childTimeoutMs <= 0) {
    throw new HookRunError(
      `${event} runner deadline expired before starting ${subcommand}`,
      1,
    );
  }
  await verifyRuntime(childTimeoutMs, deadlineAt);
  const hookRemainingMs = deadlineAt - Date.now();
  childTimeoutMs = Math.min(
    limits.childTimeoutMs,
    hookRemainingMs - limits.teardownGraceMs,
  );
  if (childTimeoutMs <= 0) {
    throw new HookRunError(
      `${event} runner deadline expired before starting ${subcommand}`,
      1,
    );
  }
  const result = await runChild(BINARY, [subcommand], {
    captureStdout,
    deadlineAt,
    input,
    label: `${BINARY} ${subcommand}`,
    teardownGraceMs: limits.teardownGraceMs,
    timeoutMs: childTimeoutMs,
  });

  if (result.error?.code === "ENOENT") {
    throw new HookRunError(
      `${BINARY} not found while running ${event}`,
      1,
    );
  }
  if (result.error) {
    throw new HookRunError(
      `${BINARY} ${subcommand} failed: ${result.error.message}`,
      1,
    );
  }
  if (event === "PreToolUse" && result.status === 2) {
    const denial = validatedGuardDeny(result.stdout);
    if (!denial) {
      throw new HookRunError(
        `${BINARY} ${subcommand} emitted an invalid guard denial`,
        1,
      );
    }
    process.stdout.write(`${denial}\n`);
    throw new HookRunError(
      `${BINARY} ${subcommand} denied the tool request`,
      2,
    );
  }
  if (result.status !== 0) {
    throw new HookRunError(
      `${BINARY} ${subcommand} failed with exit code ${result.status}`,
      result.status ?? 1,
    );
  }

  if (event === "PreToolUse" && result.stdout.trim()) {
    throw new HookRunError(
      `${BINARY} ${subcommand} emitted unexpected guard output`,
      1,
    );
  }

  if (STRUCTURED_OUTPUT_EVENTS.has(event)) {
    let output = result.stdout.trim();
    if (!output && event === "SubagentStop") {
      output = "{}";
    }
    if (!output) {
      if (REQUIRED_STRUCTURED_OUTPUT_EVENTS.has(event)) {
        throw new HookRunError(
          `${BINARY} ${subcommand} emitted no required structured output for ${event}`,
          1,
        );
      }
      return;
    }

    let parsed;
    try {
      parsed = JSON.parse(output);
    } catch {
      throw new Error(
        `${BINARY} ${subcommand} emitted invalid JSON for ${event}`,
      );
    }
    if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
      throw new Error(
        `${BINARY} ${subcommand} emitted a non-object JSON value for ${event}`,
      );
    }
    process.stdout.write(`${output}\n`);
  }
}

function failureOutputForInvocation() {
  const [mode, event] = process.argv.slice(2);
  if (mode !== "hook") {
    return null;
  }
  if (STRUCTURED_OUTPUT_EVENTS.has(event)) {
    return "{}";
  }
  return null;
}

async function main() {
  const [mode, event, subcommand, ...extra] = process.argv.slice(2);

  if (mode !== "hook" || !event || !subcommand || extra.length > 0) {
    throw new Error("usage: install.js hook <event> <subcommand>");
  }

  if (event === "SessionStart") {
    const deadlineAt = Date.now() + sessionStartRunnerTimeoutMs();
    await runHook(event, subcommand, deadlineAt);
    return;
  }
  await runHook(event, subcommand);
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(process.argv[1]).href
) {
  main().catch((error) => {
    const output = failureOutputForInvocation();
    if (output) {
      process.stdout.write(`${output}\n`);
    }
    log(error.message);
    process.exitCode = error.exitCode ?? 1;
  });
}
