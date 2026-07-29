#!/usr/bin/env node
// Epic Harness plugin bootstrap and cross-platform hook runner.
// Uses only Node.js built-ins — no npm install needed.

"use strict";

import { spawn } from "node:child_process";
import {
  chmodSync,
  createWriteStream,
  mkdtempSync,
  readFileSync,
  rmSync,
} from "node:fs";
import { join } from "node:path";
import https from "node:https";
import os from "node:os";
import { pathToFileURL } from "node:url";

const REPO = "epicsagas/epic-harness";
const BINARY = "epic-harness";
const CARGO_PKG = "epic-harness";
const INSTALLER_MAX_REDIRECTS = 5;
const INSTALLER_REQUEST_TIMEOUT_MS = 15_000;
const INSTALLER_TOTAL_TIMEOUT_MS = 60_000;
const SESSION_START_CHILD_TIMEOUT_MS = 30_000;
const SESSION_START_INPUT_TIMEOUT_MS = 5_000;
const SESSION_START_INPUT_MAX_BYTES = 1_048_576;
const STRUCTURED_CODEX_EVENTS = new Set([
  "SessionStart",
  "SubagentStop",
  "PreCompact",
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

function sessionStartLimits() {
  return {
    childTimeoutMs: positiveIntegerEnvironment(
      "EPIC_HOOK_SESSIONSTART_CHILD_TIMEOUT_MS",
      SESSION_START_CHILD_TIMEOUT_MS,
    ),
    inputMaxBytes: positiveIntegerEnvironment(
      "EPIC_HOOK_SESSIONSTART_INPUT_MAX_BYTES",
      SESSION_START_INPUT_MAX_BYTES,
    ),
    inputTimeoutMs: positiveIntegerEnvironment(
      "EPIC_HOOK_SESSIONSTART_INPUT_TIMEOUT_MS",
      SESSION_START_INPUT_TIMEOUT_MS,
    ),
  };
}

function runChild(command, args, {
  captureStdout = false,
  captureStderr = false,
  input,
  label,
  timeoutMs,
} = {}) {
  return new Promise((resolve) => {
    let child;
    let settled = false;
    let timedOut = false;
    let stdout = "";
    let stderr = "";
    let timer;
    const finish = (result) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
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
          error: new Error(`${label ?? command} timed out after ${timeoutMs} ms`),
          status,
          stderr,
          stdout,
        });
        return;
      }
      finish({ signal, status, stderr, stdout });
    });
    if (timeoutMs !== undefined) {
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
          return;
        }
        child.kill("SIGKILL");
      }, timeoutMs);
    }
    if (input !== undefined) {
      child.stdin.end(input);
    }
  });
}

async function hasCommand(command, timeoutMs) {
  const result = await runChild(command, ["version"], {
    captureStderr: true,
    label: `${command} version probe`,
    timeoutMs,
  });
  if (result.error?.code === "ENOENT") return false;
  if (result.error) throw result.error;
  return result.status === 0;
}

class HookRunError extends Error {
  constructor(message, exitCode) {
    super(message);
    this.exitCode = exitCode;
  }
}

async function getBinaryRuntime(timeoutMs) {
  const result = await runChild(BINARY, ["version"], {
    captureStderr: true,
    captureStdout: true,
    label: `${BINARY} version probe`,
    timeoutMs,
  });
  if (result.error?.code === "ENOENT") return null;
  if (result.error) throw result.error;
  if (result.status !== 0) return null;

  const output = [result.stderr, result.stdout]
    .filter(Boolean)
    .join("\n");
  const match = output.match(
    /(?:^|\r?\n)epic-harness\s+v?(\d+\.\d+\.\d+)\s+runtime-revision\s+([1-9]\d*)(?=\s|$)/,
  );
  return match ? { version: match[1], revision: match[2] } : null;
}

