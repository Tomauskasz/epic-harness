import assert from "node:assert/strict";
import {
  chmodSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { spawn, spawnSync } from "node:child_process";
import test from "node:test";

const SCRIPT = fileURLToPath(new URL("./install.js", import.meta.url));
const ROOT = fileURLToPath(new URL("../../", import.meta.url));
const IS_WINDOWS = process.platform === "win32";
const PLUGIN_VERSION = JSON.parse(
  readFileSync(new URL("../../package.json", import.meta.url), "utf8"),
).version;
const RUNTIME_REVISION = readFileSync(
  new URL("../../runtime-revision.txt", import.meta.url),
  "utf8",
).trim();
const BUILD_IDENTITY = JSON.parse(
  readFileSync(new URL("./bundle-manifest.json", import.meta.url), "utf8"),
).build_identity;
const STRUCTURED_HOOKS = [
  ["SessionStart", "resume"],
  ["PreToolUse", "guard"],
  ["SubagentStop", "observe"],
  ["PreCompact", "snapshot"],
  ["SessionEnd", "reflect"],
];
const NON_SESSION_START_MANIFEST_HOOKS = [
  ["PreToolUse", "guard"],
  ["PostToolUse", "observe"],
  ["PostToolUse", "polish"],
  ["SubagentStart", "observe"],
  ["SubagentStop", "observe"],
  ["PreCompact", "snapshot"],
  ["SessionEnd", "reflect"],
];

function makeWindowsCommandShim() {
  if (!IS_WINDOWS) return null;

  const root = mkdtempSync(join(tmpdir(), "epic-harness-command-shim-"));
  const executable = join(root, "command-shim.exe");
  const source = `
using System;
using System.Diagnostics;
using System.IO;
using System.Linq;

public static class CommandShim {
  public static int Main(string[] args) {
    var executablePath = Process.GetCurrentProcess().MainModule.FileName;
    var scriptPath = Path.ChangeExtension(executablePath, ".cmd");
    var quotedArgs = String.Join(" ", args.Select(arg => arg.IndexOfAny(new[] { ' ', '\\t', '\"' }) >= 0 ? "\\\"" + arg.Replace("\\\"", "\\\\\\\"") + "\\\"" : arg));
    var startInfo = new ProcessStartInfo {
      FileName = Environment.GetEnvironmentVariable("ComSpec") ?? "cmd.exe",
      Arguments = "/d /s /c call \\\"" + scriptPath + "\\\" " + quotedArgs,
      UseShellExecute = false,
      RedirectStandardInput = false,
      RedirectStandardOutput = false,
      RedirectStandardError = false,
    };
    using (var child = Process.Start(startInfo)) {
      child.WaitForExit();
      return child.ExitCode;
    }
  }
}`;
  const command = `Add-Type -TypeDefinition @'\n${source}\n'@ -OutputAssembly '${executable.replaceAll("'", "''")}' -OutputType ConsoleApplication`;
  const result = spawnSync(
    "powershell.exe",
    ["-NoLogo", "-NoProfile", "-NonInteractive", "-EncodedCommand", Buffer.from(command, "utf16le").toString("base64")],
    { encoding: "utf8" },
  );
  assert.equal(result.status, 0, result.stderr);
  return executable;
}

const WINDOWS_COMMAND_SHIM = makeWindowsCommandShim();

function writeCommand(bin, name, unixBody, windowsBody) {
  const path = join(bin, IS_WINDOWS ? `${name}.cmd` : name);
  const versionUnix = `if [ "$1" = "version" ]; then
  printf '%s\\n' 'epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION} build-identity ${BUILD_IDENTITY}' >&2
  exit 0
fi`;
  const versionWindows = `if "%1"=="version" (
  echo epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION} build-identity ${BUILD_IDENTITY} 1>&2
  exit /b 0
)`;
  const suppliedVersionUnix = unixBody.includes('if [ "$1" = "version" ]');
  const suppliedVersionWindows = windowsBody.includes('if "%1"=="version"');
  const doctorUnix = `if [ "$1" = "codex" ] && [ "$2" = "doctor" ]; then
  if [ -n "$EPIC_TEST_DOCTOR_OUTPUT" ]; then
    printf '%s\\n' "$EPIC_TEST_DOCTOR_OUTPUT"
  else
    printf '%s\\n' '{"healthy":true}'
  fi
  exit "${'${EPIC_TEST_DOCTOR_STATUS:-0}'}"
fi`;
  const doctorWindows = `if "%1"=="codex" if "%2"=="doctor" (
  if not "%EPIC_TEST_DOCTOR_OUTPUT%"=="" (echo %EPIC_TEST_DOCTOR_OUTPUT%) else (echo {"healthy":true})
  if not "%EPIC_TEST_DOCTOR_STATUS%"=="" exit /b %EPIC_TEST_DOCTOR_STATUS%
  exit /b 0
)`;
  writeFileSync(
    path,
    IS_WINDOWS
      ? `@echo off\r\n${suppliedVersionWindows ? "" : `${versionWindows}\r\n`}${doctorWindows}\r\n${windowsBody}\r\n`
      : `#!/bin/sh\n${suppliedVersionUnix ? "" : `${versionUnix}\n`}${doctorUnix}\n${unixBody}\n`,
  );
  if (IS_WINDOWS) {
    const executable = join(bin, `${name}.exe`);
    copyFileSync(WINDOWS_COMMAND_SHIM, executable);
    return executable;
  }
  chmodSync(path, 0o755);
  return path;
}

function makeFixture(environmentKey, manifestDir, version = PLUGIN_VERSION) {
  void environmentKey;
  const root = mkdtempSync(join(tmpdir(), "epic-harness-install-test-"));
  const bin = join(root, "bin");
  mkdirSync(join(root, manifestDir), { recursive: true });
  mkdirSync(join(root, "registry", "scripts"), { recursive: true });
  mkdirSync(bin, { recursive: true });
  writeFileSync(
    join(root, manifestDir, "plugin.json"),
    JSON.stringify({ version }),
  );
  writeFileSync(join(root, "runtime-revision.txt"), `${RUNTIME_REVISION}\n`);
  writeFileSync(
    join(root, "registry", "scripts", "bundle-manifest.json"),
    JSON.stringify({
      schema_version: 1,
      release_version: PLUGIN_VERSION,
      runtime_revision: RUNTIME_REVISION,
      build_identity: BUILD_IDENTITY,
      artifacts: [],
      identity_inputs: {},
    }),
  );

  return {
    bin,
    env: {
      ...process.env,
      CLAUDE_PLUGIN_ROOT: "",
      PLUGIN_ROOT: "",
      PATH: bin,
    },
    root,
  };
}

function runScript(args, env, input, script = SCRIPT) {
  return spawnSync(process.execPath, [script, ...args], {
    encoding: "utf8",
    env,
    input,
  });
}

function assertSingleJsonObject(stdout, label) {
  const value = JSON.parse(stdout);
  assert.ok(
    value !== null && typeof value === "object" && !Array.isArray(value),
    `${label}: stdout must be one JSON object`,
  );
  assert.equal(
    stdout.trim().split(/\r?\n/).length,
    1,
    `${label}: stdout must contain exactly one line`,
  );
  return value;
}

function waitForExit(child, timeoutMs) {
  return new Promise((resolve) => {
    const timer = setTimeout(() => resolve({ timeout: true }), timeoutMs);
    child.once("exit", (code, signal) => {
      clearTimeout(timer);
      resolve({ code, signal, timeout: false });
    });
  });
}

async function stopChildForTest(child) {
  if (child.exitCode !== null) return;
  child.kill("SIGKILL");
  await new Promise((resolve) => child.once("close", resolve));
}

test("no-argument mode is rejected without invoking any child", () => {
  const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");
  const calls = join(fixture.root, "calls.txt");
  try {
    writeCommand(fixture.bin, "epic-harness",
      "printf '%s\\n' \"$*\" >> \"$EPIC_TEST_CALLS\"",
      "echo %*>>\"%EPIC_TEST_CALLS%\"");
    const result = runScript([], { ...fixture.env, EPIC_TEST_CALLS: calls });
    assert.notEqual(result.status, 0);
    assert.equal(result.stdout, "");
    assert.match(result.stderr, /usage: install\.js hook/);
    assert.throws(() => readFileSync(calls, "utf8"));
  } finally {
    rmSync(fixture.root, { recursive: true, force: true });
  }
});

test("healthy Codex doctor output never reaches hook stdout", () => {
  const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");
  const calls = join(fixture.root, "calls.txt");
  try {
    writeCommand(fixture.bin, "epic-harness",
      "printf '%s\\n' \"$1\" >> \"$EPIC_TEST_CALLS\"; if [ \"$1\" = resume ]; then printf '%s\\n' '{}'; fi",
      "echo %1>>\"%EPIC_TEST_CALLS%\" & if \"%1\"==\"resume\" echo {}");
    const result = runScript(["hook", "SessionStart", "resume"], { ...fixture.env, EPIC_TEST_CALLS: calls }, "{\"hook_event_name\":\"SessionStart\"}");
    assert.equal(result.status, 0, result.stderr);
    assert.deepEqual(assertSingleJsonObject(result.stdout, "healthy doctor"), {});
    assert.doesNotMatch(result.stdout, /healthy/);
    assert.equal(readFileSync(calls, "utf8").trim(), "resume");
  } finally {
    rmSync(fixture.root, { recursive: true, force: true });
  }
});

test("Claude blocks a mismatched public version contract before the hook", () => {
  const fixture = makeFixture("CLAUDE_PLUGIN_ROOT", ".claude-plugin");
  const calls = join(fixture.root, "calls.txt");
  const wrong = "sha256:" + "0".repeat(64);
  try {
    writeCommand(fixture.bin, "epic-harness",
      "if [ \"$1\" = version ]; then printf '%s\\n' 'epic-harness 9.9.9 runtime-revision 999 build-identity " + wrong + "' >&2; exit 0; fi; printf '%s\\n' \"$1\" >> \"$EPIC_TEST_CALLS\"",
      "if \"%1\"==\"version\" (echo epic-harness 9.9.9 runtime-revision 999 build-identity " + wrong + " 1>&2 & exit /b 0) & echo %1>>\"%EPIC_TEST_CALLS%\"");
    const result = runScript(["hook", "SessionStart", "resume"], { ...fixture.env, EPIC_TEST_CALLS: calls }, "{\"hook_event_name\":\"SessionStart\"}");
    assert.notEqual(result.status, 0);
    assert.deepEqual(assertSingleJsonObject(result.stdout, "Claude mismatch"), {});
    assert.throws(() => readFileSync(calls, "utf8"));
    assert.match(result.stderr, /runtime verification failed|expected/);
  } finally {
    rmSync(fixture.root, { recursive: true, force: true });
  }
});

test("a missing Codex runtime never invokes package-manager helpers", () => {
  const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");
  const calls = join(fixture.root, "calls.txt");
  try {
    for (const name of ["cargo", "brew", "powershell", "sh"]) {
      writeCommand(fixture.bin, name, "printf '%s\\n' " + name + " >> \"$EPIC_TEST_CALLS\"", "echo " + name + ">>\"%EPIC_TEST_CALLS%\"");
    }
    const result = runScript(["hook", "SessionStart", "resume"], { ...fixture.env, EPIC_TEST_CALLS: calls }, "{\"hook_event_name\":\"SessionStart\"}");
    assert.notEqual(result.status, 0);
    assert.deepEqual(assertSingleJsonObject(result.stdout, "missing runtime"), {});
    assert.throws(() => readFileSync(calls, "utf8"));
    assert.match(result.stderr, /not found|doctor --repair/);
  } finally {
    rmSync(fixture.root, { recursive: true, force: true });
  }
});





test("SessionStart accepts a Codex cachebuster while comparing the base runtime version", () => {
  const fixture = makeFixture(
    "PLUGIN_ROOT",
    ".codex-plugin",
    `${PLUGIN_VERSION}+codex.20260728181552`,
  );
  const calls = join(fixture.root, "calls.txt");

  try {
    writeCommand(
      fixture.bin,
      "epic-harness",
      `if [ "$1" = "version" ]; then
  printf '%s\\n' 'epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION} build-identity ${BUILD_IDENTITY}' >&2
  exit 0
fi
printf '%s\\n' "$*" >> "$EPIC_TEST_CALLS"
printf '%s\\n' '{}'`,
      `if "%1"=="version" (
  echo epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION} build-identity ${BUILD_IDENTITY} 1>&2
  exit /b 0
)
echo %*>>"%EPIC_TEST_CALLS%"
echo {}`,
    );

    const result = runScript(
      ["hook", "SessionStart", "resume"],
      { ...fixture.env, EPIC_TEST_CALLS: calls },
      JSON.stringify({ hook_event_name: "SessionStart", session_id: "session-1" }),
    );

    assert.equal(result.status, 0, result.stderr);
    assert.deepEqual(assertSingleJsonObject(result.stdout, "SessionStart"), {});
    assert.equal(readFileSync(calls, "utf8").trim(), "resume");
  } finally {
    rmSync(fixture.root, { force: true, recursive: true });
  }
});




test("SubagentStop emits one valid JSON object after observe succeeds", () => {
  const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");
  const input = JSON.stringify({
    hook_event_name: "SubagentStop",
    agent_id: "agent-1",
    agent_type: "worker",
  });

  try {
    writeCommand(
      fixture.bin,
      "epic-harness",
      `if [ "$1" = "observe" ]; then
  exit 0
fi
exit 99`,
      `if "%1"=="observe" (
  exit /b 0
)
exit /b 99`,
    );

    const result = spawnSync(
      process.execPath,
      [SCRIPT, "hook", "SubagentStop", "observe"],
      {
        encoding: "utf8",
        env: fixture.env,
        input,
      },
    );

    assert.equal(result.status, 0, result.stderr);
    assert.deepEqual(JSON.parse(result.stdout), {});
    assert.match(result.stdout, /^\{\}\r?\n$/);
  } finally {
    rmSync(fixture.root, { force: true, recursive: true });
  }
});

test("SubagentStop preserves valid JSON emitted by observe", () => {
  const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");

  try {
    writeCommand(
      fixture.bin,
      "epic-harness",
      `if [ "$1" = "observe" ]; then
  printf '%s\\n' '{"continue":true}'
  exit 0
fi
exit 99`,
      `if "%1"=="observe" (
  echo {"continue":true}
  exit /b 0
)
exit /b 99`,
    );

    const result = spawnSync(
      process.execPath,
      [SCRIPT, "hook", "SubagentStop", "observe"],
      {
        encoding: "utf8",
        env: fixture.env,
        input: JSON.stringify({ hook_event_name: "SubagentStop" }),
      },
    );

    assert.equal(result.status, 0, result.stderr);
    assert.deepEqual(JSON.parse(result.stdout), { continue: true });
    assert.match(result.stdout, /^\{"continue":true\}\r?\n$/);
  } finally {
    rmSync(fixture.root, { force: true, recursive: true });
  }
});

test("missing runtime fails without a synthetic guard denial", () => {
  for (const [event, subcommand] of STRUCTURED_HOOKS) {
    const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");

    try {

      const result = runScript(
        ["hook", event, subcommand],
        fixture.env,
        JSON.stringify({ hook_event_name: event, session_id: "session-1" }),
      );

      if (event === "PreToolUse") {
        assert.notEqual(result.status, 0, result.stderr);
        assert.notEqual(result.status, 2, result.stderr);
        assert.equal(result.stdout, "");
      } else {
        assert.notEqual(result.status, 0, event);
        assert.deepEqual(assertSingleJsonObject(result.stdout, event), {});
      }
      assert.notEqual(result.stderr.trim(), "", `${event}: stderr diagnostic`);
    } finally {
      rmSync(fixture.root, { force: true, recursive: true });
    }
  }
});

test("failing runtime fails without a synthetic guard denial", () => {
  for (const [event, subcommand] of STRUCTURED_HOOKS) {
    const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");

    try {
      writeCommand(
        fixture.bin,
        "epic-harness",
        `if [ "$1" = "version" ]; then
  printf '%s\\n' 'epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION} build-identity ${BUILD_IDENTITY}' >&2
  exit 0
fi
printf '%s\\n' 'runtime ${event} failure' >&2
exit 17`,
        `if "%1"=="version" (
  echo epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION} build-identity ${BUILD_IDENTITY} 1>&2
  exit /b 0
)
echo runtime ${event} failure 1>&2
exit /b 17`,
      );

      const result = runScript(
        ["hook", event, subcommand],
        fixture.env,
        JSON.stringify({ hook_event_name: event, session_id: "session-1" }),
      );

      if (event === "PreToolUse") {
        assert.notEqual(result.status, 0, result.stderr);
        assert.notEqual(result.status, 2, result.stderr);
        assert.equal(result.stdout, "");
      } else {
        assert.equal(result.status, 17, event);
        assert.deepEqual(assertSingleJsonObject(result.stdout, event), {});
      }
      assert.match(result.stderr, new RegExp(`runtime ${event} failure`));
      assert.match(result.stderr, /failed with exit code/i);
    } finally {
      rmSync(fixture.root, { force: true, recursive: true });
    }
  }
});

test("missing unstructured hook runtime keeps stdout empty", () => {
  const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");

  try {
    const result = runScript(
      ["hook", "PostToolUse", "observe"],
      fixture.env,
    );

    assert.notEqual(result.status, 0);
    assert.equal(result.stdout, "");
    assert.match(result.stderr, /epic-harness.*not found/i);
  } finally {
    rmSync(fixture.root, { force: true, recursive: true });
  }
});

test("runner rejects unsupported event and subcommand pairs before invoking a runtime", () => {
  const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");

  try {
    for (const args of [
      ["hook", "PostToolUse", "guard"],
      ["hook", "UnknownEvent", "observe"],
      ["hook", "PostToolUse", "observe", "unexpected"],
    ]) {
      const result = runScript(args, fixture.env);
      assert.notEqual(result.status, 0, args.join(" "));
      assert.equal(result.stdout, "", args.join(" "));
      assert.match(result.stderr, /unsupported hook command|usage/i, args.join(" "));
    }
  } finally {
    rmSync(fixture.root, { force: true, recursive: true });
  }
});

test("runner passes supported hook payloads through without inferred provenance", () => {
  const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");
  const stdinPath = join(fixture.root, "stdin.txt");
  const events = [
    ["SessionStart", "resume"],
    ["PreToolUse", "guard"],
    ["PostToolUse", "observe"],
    ["PostToolUse", "polish"],
    ["PostToolUseFailure", "observe"],
    ["SubagentStart", "observe"],
    ["SubagentStop", "observe"],
    ["PreCompact", "snapshot"],
    ["SessionEnd", "reflect"],
  ];

  try {
    writeCommand(
      fixture.bin,
      "epic-harness",
      `if [ "$1" = "version" ]; then
  printf '%s\\n' 'epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION} build-identity ${BUILD_IDENTITY}' >&2
  exit 0
fi
IFS= read -r EPIC_STDIN || true
printf '%s\\n' "$EPIC_STDIN" > "$EPIC_TEST_STDIN"
if [ "$1" = "resume" ]; then
  printf '%s\\n' '{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"ok"}}'
elif [ "$1" = "reflect" ]; then
  printf '%s\\n' '{"continue":true}'
fi`,
      `if "%1"=="version" (
  echo epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION} build-identity ${BUILD_IDENTITY} 1>&2
  exit /b 0
)
set /p EPIC_STDIN=
> "%EPIC_TEST_STDIN%" echo %EPIC_STDIN%
if "%1"=="resume" echo {"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"ok"}}
if "%1"=="reflect" echo {"continue":true}`,
    );

    for (const [event, subcommand] of events) {
      const input = JSON.stringify({ hook_event_name: event, session_id: "session-1" });
      const result = runScript(
        ["hook", event, subcommand],
        { ...fixture.env, EPIC_TEST_STDIN: stdinPath },
        `${input}\n`,
      );

      assert.equal(result.status, 0, `${event}: ${result.stderr}`);
      assert.deepEqual(
        JSON.parse(readFileSync(stdinPath, "utf8")),
        JSON.parse(input),
        event,
      );
      if (event === "SessionStart") {
        assert.deepEqual(JSON.parse(result.stdout), {
          hookSpecificOutput: {
            hookEventName: "SessionStart",
            additionalContext: "ok",
          },
        });
      } else if (event === "SessionEnd") {
        assert.deepEqual(JSON.parse(result.stdout), { continue: true });
      } else if (event === "SubagentStop") {
        assert.deepEqual(JSON.parse(result.stdout), {});
      } else {
        assert.equal(result.stdout, "", event);
      }
    }
  } finally {
    rmSync(fixture.root, { force: true, recursive: true });
  }
});

test("declared event behavior is independent of plugin root locator variables", () => {
  const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");
  const stdinPath = join(fixture.root, "stdin.txt");
  const input = JSON.stringify({
    hook_event_name: "SessionEnd",
    session_id: "root-locator-matrix",
  });
  const locators = [
    ["neither", { CLAUDE_PLUGIN_ROOT: "", PLUGIN_ROOT: "" }],
    ["Claude locator", { CLAUDE_PLUGIN_ROOT: ROOT, PLUGIN_ROOT: "" }],
    ["Codex locator", { CLAUDE_PLUGIN_ROOT: "", PLUGIN_ROOT: ROOT }],
    ["both locators", { CLAUDE_PLUGIN_ROOT: ROOT, PLUGIN_ROOT: ROOT }],
  ];

  try {
    writeCommand(
      fixture.bin,
      "epic-harness",
      `if [ "$1" = "version" ]; then
  printf '%s\\n' 'epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION} build-identity ${BUILD_IDENTITY}' >&2
  exit 0
fi
IFS= read -r EPIC_STDIN || true
printf '%s\\n' "$EPIC_STDIN" > "$EPIC_TEST_STDIN"
printf '%s\\n' '{"continue":true}'`,
      `if "%1"=="version" (
  echo epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION} build-identity ${BUILD_IDENTITY} 1>&2
  exit /b 0
)
set /p EPIC_STDIN=
> "%EPIC_TEST_STDIN%" echo %EPIC_STDIN%
echo {"continue":true}`,
    );

    for (const [label, locatorEnvironment] of locators) {
      const result = runScript(
        ["hook", "SessionEnd", "reflect"],
        {
          ...fixture.env,
          ...locatorEnvironment,
          EPIC_TEST_STDIN: stdinPath,
        },
        input,
      );

      assert.equal(result.status, 0, `${label}: ${result.stderr}`);
      assert.deepEqual(assertSingleJsonObject(result.stdout, label), { continue: true });
      assert.equal(readFileSync(stdinPath, "utf8").trim(), input, label);
    }
  } finally {
    rmSync(fixture.root, { force: true, recursive: true });
  }
});

test("adapter root follows its own path with spaces and rejects a mismatched locator", () => {
  const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");
  const adapterRoot = join(fixture.root, "adapter root with spaces");
  const adapterScript = join(adapterRoot, "registry", "scripts", "install.js");
  const callsPath = join(fixture.root, "calls.txt");
  mkdirSync(join(adapterRoot, "registry", "scripts"), { recursive: true });
  copyFileSync(SCRIPT, adapterScript);
  copyFileSync(
    join(ROOT, "runtime-revision.txt"),
    join(adapterRoot, "runtime-revision.txt"),
  );
  copyFileSync(
    join(ROOT, "registry", "scripts", "bundle-manifest.json"),
    join(adapterRoot, "registry", "scripts", "bundle-manifest.json"),
  );

  try {
    writeCommand(
      fixture.bin,
      "epic-harness",
      `printf '%s\\n' "$1" > "$EPIC_TEST_CALLS"
printf '%s\\n' '{"continue":true}'`,
      `> "%EPIC_TEST_CALLS%" echo %1
echo {"continue":true}`,
    );

    const valid = runScript(
      ["hook", "SessionEnd", "reflect"],
      {
        ...fixture.env,
        PLUGIN_ROOT: adapterRoot,
        EPIC_TEST_CALLS: callsPath,
      },
      JSON.stringify({ hook_event_name: "SessionEnd" }),
      adapterScript,
    );
    assert.equal(valid.status, 0, valid.stderr);
    assert.deepEqual(assertSingleJsonObject(valid.stdout, "space path"), { continue: true });
    assert.equal(readFileSync(callsPath, "utf8").trim(), "reflect");

    rmSync(callsPath, { force: true });
    const invalid = runScript(
      ["hook", "SessionEnd", "reflect"],
      {
        ...fixture.env,
        PLUGIN_ROOT: join(adapterRoot, "wrong locator"),
        EPIC_TEST_CALLS: callsPath,
      },
      JSON.stringify({ hook_event_name: "SessionEnd" }),
      adapterScript,
    );
    assert.notEqual(invalid.status, 0, invalid.stderr);
    assert.deepEqual(assertSingleJsonObject(invalid.stdout, "mismatched locator"), {});
    assert.match(invalid.stderr, /PLUGIN_ROOT locates .*adapter is installed/i);
    assert.equal(existsSync(callsPath), false, "a mismatched locator must stop before execution");
  } finally {
    rmSync(fixture.root, { force: true, recursive: true });
  }
});

test("Codex SessionStart dispatches complete JSON before stdin closes", async () => {
  const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");
  const stdinPath = join(fixture.root, "stdin.txt");

  try {
    writeCommand(
      fixture.bin,
      "epic-harness",
      `if [ "$1" = "version" ]; then
  printf '%s\\n' 'epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION} build-identity ${BUILD_IDENTITY}' >&2
  exit 0
fi
IFS= read -r EPIC_STDIN || true
printf '%s\\n' "$EPIC_STDIN" > "$EPIC_TEST_STDIN"
printf '%s\\n' '{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"ok"}}'`,
      `if "%1"=="version" (
  echo epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION} build-identity ${BUILD_IDENTITY} 1>&2
  exit /b 0
)
set /p EPIC_STDIN=
> "%EPIC_TEST_STDIN%" (echo(%EPIC_STDIN%)
echo {"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"ok"}}`,
    );
    const input = JSON.stringify({
      hook_event_name: "SessionStart",
      session_id: "open-stdin-session",
    });
    const child = spawn(process.execPath, [SCRIPT, "hook", "SessionStart", "resume"], {
      env: { ...fixture.env, EPIC_TEST_STDIN: stdinPath },
      stdio: ["pipe", "pipe", "pipe"],
    });
    let stdout = "";
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (chunk) => {
      stdout += chunk;
    });
    child.stdin.write(input.slice(0, 13));
    await new Promise((resolve) => setImmediate(resolve));
    child.stdin.write(input.slice(13));

    const result = await new Promise((resolve) => {
      const timer = setTimeout(() => resolve({ timeout: true }), 500);
      child.once("exit", (code, signal) => {
        clearTimeout(timer);
        resolve({ code, signal, timeout: false });
      });
    });
    if (result.timeout) {
      child.kill();
      await new Promise((resolve) => child.once("close", resolve));
    }
    assert.equal(result.timeout, false, "runner waited for stdin EOF after complete JSON");
    assert.equal(result.code, 0, result.signal ?? "SessionStart failed");
    assert.deepEqual(
      JSON.parse(readFileSync(stdinPath, "utf8")),
      JSON.parse(input),
      "runner must invoke resume with the complete payload",
    );
    assert.deepEqual(
      assertSingleJsonObject(stdout, "SessionStart"),
      {
        hookSpecificOutput: {
          hookEventName: "SessionStart",
          additionalContext: "ok",
        },
      },
      "runner must forward resume output",
    );
    child.stdin.destroy();
  } finally {
    rmSync(fixture.root, { force: true, recursive: true });
  }
});

