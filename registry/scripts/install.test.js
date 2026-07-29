import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import {
  chmodSync,
  copyFileSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { spawnSync } from "node:child_process";
import https from "node:https";
import test from "node:test";

import { downloadFile } from "./install.js";

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
const versionParts = PLUGIN_VERSION.split(".").map(Number);
const PREVIOUS_VERSION = `${versionParts[0]}.${versionParts[1]}.${versionParts[2] - 1}`;
const VERSION_PATTERN = PLUGIN_VERSION.replaceAll(".", "\\.");
const PREVIOUS_VERSION_PATTERN = PREVIOUS_VERSION.replaceAll(".", "\\.");
const STRUCTURED_HOOKS = [
  ["SessionStart", "resume"],
  ["PreToolUse", "guard"],
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
  writeFileSync(
    path,
    IS_WINDOWS
      ? `@echo off\r\n${windowsBody}\r\n`
      : `#!/bin/sh\n${unixBody}\n`,
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
  const root = mkdtempSync(join(tmpdir(), "epic-harness-install-test-"));
  const bin = join(root, "bin");
  mkdirSync(join(root, manifestDir), { recursive: true });
  mkdirSync(bin, { recursive: true });
  writeFileSync(
    join(root, manifestDir, "plugin.json"),
    JSON.stringify({ version }),
  );
  writeFileSync(join(root, "runtime-revision.txt"), `${RUNTIME_REVISION}\n`);

  return {
    bin,
    env: {
      ...process.env,
      CLAUDE_PLUGIN_ROOT: "",
      PLUGIN_ROOT: "",
      [environmentKey]: root,
      EPIC_TEST_RUNTIME_REVISION: RUNTIME_REVISION,
      PATH: bin,
    },
    root,
  };
}

function runScript(args, env, input) {
  return spawnSync(process.execPath, [SCRIPT, ...args], {
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

test("installer module is import-safe and exposes its downloader", () => {
  const moduleUrl = pathToFileURL(SCRIPT).href;
  const result = spawnSync(
    process.execPath,
    [
      "--input-type=module",
      "--eval",
      `import { downloadFile } from ${JSON.stringify(moduleUrl)}; process.stdout.write(typeof downloadFile);`,
    ],
    {
      encoding: "utf8",
      env: {
        ...process.env,
        CLAUDE_PLUGIN_ROOT: "",
        PLUGIN_ROOT: "",
      },
    },
  );

  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.stdout, "function");
  assert.equal(result.stderr, "");
});

test("installer download rejects an excessive HTTPS redirect chain", async () => {
  const root = mkdtempSync(join(tmpdir(), "epic-harness-download-test-"));
  const destination = join(root, "installer.sh");
  const originalGet = https.get;
  let requests = 0;

  https.get = (_url, onResponse) => {
    const request = new EventEmitter();
    request.setTimeout = () => request;
    request.destroy = () => {};
    queueMicrotask(() => {
      requests += 1;
      const response = new EventEmitter();
      response.statusCode = requests <= 10 ? 302 : 500;
      response.headers = { location: "https://example.test/next" };
      response.resume = () => {};
      onResponse(response);
    });
    return request;
  };

  try {
    await assert.rejects(
      downloadFile("https://example.test/start", destination),
      /redirect limit/i,
    );
  } finally {
    https.get = originalGet;
    rmSync(root, { force: true, recursive: true });
  }
});

test("installer download times out a stalled HTTPS request", async () => {
  const root = mkdtempSync(join(tmpdir(), "epic-harness-download-test-"));
  const destination = join(root, "installer.sh");
  const originalGet = https.get;

  https.get = () => {
    const request = new EventEmitter();
    let timer;
    request.setTimeout = (milliseconds, onTimeout) => {
      timer = setTimeout(onTimeout, milliseconds);
      return request;
    };
    request.destroy = (error) => {
      clearTimeout(timer);
      queueMicrotask(() => request.emit("error", error));
    };
    return request;
  };

  try {
    const hardDeadline = new Promise((_, reject) => {
      setTimeout(() => reject(new Error("test deadline exceeded")), 100);
    });
    await assert.rejects(
      Promise.race([
        downloadFile("https://example.test/start", destination, {
          requestTimeoutMs: 10,
          totalTimeoutMs: 80,
        }),
        hardDeadline,
      ]),
      /installer request timed out/i,
    );
  } finally {
    https.get = originalGet;
    rmSync(root, { force: true, recursive: true });
  }
});

test("installer download enforces one total deadline across redirects", async () => {
  const root = mkdtempSync(join(tmpdir(), "epic-harness-download-test-"));
  const destination = join(root, "installer.sh");
  const originalGet = https.get;

  https.get = (_url, onResponse) => {
    const request = new EventEmitter();
    let destroyed = false;
    request.setTimeout = () => request;
    request.destroy = () => {
      destroyed = true;
    };
    setTimeout(() => {
      if (destroyed) return;
      const response = new EventEmitter();
      response.statusCode = 302;
      response.headers = { location: "https://example.test/next" };
      response.resume = () => {};
      onResponse(response);
    }, 10);
    return request;
  };

  try {
    const hardDeadline = new Promise((_, reject) => {
      setTimeout(() => reject(new Error("test deadline exceeded")), 150);
    });
    await assert.rejects(
      Promise.race([
        downloadFile("https://example.test/start", destination, {
          requestTimeoutMs: 100,
          totalTimeoutMs: 25,
        }),
        hardDeadline,
      ]),
      /total timeout/i,
    );
  } finally {
    https.get = originalGet;
    rmSync(root, { force: true, recursive: true });
  }
});

for (const [environmentKey, manifestDir] of [
  ["PLUGIN_ROOT", ".codex-plugin"],
  ["CLAUDE_PLUGIN_ROOT", ".claude-plugin"],
]) {
  test(`${environmentKey} parses the real stderr version contract`, () => {
    const fixture = makeFixture(environmentKey, manifestDir);
    const probes = join(fixture.root, "probes.txt");

    try {
      writeCommand(
        fixture.bin,
        "epic-harness",
        `if [ "$1" = "version" ]; then
  printf '%s\\n' version >> "$EPIC_TEST_PROBES"
  printf '%s\\n' 'epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION}' >&2
  exit 0
fi
exit 99`,
        `if "%1"=="version" (
  echo version>>"%EPIC_TEST_PROBES%"
  echo epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION} 1>&2
  exit /b 0
)
exit /b 99`,
      );

      const result = runScript([], {
        ...fixture.env,
        EPIC_TEST_PROBES: probes,
      });

      assert.equal(result.status, 0, result.stderr);
      assert.equal(result.stdout, "");
      assert.equal(result.stderr, "", "a compatible runtime must be quiet");
      assert.match(readFileSync(probes, "utf8"), /^version\r?\nversion\r?\n$/);
    } finally {
      rmSync(fixture.root, { force: true, recursive: true });
    }
  });

  test(`${environmentKey} installs and verifies an exact newer runtime`, () => {
    const fixture = makeFixture(environmentKey, manifestDir);
    const calls = join(fixture.root, "calls.txt");
    const versionFile = join(fixture.root, "version.txt");
    writeFileSync(versionFile, PREVIOUS_VERSION);

    try {
      writeCommand(
        fixture.bin,
        "epic-harness",
        `if [ "$1" = "version" ]; then
  IFS= read -r version < "$EPIC_TEST_VERSION_FILE"
  printf 'epic-harness %s runtime-revision %s\\n' "$version" "$EPIC_TEST_RUNTIME_REVISION" >&2
  exit 0
fi
printf '%s\\n' "$*" >> "$EPIC_TEST_CALLS"`,
        `if "%1"=="version" (
  for /f "usebackq delims=" %%v in ("%EPIC_TEST_VERSION_FILE%") do echo epic-harness %%v runtime-revision %EPIC_TEST_RUNTIME_REVISION% 1>&2
  exit /b 0
)
echo %*>>"%EPIC_TEST_CALLS%"`,
      );
      writeCommand(
        fixture.bin,
        "brew",
        "exit 1",
        "exit /b 1",
      );
      writeCommand(
        fixture.bin,
        "cargo",
        `if [ "$1" = "binstall" ] && [ "$2" = "--version" ]; then
  exit 0
fi
printf '%s\\n' "$*" >> "$EPIC_TEST_CALLS"
printf '%s\\n' '${PLUGIN_VERSION}' > "$EPIC_TEST_VERSION_FILE"`,
        `if "%1"=="binstall" if "%2"=="--version" exit /b 0
echo %*>>"%EPIC_TEST_CALLS%"
> "%EPIC_TEST_VERSION_FILE%" echo ${PLUGIN_VERSION}`,
      );

      const result = runScript([], {
        ...fixture.env,
        EPIC_TEST_CALLS: calls,
        EPIC_TEST_VERSION_FILE: versionFile,
      });

      assert.equal(result.status, 0, result.stderr);
      assert.equal(result.stdout, "");
      assert.match(
        readFileSync(calls, "utf8"),
        new RegExp(`binstall epic-harness@${VERSION_PATTERN} --no-confirm`),
      );
      assert.match(
        result.stderr,
        new RegExp(
          `Updating epic-harness ${PREVIOUS_VERSION_PATTERN} → ${VERSION_PATTERN}`,
        ),
      );
      assert.match(
        result.stderr,
        new RegExp(`Updated to ${VERSION_PATTERN}`),
      );
    } finally {
      rmSync(fixture.root, { force: true, recursive: true });
    }
  });
}

test("bootstrap fails when an installer reports success without a compatible binary", () => {
  const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");

  try {
    writeCommand(
      fixture.bin,
      "cargo",
      `if [ "$1" = "binstall" ] && [ "$2" = "--version" ]; then
  exit 0
fi
exit 0`,
      `if "%1"=="binstall" if "%2"=="--version" exit /b 0
exit /b 0`,
    );

    const result = runScript([], fixture.env);

    assert.notEqual(result.status, 0);
    assert.equal(result.stdout, "");
    assert.match(
      result.stderr,
      new RegExp(
        `required epic-harness ${VERSION_PATTERN} is unavailable after installation`,
        "i",
      ),
    );
  } finally {
    rmSync(fixture.root, { force: true, recursive: true });
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
  printf '%s\\n' 'epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION}' >&2
  exit 0
fi
printf '%s\\n' "$*" >> "$EPIC_TEST_CALLS"
printf '%s\\n' '{}'`,
      `if "%1"=="version" (
  echo epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION} 1>&2
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

test("bootstrap rejects versions outside the base or Codex-cachebuster contract", () => {
  for (const version of [
    `${PLUGIN_VERSION}+other.20260728181552`,
    `${PLUGIN_VERSION}+codex.`,
    `${PLUGIN_VERSION}+codex.bad..token`,
  ]) {
    const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin", version);

    try {
      const result = runScript([], fixture.env);

      assert.notEqual(result.status, 0);
      assert.match(result.stderr, /plugin manifest has an invalid version/i);
    } finally {
      rmSync(fixture.root, { force: true, recursive: true });
    }
  }
});

test("a same-version runtime with a different revision is reinstalled and verified", () => {
  const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");
  const calls = join(fixture.root, "calls.txt");
  const revisionFile = join(fixture.root, "revision.txt");
  const staleRevision = RUNTIME_REVISION === "1" ? "2" : "1";
  writeFileSync(revisionFile, `${staleRevision}\n`);

  try {
    writeCommand(
      fixture.bin,
      "epic-harness",
      `if [ "$1" = "version" ]; then
  IFS= read -r revision < "$EPIC_TEST_REVISION_FILE"
  printf 'epic-harness ${PLUGIN_VERSION} runtime-revision %s\\n' "$revision" >&2
  exit 0
fi
exit 99`,
      `if "%1"=="version" (
  for /f "usebackq delims=" %%v in ("%EPIC_TEST_REVISION_FILE%") do echo epic-harness ${PLUGIN_VERSION} runtime-revision %%v 1>&2
  exit /b 0
)
exit /b 99`,
    );
    writeCommand(
      fixture.bin,
      "cargo",
      `if [ "$1" = "binstall" ] && [ "$2" = "--version" ]; then
  exit 0
fi
printf '%s\\n' "$*" >> "$EPIC_TEST_CALLS"
printf '%s\\n' '${RUNTIME_REVISION}' > "$EPIC_TEST_REVISION_FILE"`,
      `if "%1"=="binstall" if "%2"=="--version" exit /b 0
echo %*>>"%EPIC_TEST_CALLS%"
> "%EPIC_TEST_REVISION_FILE%" echo ${RUNTIME_REVISION}`,
    );

    const result = runScript([], {
      ...fixture.env,
      EPIC_TEST_CALLS: calls,
      EPIC_TEST_REVISION_FILE: revisionFile,
    });

    assert.equal(result.status, 0, result.stderr);
    assert.match(readFileSync(calls, "utf8"), new RegExp(`binstall epic-harness@${VERSION_PATTERN} --no-confirm`));
    assert.match(result.stderr, new RegExp(`revision ${staleRevision}`));
  } finally {
    rmSync(fixture.root, { force: true, recursive: true });
  }
});

test("SessionStart does not resume with an incompatible same-path runtime", () => {
  const fixture = makeFixture("PLUGIN_ROOT", ".codex-plugin");
  const calls = join(fixture.root, "calls.txt");

  try {
    writeCommand(
      fixture.bin,
      "epic-harness",
      `if [ "$1" = "version" ]; then
  printf '%s\\n' 'epic-harness ${PREVIOUS_VERSION} runtime-revision ${RUNTIME_REVISION}' >&2
  exit 0
fi
printf '%s\\n' "$*" >> "$EPIC_TEST_CALLS"`,
      `if "%1"=="version" (
  echo epic-harness ${PREVIOUS_VERSION} runtime-revision ${RUNTIME_REVISION} 1>&2
  exit /b 0
)
echo %*>>"%EPIC_TEST_CALLS%"`,
    );
    writeCommand(
      fixture.bin,
      "cargo",
      `if [ "$1" = "binstall" ] && [ "$2" = "--version" ]; then
  exit 0
fi
exit 0`,
      `if "%1"=="binstall" if "%2"=="--version" exit /b 0
exit /b 0`,
    );

    const result = runScript(["hook", "SessionStart", "resume"], {
      ...fixture.env,
      EPIC_TEST_CALLS: calls,
    });

    assert.notEqual(result.status, 0);
    assert.deepEqual(
      assertSingleJsonObject(result.stdout, "SessionStart bootstrap failure"),
      {},
    );
    assert.throws(() => readFileSync(calls, "utf8"));
    assert.match(
      result.stderr,
      new RegExp(`required epic-harness ${VERSION_PATTERN}`, "i"),
    );
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
      if (event === "SessionStart") {
        writeCommand(
          fixture.bin,
          "cargo",
          `if [ "$1" = "binstall" ] && [ "$2" = "--version" ]; then
  exit 0
fi
if [ "$1" = "binstall" ]; then
  exit 0
fi
exit 99`,
          `if "%1"=="binstall" if "%2"=="--version" exit /b 0
if "%1"=="binstall" exit /b 0
exit /b 99`,
        );
      }

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
  printf '%s\\n' 'epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION}' >&2
  exit 0
fi
printf '%s\\n' 'runtime ${event} failure' >&2
exit 17`,
        `if "%1"=="version" (
  echo epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION} 1>&2
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

test("Codex runner stamps every supported hook payload with explicit host provenance", () => {
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
  printf '%s\\n' 'epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION}' >&2
  exit 0
fi
IFS= read -r EPIC_STDIN || true
printf '%s\\n' "$EPIC_STDIN" > "$EPIC_TEST_STDIN"`,
      `if "%1"=="version" (
  echo epic-harness ${PLUGIN_VERSION} runtime-revision ${RUNTIME_REVISION} 1>&2
  exit /b 0
)
set /p EPIC_STDIN=
> "%EPIC_TEST_STDIN%" echo %EPIC_STDIN%`,
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
        { ...JSON.parse(input), host: "codex" },
        event,
      );
      if (event === "SubagentStop") {
        assert.deepEqual(JSON.parse(result.stdout), {});
      } else {
        assert.equal(result.stdout, "", event);
      }
    }
  } finally {
    rmSync(fixture.root, { force: true, recursive: true });
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