function getPluginRuntime() {
  const isClaude = !!process.env.CLAUDE_PLUGIN_ROOT;
  const pluginRoot =
    process.env.CLAUDE_PLUGIN_ROOT || process.env.PLUGIN_ROOT || "";
  if (!pluginRoot) {
    throw new Error("plugin root is unavailable");
  }

  const manifestPath = join(
    pluginRoot,
    isClaude ? ".claude-plugin" : ".codex-plugin",
    "plugin.json",
  );
  let manifest;
  try {
    manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
  } catch (error) {
    throw new Error(
      `cannot read plugin manifest ${manifestPath}: ${error.message}`,
    );
  }
  const versionMatch =
    /^(\d+\.\d+\.\d+)(?:\+codex\.[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/.exec(
      manifest.version ?? "",
    );
  if (!versionMatch) {
    throw new Error(`plugin manifest has an invalid version: ${manifest.version}`);
  }
  const revisionPath = join(pluginRoot, "runtime-revision.txt");
  let revision;
  try {
    revision = readFileSync(revisionPath, "utf8").trim();
  } catch (error) {
    throw new Error(
      `cannot read runtime revision ${revisionPath}: ${error.message}`,
    );
  }
  if (!/^[1-9]\d*$/.test(revision)) {
    throw new Error(`runtime revision must be a positive integer: ${revision}`);
  }
  return { version: versionMatch[1], revision };
}

function installerUrl(version, extension) {
  return `https://github.com/${REPO}/releases/download/v${version}/epic-harness-installer.${extension}`;
}

export function downloadFile(
  url,
  destination,
  {
    requestTimeoutMs = INSTALLER_REQUEST_TIMEOUT_MS,
    totalTimeoutMs = INSTALLER_TOTAL_TIMEOUT_MS,
  } = {},
) {
  return new Promise((resolve, reject) => {
    const file = createWriteStream(destination, {
      flags: "wx",
      mode: 0o600,
    });
    let settled = false;
    let activeRequest;
    let totalTimer;
    const fail = (error) => {
      if (settled) return;
      settled = true;
      clearTimeout(totalTimer);
      activeRequest?.destroy();
      file.destroy();
      reject(error);
    };
    totalTimer = setTimeout(() => {
      fail(
        new Error(
          `installer download exceeded ${totalTimeoutMs} ms total timeout`,
        ),
      );
    }, totalTimeoutMs);
    file.once("error", fail);

    const follow = (currentUrl, redirects = 0) => {
      let parsedUrl;
      try {
        parsedUrl = new URL(currentUrl);
      } catch (error) {
        fail(new Error(`invalid installer URL ${currentUrl}: ${error.message}`));
        return;
      }
      if (parsedUrl.protocol !== "https:") {
        fail(new Error(`installer URL must use HTTPS: ${currentUrl}`));
        return;
      }
      activeRequest = https.get(parsedUrl, (response) => {
          if ([301, 302, 307, 308].includes(response.statusCode)) {
            if (!response.headers.location) {
              fail(new Error(`redirect without a location for ${currentUrl}`));
              return;
            }
            response.resume();
            if (redirects >= INSTALLER_MAX_REDIRECTS) {
              fail(
                new Error(
                  `installer redirect limit of ${INSTALLER_MAX_REDIRECTS} exceeded`,
                ),
              );
              return;
            }
            follow(
              new URL(response.headers.location, parsedUrl).toString(),
              redirects + 1,
            );
            return;
          }
          if (response.statusCode !== 200) {
            fail(new Error(`HTTP ${response.statusCode} for ${currentUrl}`));
            response.resume();
            return;
          }
          response.pipe(file);
          file.once("finish", () => {
            file.close((error) => {
              if (error) {
                fail(error);
              } else if (!settled) {
                settled = true;
                clearTimeout(totalTimer);
                resolve();
              }
            });
          });
        })
        .on("error", fail);
      activeRequest.setTimeout(requestTimeoutMs, () => {
        fail(
          new Error(
            `installer request timed out after ${requestTimeoutMs} ms for ${currentUrl}`,
          ),
        );
      });
    };
    follow(url);
  });
}

function sameRuntime(left, right) {
  return (
    left?.version === right?.version && left?.revision === right?.revision
  );
}

function runtimeLabel(runtime) {
  return `${runtime.version} (revision ${runtime.revision})`;
}

async function install(requiredRuntime, childTimeoutMs) {
  const requiredVersion = requiredRuntime.version;
  const platform = os.platform();

  if (platform === "darwin") {
    const brewProbe = await runChild("brew", ["--version"], {
      label: "Homebrew probe",
      timeoutMs: childTimeoutMs,
    });
    if (brewProbe.error) throw brewProbe.error;
    if (brewProbe.status === 0) {
      log(`Homebrew detected — installing ${requiredVersion}...`);
      const result = await runChild(
        "brew",
        ["install", "epicsagas/tap/epic-harness"],
        { label: "Homebrew installer", timeoutMs: childTimeoutMs },
      );
      if (result.error) throw result.error;
      if (
        result.status === 0 &&
        sameRuntime(await getBinaryRuntime(childTimeoutMs), requiredRuntime)
      ) {
        return;
      }
      log("Homebrew did not provide the required version; trying next method...");
    }
  }

  const binstallProbe = await runChild("cargo", ["binstall", "--version"], {
    label: "cargo-binstall probe",
    timeoutMs: childTimeoutMs,
  });
  if (binstallProbe.error) throw binstallProbe.error;
  if (binstallProbe.status === 0) {
    log(`cargo-binstall detected — installing ${requiredVersion}...`);
    const result = await runChild(
      "cargo",
      [
        "binstall",
        `${CARGO_PKG}@${requiredVersion}`,
        "--no-confirm",
        "--force",
      ],
      { label: "cargo-binstall installer", timeoutMs: childTimeoutMs },
    );
    if (result.error) throw result.error;
    if (result.status === 0) return;
    log("cargo-binstall failed; falling back to the release installer...");
  }

  if (platform === "win32") {
    const privateDirectory = mkdtempSync(
      join(os.tmpdir(), "epic-harness-installer-"),
    );
    try {
      const destination = join(privateDirectory, "installer.ps1");
      log(`Downloading Windows installer for ${requiredVersion}...`);
      await downloadFile(installerUrl(requiredVersion, "ps1"), destination);
      const result = await runChild(
        "powershell",
        ["-ExecutionPolicy", "Bypass", "-File", destination],
        { label: "PowerShell installer", timeoutMs: childTimeoutMs },
      );
      if (result.error) throw result.error;
      if (result.status !== 0) throw new Error("PowerShell installer failed");
      return;
    } finally {
      rmSync(privateDirectory, { recursive: true, force: true });
    }
  }

  const privateDirectory = mkdtempSync(
    join(os.tmpdir(), "epic-harness-installer-"),
  );
  try {
    const destination = join(privateDirectory, "installer.sh");
    log(`Downloading installer for ${requiredVersion}...`);
    await downloadFile(installerUrl(requiredVersion, "sh"), destination);
    chmodSync(destination, 0o700);
    const result = await runChild("sh", [destination], {
      label: "shell installer",
      timeoutMs: childTimeoutMs,
    });
    if (result.error) throw result.error;
    if (result.status !== 0) throw new Error("shell installer failed");
  } finally {
    rmSync(privateDirectory, { recursive: true, force: true });
  }
}

async function ensureCompatibleRuntime(childTimeoutMs) {
  const requiredRuntime = getPluginRuntime();
  const present = await hasCommand(BINARY, childTimeoutMs);
  const currentRuntime = present ? await getBinaryRuntime(childTimeoutMs) : null;

  if (sameRuntime(currentRuntime, requiredRuntime)) return;

  if (!present) {
    log(`${BINARY} not found — installing ${runtimeLabel(requiredRuntime)}...`);
  } else if (currentRuntime) {
    log(
      `Updating ${BINARY} ${currentRuntime.version} → ${requiredRuntime.version} ` +
        `(runtime revision ${currentRuntime.revision} → ${requiredRuntime.revision})...`,
    );
  } else {
    log(
      `${BINARY} has an unreadable version or runtime revision — installing ${runtimeLabel(requiredRuntime)}...`,
    );
  }

  await install(requiredRuntime, childTimeoutMs);

  const installedRuntime = await getBinaryRuntime(childTimeoutMs);
  if (!sameRuntime(installedRuntime, requiredRuntime)) {
    const actual = installedRuntime
      ? runtimeLabel(installedRuntime)
      : "no readable version";
    throw new Error(
      `required ${BINARY} ${requiredRuntime.version} is unavailable after installation ` +
        `(runtime revision ${requiredRuntime.revision}; found ${actual})`,
    );
  }

  log(
    present
      ? `Updated to ${runtimeLabel(installedRuntime)}`
      : `Installed ${BINARY} ${runtimeLabel(installedRuntime)}`,
  );
}

function runnerProvenance(input) {
  if (!input.trim()) return input;
  try {
    const payload = JSON.parse(input);
    if (payload === null || typeof payload !== "object" || Array.isArray(payload)) {
      return input;
    }
    const host = process.env.CLAUDE_PLUGIN_ROOT ? "claude" : "codex";
    return JSON.stringify({ ...payload, host });
  } catch {
    return input;
  }
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
    hookSpecificOutput.permissionDecision !== "deny"
  ) {
    return null;
  }
  return trimmed;
}

function readHookInput(event) {
  return new Promise((resolve, reject) => {
    let input = "";
    let inputBytes = 0;
    let settled = false;
    const limits = event === "SessionStart" ? sessionStartLimits() : null;
    let timer;
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
      inputBytes += Buffer.byteLength(chunk, "utf8");
      if (limits && inputBytes > limits.inputMaxBytes) {
        fail(
          new Error(
            `SessionStart input exceeded ${limits.inputMaxBytes} byte limit`,
          ),
        );
        return;
      }
      input += chunk;
      if (event === "SessionStart") {
        try {
          JSON.parse(input);
          // Codex keeps SessionStart stdin open while waiting for this command.
          // Only this event may dispatch before EOF.
          process.stdin.pause();
          process.stdin.destroy();
          finish();
        } catch {
          // The JSON may be split across chunks; the bounded timer handles an
          // incomplete held-open payload while EOF retains the legacy path.
        }
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
    if (limits) {
      timer = setTimeout(() => {
        fail(
          new Error(
            `SessionStart input timed out after ${limits.inputTimeoutMs} ms`,
          ),
        );
      }, limits.inputTimeoutMs);
    }
  });
}

async function runHook(event, subcommand) {
  if (!HOOK_COMMANDS.get(event)?.has(subcommand)) {
    throw new Error(`unsupported hook command: ${event} ${subcommand}`);
  }

  const captureStdout =
    event === "PreToolUse" || STRUCTURED_CODEX_EVENTS.has(event);
  const input = runnerProvenance(await readHookInput(event));
  const result = await runChild(BINARY, [subcommand], {
    captureStdout,
    input,
    label: `${BINARY} ${subcommand}`,
    timeoutMs:
      event === "SessionStart" ? sessionStartLimits().childTimeoutMs : undefined,
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

  if (STRUCTURED_CODEX_EVENTS.has(event)) {
    let output = result.stdout.trim();
    if (!output && event === "SubagentStop") {
      output = "{}";
    }
    if (!output) {
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
  if (STRUCTURED_CODEX_EVENTS.has(event)) {
    return "{}";
  }
  return null;
}

async function main() {
  const [mode, event, subcommand, ...extra] = process.argv.slice(2);

  if (mode === undefined) {
    await ensureCompatibleRuntime();
    return;
  }
  if (mode !== "hook" || !event || !subcommand || extra.length > 0) {
    throw new Error("usage: install.js [hook <event> <subcommand>]");
  }

  if (event === "SessionStart") {
    await ensureCompatibleRuntime(sessionStartLimits().childTimeoutMs);
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