test("non-SessionStart hooks wait for EOF and reject bytes after their JSON input", async () => {
  const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");
  const calls = join(fixture.root, "calls.txt");
  const input = JSON.stringify({ hook_event_name: "PostToolUse", session_id: "eof-session" });

  try {
    writeCommand(
      fixture.bin,
      "epic-harness",
      `if [ "$1" = "observe" ]; then
  printf '%s\\n' called > "$EPIC_TEST_CALLS"
  exit 0
fi
exit 99`,
      `if "%1"=="observe" (
  > "%EPIC_TEST_CALLS%" echo called
  exit /b 0
)
exit /b 99`,
    );

    const child = spawn(process.execPath, [SCRIPT, "hook", "PostToolUse", "observe"], {
      env: { ...fixture.env, EPIC_TEST_CALLS: calls },
      stdio: ["pipe", "pipe", "pipe"],
    });
    child.stdin.write(input);
    await new Promise((resolve) => setTimeout(resolve, 75));
    assert.equal(child.exitCode, null, "PostToolUse must wait for stdin EOF");
    assert.throws(() => readFileSync(calls, "utf8"));
    child.stdin.end();

    const result = await new Promise((resolve) => {
      child.once("exit", (code, signal) => resolve({ code, signal }));
    });
    assert.equal(result.code, 0, result.signal ?? "PostToolUse failed");
    assert.equal(readFileSync(calls, "utf8").trim(), "called");

    const trailing = runScript(
      ["hook", "PostToolUse", "observe"],
      { ...fixture.env, EPIC_TEST_CALLS: calls },
      `${input} trailing`,
    );
    assert.notEqual(trailing.status, 0, trailing.stderr);
    assert.equal(trailing.stdout, "", "trailing input must not reach hook stdout");
    assert.match(trailing.stderr, /invalid JSON input|trailing/i);
  } finally {
    rmSync(fixture.root, { force: true, recursive: true });
  }
});

test("every non-SessionStart manifest hook bounds held-open input before invoking the runtime", async () => {
  for (const [event, subcommand] of NON_SESSION_START_MANIFEST_HOOKS) {
    const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");
    const callsPath = join(fixture.root, "calls.txt");

    try {
      writeCommand(
        fixture.bin,
        "epic-harness",
        `printf '%s\\n' "$1" > "$EPIC_TEST_CALLS"`,
        `> "%EPIC_TEST_CALLS%" echo %1`,
      );
      const child = spawn(process.execPath, [SCRIPT, "hook", event, subcommand], {
        env: {
          ...fixture.env,
          EPIC_HOOK_INPUT_TIMEOUT_MS: "40",
          EPIC_TEST_CALLS: callsPath,
        },
        stdio: ["pipe", "pipe", "pipe"],
      });
      let stdout = "";
      let stderr = "";
      child.stdout.setEncoding("utf8");
      child.stderr.setEncoding("utf8");
      child.stdout.on("data", (chunk) => { stdout += chunk; });
      child.stderr.on("data", (chunk) => { stderr += chunk; });
      child.stdin.write(JSON.stringify({ hook_event_name: event, session_id: "held-open" }));

      const result = await waitForExit(child, 750);
      if (result.timeout) await stopChildForTest(child);

      assert.equal(result.timeout, false, `${event} waited for stdin EOF`);
      assert.notEqual(result.code, 0, `${event}: ${result.signal ?? stderr}`);
      assert.throws(() => readFileSync(callsPath, "utf8"), `${event} invoked its runtime`);
      assert.match(stderr, new RegExp(`${event} input timed out`, "i"));
      if (["SubagentStop", "PreCompact", "SessionEnd"].includes(event)) {
        assert.deepEqual(assertSingleJsonObject(stdout, `${event} input timeout`), {});
      } else {
        assert.equal(stdout, "", `${event} input timeout must not emit stdout`);
      }
    } finally {
      rmSync(fixture.root, { force: true, recursive: true });
    }
  }
});

test("every non-SessionStart manifest hook bounds and terminates a hung runtime", async () => {
  for (const [event, subcommand] of NON_SESSION_START_MANIFEST_HOOKS) {
    const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");
    const pidPath = join(fixture.root, "runtime.pid");

    try {
      writeCommand(
        fixture.bin,
        "epic-harness",
        `exec "$EPIC_TEST_NODE" -e 'require("node:fs").writeFileSync(process.env.EPIC_TEST_RUNTIME_PID, String(process.pid)); setTimeout(() => {}, 5000)'`,
        `"%EPIC_TEST_NODE%" -e "require('node:fs').writeFileSync(process.env.EPIC_TEST_RUNTIME_PID, String(process.pid)); setTimeout(() => {}, 5000)"`,
      );
      const child = spawn(process.execPath, [SCRIPT, "hook", event, subcommand], {
        env: {
          ...fixture.env,
          EPIC_HOOK_CHILD_TEARDOWN_GRACE_MS: "20",
          // Leave enough time for the Windows command shim to start doctor,
          // while still bounding every diagnosis + hook attempt tightly.
          EPIC_HOOK_CHILD_TIMEOUT_MS: "250",
          EPIC_TEST_NODE: process.execPath,
          EPIC_TEST_RUNTIME_PID: pidPath,
        },
        stdio: ["pipe", "pipe", "pipe"],
      });
      let stdout = "";
      let stderr = "";
      child.stdout.setEncoding("utf8");
      child.stderr.setEncoding("utf8");
      child.stdout.on("data", (chunk) => { stdout += chunk; });
      child.stderr.on("data", (chunk) => { stderr += chunk; });
      child.stdin.end(JSON.stringify({ hook_event_name: event, session_id: "hung-runtime" }));

      const result = await waitForExit(child, 750);
      if (result.timeout) {
        child.kill("SIGKILL");
        if (existsSync(pidPath)) {
          try {
            process.kill(Number(readFileSync(pidPath, "utf8")));
          } catch (error) {
            assert.equal(error.code, "ESRCH");
          }
        }
        await new Promise((resolve) => child.once("close", resolve));
      }

      assert.equal(result.timeout, false, `${event} waited for its hung runtime`);
      assert.notEqual(result.code, 0, `${event}: ${result.signal ?? stderr}`);
      assert.match(stderr, new RegExp(`${subcommand} timed out`, "i"));
      if (["SubagentStop", "PreCompact", "SessionEnd"].includes(event)) {
        assert.deepEqual(assertSingleJsonObject(stdout, `${event} runtime timeout`), {});
      } else {
        assert.equal(stdout, "", `${event} runtime timeout must not emit stdout`);
      }
      if (process.platform !== "win32") {
        const runtimePid = Number(readFileSync(pidPath, "utf8"));
        await new Promise((resolve) => setTimeout(resolve, 50));
        assert.throws(() => process.kill(runtimePid, 0), { code: "ESRCH" });
      }
    } finally {
      rmSync(fixture.root, { force: true, recursive: true });
    }
  }
});

test("SessionEnd shares its three-second host budget across input, runtime, and teardown", async () => {
  const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");
  const pidPath = join(fixture.root, "runtime.pid");

  try {
    writeCommand(
      fixture.bin,
      "epic-harness",
      `exec "$EPIC_TEST_NODE" -e 'require("node:fs").writeFileSync(process.env.EPIC_TEST_RUNTIME_PID, String(process.pid)); setTimeout(() => {}, 5000)'`,
      `"%EPIC_TEST_NODE%" -e "require('node:fs').writeFileSync(process.env.EPIC_TEST_RUNTIME_PID, String(process.pid)); setTimeout(() => {}, 5000)"`,
    );
    const child = spawn(process.execPath, [SCRIPT, "hook", "SessionEnd", "reflect"], {
      env: {
        ...fixture.env,
        EPIC_HOOK_CHILD_TEARDOWN_GRACE_MS: "20",
        EPIC_HOOK_CHILD_TIMEOUT_MS: "2000",
        EPIC_HOOK_INPUT_TIMEOUT_MS: "2000",
        EPIC_HOOK_RUNNER_TIMEOUT_MS: "220",
        EPIC_TEST_NODE: process.execPath,
        EPIC_TEST_RUNTIME_PID: pidPath,
      },
      stdio: ["pipe", "pipe", "pipe"],
    });
    let stdout = "";
    let stderr = "";
    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (chunk) => { stdout += chunk; });
    child.stderr.on("data", (chunk) => { stderr += chunk; });
    await new Promise((resolve) => setTimeout(resolve, 80));
    child.stdin.end(JSON.stringify({ hook_event_name: "SessionEnd", session_id: "shared-budget" }));

    const result = await waitForExit(child, 750);
    if (result.timeout) {
      child.kill("SIGKILL");
      if (existsSync(pidPath)) {
        try {
          process.kill(Number(readFileSync(pidPath, "utf8")));
        } catch (error) {
          assert.equal(error.code, "ESRCH");
        }
      }
      await new Promise((resolve) => child.once("close", resolve));
    }

    assert.equal(result.timeout, false, "SessionEnd exceeded its host budget");
    assert.notEqual(result.code, 0, result.signal ?? stderr);
    assert.deepEqual(assertSingleJsonObject(stdout, "SessionEnd shared budget"), {});
    assert.match(stderr, /reflect timed out|runner deadline/i);
  } finally {
    rmSync(fixture.root, { force: true, recursive: true });
  }
});

test("SessionStart shares its deadline across bootstrap and resume", async () => {
  const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");
  const pidPath = join(fixture.root, "bootstrap.pid");

  try {
    writeCommand(
      fixture.bin,
      "epic-harness",
      `exec "$EPIC_TEST_NODE" -e 'require("node:fs").writeFileSync(process.env.EPIC_TEST_BOOTSTRAP_PID, String(process.pid)); setTimeout(() => {}, 5000)'`,
      `"%EPIC_TEST_NODE%" -e "require('node:fs').writeFileSync(process.env.EPIC_TEST_BOOTSTRAP_PID, String(process.pid)); setTimeout(() => {}, 5000)"`,
    );
    const child = spawn(process.execPath, [SCRIPT, "hook", "SessionStart", "resume"], {
      env: {
        ...fixture.env,
        EPIC_HOOK_CHILD_TEARDOWN_GRACE_MS: "20",
        EPIC_HOOK_SESSIONSTART_CHILD_TIMEOUT_MS: "2000",
        EPIC_HOOK_SESSIONSTART_RUNNER_TIMEOUT_MS: "220",
        EPIC_TEST_BOOTSTRAP_PID: pidPath,
        EPIC_TEST_NODE: process.execPath,
      },
      stdio: ["pipe", "pipe", "pipe"],
    });
    let stdout = "";
    let stderr = "";
    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (chunk) => { stdout += chunk; });
    child.stderr.on("data", (chunk) => { stderr += chunk; });
    child.stdin.end(JSON.stringify({ hook_event_name: "SessionStart", session_id: "bootstrap-budget" }));

    const result = await waitForExit(child, 750);
    if (result.timeout) {
      child.kill("SIGKILL");
      if (existsSync(pidPath)) {
        try {
          process.kill(Number(readFileSync(pidPath, "utf8")));
        } catch (error) {
          assert.equal(error.code, "ESRCH");
        }
      }
      await new Promise((resolve) => child.once("close", resolve));
    }

    assert.equal(result.timeout, false, "SessionStart exceeded its bootstrap budget");
    assert.notEqual(result.code, 0, result.signal ?? stderr);
    assert.deepEqual(assertSingleJsonObject(stdout, "SessionStart bootstrap budget"), {});
    assert.match(stderr, /codex doctor timed out|runner deadline|not found/i);
  } finally {
    rmSync(fixture.root, { force: true, recursive: true });
  }
});

test("SessionStart bounds malformed held-open input and input byte growth", async () => {
  const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");

  try {
    writeCommand(
      fixture.bin,
      "epic-harness",
      `if [ "$1" = "version" ]; then
  printf '%s\\n' 'epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION} build-identity ${BUILD_IDENTITY}' >&2
  exit 0
fi
exit 99`,
      `if "%1"=="version" (
  echo epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION} build-identity ${BUILD_IDENTITY} 1>&2
  exit /b 0
)
exit /b 99`,
    );
    const child = spawn(process.execPath, [SCRIPT, "hook", "SessionStart", "resume"], {
      env: { ...fixture.env, EPIC_HOOK_SESSIONSTART_INPUT_TIMEOUT_MS: "40" },
      stdio: ["pipe", "pipe", "pipe"],
    });
    let stdout = "";
    let stderr = "";
    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (chunk) => { stdout += chunk; });
    child.stderr.on("data", (chunk) => { stderr += chunk; });
    child.stdin.write('{"hook_event_name":"SessionStart"');
    const result = await new Promise((resolve) => {
      const timer = setTimeout(() => resolve({ timeout: true }), 750);
      child.once("exit", (code, signal) => {
        clearTimeout(timer);
        resolve({ code, signal, timeout: false });
      });
    });
    if (result.timeout) {
      child.kill();
      await new Promise((resolve) => child.once("close", resolve));
    }
    assert.equal(result.timeout, false, "held-open malformed input must time out");
    assert.notEqual(result.code, 0, result.signal ?? "malformed input unexpectedly succeeded");
    assert.deepEqual(assertSingleJsonObject(stdout, "SessionStart input timeout"), {});
    assert.match(stderr, /input timed out/i);

    const oversized = runScript(
      ["hook", "SessionStart", "resume"],
      { ...fixture.env, EPIC_HOOK_SESSIONSTART_INPUT_MAX_BYTES: "16" },
      JSON.stringify({ hook_event_name: "SessionStart", session_id: "too-large" }),
    );
    assert.notEqual(oversized.status, 0, oversized.stderr);
    assert.deepEqual(assertSingleJsonObject(oversized.stdout, "SessionStart oversized input"), {});
    assert.match(oversized.stderr, /input exceeded .* byte/i);
  } finally {
    rmSync(fixture.root, { force: true, recursive: true });
  }
});


test(
  "POSIX SessionStart timeout kills a forked resume descendant holding stdout",
  { skip: IS_WINDOWS },
  async () => {
    const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");
    const descendantPidPath = join(fixture.root, "descendant.pid");

    try {
      writeCommand(
        fixture.bin,
        "epic-harness",
        `if [ "$1" = "version" ]; then
  printf '%s\\n' 'epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION} build-identity ${BUILD_IDENTITY}' >&2
  exit 0
fi
"$EPIC_TEST_NODE" -e 'require("node:fs").writeFileSync(process.env.EPIC_TEST_DESCENDANT_PID, String(process.pid)); setTimeout(() => {}, 5000)' &
exit 0`,
        "exit /b 99",
      );

      const child = spawn(process.execPath, [SCRIPT, "hook", "SessionStart", "resume"], {
        env: {
          ...fixture.env,
          EPIC_HOOK_SESSIONSTART_CHILD_TIMEOUT_MS: "100",
          EPIC_TEST_DESCENDANT_PID: descendantPidPath,
          EPIC_TEST_NODE: process.execPath,
        },
        stdio: ["pipe", "pipe", "pipe"],
      });
      child.stdin.end(JSON.stringify({ hook_event_name: "SessionStart" }));
      let stdout = "";
      let stderr = "";
      child.stdout.setEncoding("utf8");
      child.stderr.setEncoding("utf8");
      child.stdout.on("data", (chunk) => { stdout += chunk; });
      child.stderr.on("data", (chunk) => { stderr += chunk; });

      const result = await new Promise((resolve) => {
        const timer = setTimeout(() => resolve({ timeout: true }), 1_500);
        child.once("exit", (code, signal) => {
          clearTimeout(timer);
          resolve({ code, signal, timeout: false });
        });
      });
      if (result.timeout) {
        const descendantPid = Number(readFileSync(descendantPidPath, "utf8"));
        process.kill(descendantPid, "SIGKILL");
        await new Promise((resolve) => child.once("close", resolve));
      }
      assert.equal(result.timeout, false, "runner waited for an inherited stdout pipe");
      assert.notEqual(result.code, 0, result.signal ?? "timed-out resume unexpectedly succeeded");
      assert.deepEqual(assertSingleJsonObject(stdout, "forked resume timeout"), {});
      assert.match(stderr, /resume timed out/i);

      const descendantPid = Number(readFileSync(descendantPidPath, "utf8"));
      await new Promise((resolve) => setTimeout(resolve, 100));
      let live = true;
      try {
        process.kill(descendantPid, 0);
        try {
          const state = readFileSync(`/proc/${descendantPid}/stat`, "utf8").split(" ")[2];
          live = state !== "Z";
        } catch {
          // macOS does not expose /proc; a non-ESRCH probe is the best check.
        }
      } catch (error) {
        assert.equal(error.code, "ESRCH");
        live = false;
      }
      assert.equal(live, false, "timed-out descendant remained live");
    } finally {
      rmSync(fixture.root, { force: true, recursive: true });
    }
  },
);

test(
  "Windows SessionStart timeout kills the complete resume process tree",
  { skip: !IS_WINDOWS },
  async () => {
    const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");
    const descendantPidPath = join(fixture.root, "descendant.pid");
    const descendantScript = join(fixture.root, "descendant.js");

    try {
      writeFileSync(
        descendantScript,
        `require("node:fs").writeFileSync(process.env.EPIC_TEST_DESCENDANT_PID, String(process.pid)); setTimeout(() => {}, 5000);`,
      );
      writeCommand(
        fixture.bin,
        "epic-harness",
        "exit 99",
        `if "%1"=="version" (
  echo epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION} build-identity ${BUILD_IDENTITY} 1>&2
  exit /b 0
)
start "" /b "%EPIC_TEST_NODE%" "%EPIC_TEST_DESCENDANT_SCRIPT%"
"%SystemRoot%\\System32\\ping.exe" -n 6 127.0.0.1 >nul`,
      );

      const child = spawn(process.execPath, [SCRIPT, "hook", "SessionStart", "resume"], {
        env: {
          ...fixture.env,
          EPIC_HOOK_SESSIONSTART_CHILD_TIMEOUT_MS: "250",
          EPIC_TEST_DESCENDANT_PID: descendantPidPath,
          EPIC_TEST_DESCENDANT_SCRIPT: descendantScript,
          EPIC_TEST_NODE: process.execPath,
        },
        stdio: ["pipe", "pipe", "pipe"],
      });
      child.stdin.end(JSON.stringify({ hook_event_name: "SessionStart" }));
      let stdout = "";
      let stderr = "";
      child.stdout.setEncoding("utf8");
      child.stderr.setEncoding("utf8");
      child.stdout.on("data", (chunk) => { stdout += chunk; });
      child.stderr.on("data", (chunk) => { stderr += chunk; });

      const result = await waitForExit(child, 1_500);
      if (result.timeout) await stopChildForTest(child);
      assert.equal(result.timeout, false, "runner waited for the Windows process tree");
      assert.notEqual(result.code, 0, result.signal ?? "timed-out resume unexpectedly succeeded");
      assert.deepEqual(assertSingleJsonObject(stdout, "Windows resume timeout"), {});
      assert.match(stderr, /resume timed out/i);

      const descendantPid = Number(readFileSync(descendantPidPath, "utf8"));
      let exited = false;
      const deadline = Date.now() + 1_000;
      while (Date.now() < deadline) {
        try {
          process.kill(descendantPid, 0);
        } catch (error) {
          assert.equal(error.code, "ESRCH");
          exited = true;
          break;
        }
        await new Promise((resolve) => setTimeout(resolve, 25));
      }
      assert.equal(exited, true, `descendant ${descendantPid} remained live after taskkill /t`);
    } finally {
      rmSync(fixture.root, { force: true, recursive: true });
    }
  },
);

test("Codex SessionStart preserves closed empty and incomplete input for resume", () => {
  const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");
  const stdinPath = join(fixture.root, "stdin.txt");

  try {
    writeCommand(
      fixture.bin,
      "epic-harness",
      `if [ "$1" = "version" ]; then
  printf '%s\\n' 'epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION} build-identity ${BUILD_IDENTITY}' >&2
  exit 0
fi
IFS= read -r EPIC_STDIN || true
printf '%s\\n' "$EPIC_STDIN" > "$EPIC_TEST_STDIN"
printf '%s\\n' '{"hookSpecificOutput":{"hookEventName":"SessionStart"}}'`,
      `if "%1"=="version" (
  echo epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION} build-identity ${BUILD_IDENTITY} 1>&2
  exit /b 0
)
set /p EPIC_STDIN=
> "%EPIC_TEST_STDIN%" (echo(%EPIC_STDIN%)
echo {"hookSpecificOutput":{"hookEventName":"SessionStart"}}
exit /b 0`,
    );
    for (const input of ["", '{"hook_event_name":"SessionStart"']) {
      const result = runScript(
        ["hook", "SessionStart", "resume"],
        { ...fixture.env, EPIC_TEST_STDIN: stdinPath },
        input,
      );
      assert.equal(result.status, 0, result.stderr);
      assert.equal(readFileSync(stdinPath, "utf8").trimEnd(), input);
    }
  } finally {
    rmSync(fixture.root, { force: true, recursive: true });
  }
});

test("Codex SessionStart rejects already-present non-whitespace after complete JSON before resume", () => {
  const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");
  const callsPath = join(fixture.root, "calls.txt");

  try {
    writeCommand(
      fixture.bin,
      "epic-harness",
      `if [ "$1" = "version" ]; then
  printf '%s\\n' 'epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION} build-identity ${BUILD_IDENTITY}' >&2
  exit 0
fi
printf '%s\\n' "$1" > "$EPIC_TEST_CALLS"
printf '%s\\n' '{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"unexpected"}}'`,
      `if "%1"=="version" (
  echo epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION} build-identity ${BUILD_IDENTITY} 1>&2
  exit /b 0
)
> "%EPIC_TEST_CALLS%" echo %1
echo {"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"unexpected"}}`,
    );

    const result = runScript(
      ["hook", "SessionStart", "resume"],
      { ...fixture.env, EPIC_TEST_CALLS: callsPath },
      '{"hook_event_name":"SessionStart"} trailing',
    );

    assert.notEqual(result.status, 0, result.stderr);
    assert.deepEqual(assertSingleJsonObject(result.stdout, "SessionStart trailing input"), {});
    assert.throws(() => readFileSync(callsPath, "utf8"));
    assert.match(result.stderr, /trailing|invalid JSON input/i);
  } finally {
    rmSync(fixture.root, { force: true, recursive: true });
  }
});

test("Codex SessionStart drains immediately buffered trailing chunks before resume", async () => {
  const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");
  const callsPath = join(fixture.root, "calls.txt");

  try {
    writeCommand(
      fixture.bin,
      "epic-harness",
      `if [ "$1" = "version" ]; then
  printf '%s\\n' 'epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION} build-identity ${BUILD_IDENTITY}' >&2
  exit 0
fi
printf '%s\\n' "$1" > "$EPIC_TEST_CALLS"
printf '%s\\n' '{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"unexpected"}}'`,
      `if "%1"=="version" (
  echo epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION} build-identity ${BUILD_IDENTITY} 1>&2
  exit /b 0
)
> "%EPIC_TEST_CALLS%" echo %1
echo {"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"unexpected"}}`,
    );
    const child = spawn(process.execPath, [SCRIPT, "hook", "SessionStart", "resume"], {
      env: { ...fixture.env, EPIC_TEST_CALLS: callsPath },
      stdio: ["pipe", "pipe", "pipe"],
    });
    let stdout = "";
    let stderr = "";
    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (chunk) => { stdout += chunk; });
    child.stderr.on("data", (chunk) => { stderr += chunk; });
    child.stdin.write('{"hook_event_name":"SessionStart"}');
    child.stdin.write(" trailing");

    const result = await waitForExit(child, 750);
    if (result.timeout) await stopChildForTest(child);

    assert.equal(result.timeout, false, "SessionStart waited for stdin EOF");
    assert.notEqual(result.code, 0, result.signal ?? stderr);
    assert.deepEqual(assertSingleJsonObject(stdout, "SessionStart trailing chunk"), {});
    assert.throws(() => readFileSync(callsPath, "utf8"));
    assert.match(stderr, /trailing/i);
  } finally {
    rmSync(fixture.root, { force: true, recursive: true });
  }
});

test("Codex SessionStart requires one structured response from resume", () => {
  const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");

  try {
    writeCommand(
      fixture.bin,
      "epic-harness",
      `if [ "$1" = "version" ]; then
  printf '%s\\n' 'epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION} build-identity ${BUILD_IDENTITY}' >&2
fi`,
      `if "%1"=="version" (
  echo epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION} build-identity ${BUILD_IDENTITY} 1>&2
  exit /b 0
)`,
    );

    const result = runScript(
      ["hook", "SessionStart", "resume"],
      fixture.env,
      JSON.stringify({ hook_event_name: "SessionStart" }),
    );

    assert.notEqual(result.status, 0, result.stderr);
    assert.deepEqual(assertSingleJsonObject(result.stdout, "SessionStart missing output"), {});
    assert.match(result.stderr, /structured output|required JSON/i);
  } finally {
    rmSync(fixture.root, { force: true, recursive: true });
  }
});

test("Codex validates structured output only for events with a structured contract", () => {
  const cases = [
    ["SessionEnd", "reflect", "__SILENT__", false],
    ["SessionEnd", "reflect", "not-json", false],
    ["PreCompact", "snapshot", "__SILENT__", true],
    ["PreCompact", "snapshot", "not-json", false],
    ["PostToolUse", "observe", "not-json", true],
    ["SubagentStart", "observe", "not-json", true],
  ];

  for (const [event, subcommand, output, succeeds] of cases) {
    const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");
    try {
      writeCommand(
        fixture.bin,
        "epic-harness",
        `if [ "$EPIC_TEST_OUTPUT" != "__SILENT__" ]; then
  printf '%s\\n' "$EPIC_TEST_OUTPUT"
fi`,
        `if not "%EPIC_TEST_OUTPUT%"=="__SILENT__" echo %EPIC_TEST_OUTPUT%`,
      );
      const result = runScript(
        ["hook", event, subcommand],
        { ...fixture.env, EPIC_TEST_OUTPUT: output },
        JSON.stringify({ hook_event_name: event, session_id: "structured-output" }),
      );

      if (succeeds) {
        assert.equal(result.status, 0, `${event}: ${result.stderr}`);
        assert.equal(result.stdout, "", `${event} must not forward non-structured output`);
      } else {
        assert.notEqual(result.status, 0, `${event}: ${result.stderr}`);
        assert.deepEqual(assertSingleJsonObject(result.stdout, `${event} invalid output`), {});
        assert.match(result.stderr, /required structured output|invalid JSON/i);
      }
    } finally {
      rmSync(fixture.root, { force: true, recursive: true });
    }
  }
});

test("a structured PreToolUse guard denial preserves exit two and deny JSON", () => {
  const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");

  try {
    writeCommand(
      fixture.bin,
      "epic-harness",
      `if [ "$1" = "guard" ]; then
  printf '%s\\n' '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"Guard-specific denial reason"}}'
  exit 2
fi
exit 99`,
      `if "%1"=="guard" (
  echo {"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"Guard-specific denial reason"}}
  exit /b 2
)
exit /b 99`,
    );

    const result = runScript(
      ["hook", "PreToolUse", "guard"],
      fixture.env,
      '{"hook_event_name":"PreToolUse"}',
    );

    assert.equal(result.status, 2, result.stderr);
    assert.equal(
      assertSingleJsonObject(result.stdout, "blocking PreToolUse")
        .hookSpecificOutput.permissionDecision,
      "deny",
    );
    assert.equal(
      assertSingleJsonObject(result.stdout, "blocking PreToolUse")
        .hookSpecificOutput.permissionDecisionReason,
      "Guard-specific denial reason",
    );
    assert.match(result.stderr, /denied the tool request/i);
  } finally {
    rmSync(fixture.root, { force: true, recursive: true });
  }
});

test("a malformed or non-structured guard denial fails without a permission decision", () => {
  const cases = [
    ["malformed JSON", "not-json"],
    ["non-object JSON", "[]"],
    ["missing Codex hook event", '{"hookSpecificOutput":{"permissionDecision":"deny"}}'],
    ["non-denial Codex output", '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow"}}'],
    ["reasonless denial", '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny"}}'],
  ];

  for (const [label, output] of cases) {
    const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");

    try {
      writeCommand(
        fixture.bin,
        "epic-harness",
        `if [ "$1" = "guard" ]; then
  printf '%s\\n' '${output}'
  exit 2
fi
exit 99`,
        `if "%1"=="guard" (
  echo ${output}
  exit /b 2
)
exit /b 99`,
      );

      const result = runScript(
        ["hook", "PreToolUse", "guard"],
        fixture.env,
        '{"hook_event_name":"PreToolUse"}',
      );

      assert.notEqual(result.status, 0, `${label}: ${result.stderr}`);
      assert.notEqual(result.status, 2, `${label}: ${result.stderr}`);
      assert.equal(result.stdout, "", `${label}: no permission decision`);
      assert.match(result.stderr, /invalid guard denial|failed with exit code/i, label);
    } finally {
      rmSync(fixture.root, { force: true, recursive: true });
    }
  }
});

test(
  "the Windows Codex command turns a guard denial into successful structured output",
  { skip: !IS_WINDOWS },
  () => {
    const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");

    try {
      mkdirSync(join(fixture.root, "registry", "scripts"), { recursive: true });
      copyFileSync(
        join(ROOT, "registry", "scripts", "run-hook.cmd"),
        join(fixture.root, "registry", "scripts", "run-hook.cmd"),
      );
      copyFileSync(
        SCRIPT,
        join(fixture.root, "registry", "scripts", "install.js"),
      );
      writeCommand(
        fixture.bin,
        "epic-harness",
        `printf '%s\\n' '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"Windows guard denial"}}'
exit 2`,
        `echo {"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"Windows guard denial"}}
exit /b 2`,
      );
      const manifest = JSON.parse(
        readFileSync(join(ROOT, ".codex-plugin", "hooks.json"), "utf8"),
      );
      const command = manifest.hooks.PreToolUse[0].hooks[0].commandWindows;
      const result = spawnSync(
        "powershell.exe",
        ["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", command],
        {
          encoding: "utf8",
          env: {
            ...fixture.env,
            PLUGIN_ROOT: fixture.root,
            PATH: `${fixture.bin}${delimiter}${dirname(process.execPath)}${delimiter}${process.env.PATH}`,
          },
          input:
            '{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"git push --force origin main"}}',
        },
      );

      assert.equal(
        result.status,
        0,
        `stdout: ${result.stdout}\nstderr: ${result.stderr}\ncommand: ${command}`,
      );
      const output = assertSingleJsonObject(result.stdout, "Windows guard deny");
      assert.equal(
        output.hookSpecificOutput.permissionDecision,
        "deny",
      );
      assert.equal(
        output.hookSpecificOutput.permissionDecisionReason,
        "Windows guard denial",
      );
    } finally {
      rmSync(fixture.root, { force: true, recursive: true });
    }
  },
);

test(
  "the Windows Codex command preserves guard runtime failures without a permission decision",
  { skip: !IS_WINDOWS },
  () => {
    const cases = [
      ["missing runtime", null],
      ["runtime failure", { output: "runtime failure", status: 17 }],
      ["malformed guard denial", { output: "not-json", status: 2 }],
    ];
    const manifest = JSON.parse(
      readFileSync(join(ROOT, ".codex-plugin", "hooks.json"), "utf8"),
    );
    const command = manifest.hooks.PreToolUse[0].hooks[0].commandWindows;

    for (const [label, runtime] of cases) {
      const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");

      try {
        mkdirSync(join(fixture.root, "registry", "scripts"), { recursive: true });
        copyFileSync(
          join(ROOT, "registry", "scripts", "run-hook.cmd"),
          join(fixture.root, "registry", "scripts", "run-hook.cmd"),
        );
        copyFileSync(
          SCRIPT,
          join(fixture.root, "registry", "scripts", "install.js"),
        );
        if (runtime) {
          writeCommand(
            fixture.bin,
            "epic-harness",
            `printf '%s\\n' '${runtime.output}'\nexit ${runtime.status}`,
            `echo ${runtime.output}\nexit /b ${runtime.status}`,
          );
        }

        const result = spawnSync(
          join(
            process.env.SystemRoot,
            "System32",
            "WindowsPowerShell",
            "v1.0",
            "powershell.exe",
          ),
          ["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", command],
          {
            encoding: "utf8",
            env: {
              ...fixture.env,
              PLUGIN_ROOT: fixture.root,
              PATH: `${fixture.bin}${delimiter}${dirname(process.execPath)}${delimiter}${join(process.env.SystemRoot, "System32")}`,
            },
            input:
              '{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"echo wrapper-test"}}',
          },
        );

        assert.equal(result.error, undefined, `${label}: ${result.error?.message}`);
        assert.notEqual(result.status, 0, `${label}: ${result.stderr}`);
        assert.notEqual(result.status, 2, `${label}: ${result.stderr}`);
        assert.equal(result.stdout, "", `${label}: no permission decision`);
      } finally {
        rmSync(fixture.root, { force: true, recursive: true });
      }
    }
  },
);

test("a missing PreToolUse runtime fails without a permission decision", () => {
  const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");

  try {
    const result = runScript(
      ["hook", "PreToolUse", "guard"],
      fixture.env,
      '{"hook_event_name":"PreToolUse"}',
    );

    assert.notEqual(result.status, 0, result.stderr);
    assert.notEqual(result.status, 2, result.stderr);
    assert.equal(result.stdout, "");
    assert.match(result.stderr, /epic-harness.*not found/i);
  } finally {
    rmSync(fixture.root, { force: true, recursive: true });
  }
});
