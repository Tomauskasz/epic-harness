import assert from "node:assert/strict";
import {
  copyFileSync,
  cpSync,
  existsSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import os from "node:os";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import test from "node:test";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const BUNDLE_MANIFEST_PATH = join(
  ROOT,
  "registry",
  "scripts",
  "bundle-manifest.json",
);
const BUNDLE_GENERATOR_PATH = join(
  ROOT,
  "registry",
  "scripts",
  "generate-bundle-manifest.js",
);
const CODEX_PATH = join(ROOT, ".codex-plugin", "hooks.json");
const CLAUDE_PATH = join(ROOT, "hooks", "hooks.json");
const CODEX = JSON.parse(readFileSync(CODEX_PATH, "utf8"));
const CLAUDE = JSON.parse(readFileSync(CLAUDE_PATH, "utf8"));
const CODEX_CONTRACT = [
  ["SessionStart", [["*", "resume"]]],
  ["PreToolUse", [["Bash", "guard"], ["apply_patch", "guard"]]],
  ["PostToolUse", [["*", "observe"], ["apply_patch", "polish"]]],
  ["SubagentStart", [["*", "observe"]]],
  ["SubagentStop", [["*", "observe"]]],
  ["PreCompact", [["*", "snapshot"]]],
  ["SessionEnd", [["*", "reflect", { timeout: 3 }]]],
];

const CLAUDE_CONTRACT = [
  ["SessionStart", [["*", "resume", { async: false }]]],
  [
    "PreToolUse",
    [
      ["Bash", "guard"],
      ["Agent", "observe", {}, "SubagentStart"],
      ["Edit|Write|MultiEdit|NotebookEdit", "guard"],
    ],
  ],
  [
    "PostToolUse",
    [
      ["Edit|Write|MultiEdit|NotebookEdit", "polish"],
      ["*", "observe", { async: true, timeout: 5 }],
    ],
  ],
  ["PostToolUseFailure", [["*", "observe", { async: true, timeout: 5 }]]],
  ["PreCompact", [["*", "snapshot"]]],
  ["SessionEnd", [["*", "reflect"]]],
];

function handlers(manifest) {
  return Object.entries(manifest.hooks).flatMap(([event, groups]) =>
    groups.flatMap((group) =>
      group.hooks.map((handler) => ({ event, group, handler })),
    ),
  );
}

function runBundleGenerator(root, ...args) {
  return spawnSync(process.execPath, [
    BUNDLE_GENERATOR_PATH,
    "--root",
    root,
    ...args,
  ], {
    cwd: ROOT,
    encoding: "utf8",
  });
}

function relativePath(root, path) {
  return relative(root, path).replaceAll("\\", "/");
}

function regularFiles(root, directory) {
  const files = [];
  const visit = (current) => {
    for (const entry of readdirSync(current, { withFileTypes: true })) {
      const path = join(current, entry.name);
      const metadata = lstatSync(path);
      assert.ok(!metadata.isSymbolicLink(), `${relativePath(root, path)} must not be a symlink`);
      if (metadata.isDirectory()) {
        visit(path);
      } else if (metadata.isFile()) {
        files.push(relativePath(root, path));
      }
    }
  };
  visit(directory);
  return files.sort();
}

function pluginTargetFiles(root, target) {
  const relativeTarget = target.replace(/^\.\//, "").replace(/\/$/, "");
  const path = join(root, relativeTarget);
  assert.ok(existsSync(path), `plugin target ${target} must exist`);
  return lstatSync(path).isDirectory()
    ? regularFiles(root, path)
    : [relativePath(root, path)];
}

function hookRunnerFiles(root) {
  const paths = new Set();
  for (const manifest of [CODEX, CLAUDE]) {
    for (const { handler } of handlers(manifest)) {
      for (const command of [handler.command, handler.commandWindows].filter(Boolean)) {
        for (const match of command.matchAll(/(registry[\\/]scripts[\\/][A-Za-z0-9._-]+)/g)) {
          paths.add(match[1].replaceAll("\\", "/"));
        }
      }
    }
  }
  for (const path of paths) {
    assert.ok(existsSync(join(root, path)), `hook runner target ${path} must exist`);
  }
  return [...paths].sort();
}

function packageDeclaresPath(packageJson, path) {
  return packageJson.files.some((entry) => {
    const declared = entry.replace(/\/$/, "");
    return path === declared || path.startsWith(`${declared}/`);
  });
}

// Derive expected files from package reachability and the two host manifests.
// This deliberately does not read bundle-spec.json or a producer-side list.
function independentlyReachableRuntimeFiles(root) {
  const packageJson = JSON.parse(readFileSync(join(root, "package.json"), "utf8"));
  const files = new Set(["package.json"]);
  const pluginPaths = [
    ".claude-plugin/plugin.json",
    ".codex-plugin/plugin.json",
  ];
  for (const pluginPath of pluginPaths) {
    files.add(pluginPath);
    const plugin = JSON.parse(readFileSync(join(root, pluginPath), "utf8"));
    for (const target of [plugin.skills, plugin.mcpServers, plugin.hooks].filter(Boolean)) {
      for (const path of pluginTargetFiles(root, target)) files.add(path);
    }
  }

  // Claude discovers this manifest at a fixed package path rather than from
  // plugin.json, so prove that it is packaged and reachable separately.
  const claudeHooks = "hooks/hooks.json";
  assert.ok(existsSync(join(root, claudeHooks)), "Claude hook manifest must exist");
  files.add(claudeHooks);
  for (const path of hookRunnerFiles(root)) files.add(path);

  const runner = readFileSync(join(root, "registry", "scripts", "install.js"), "utf8");
  assert.match(runner, /runtime-revision\.txt/);
  assert.match(runner, /bundle-manifest\.json/);
  files.add("runtime-revision.txt");

  assert.ok(packageJson.files.includes("registry/presets/"));
  for (const path of regularFiles(root, join(root, "registry", "presets"))) {
    files.add(path);
  }

  for (const path of files) {
    assert.ok(
      path === "package.json" || packageDeclaresPath(packageJson, path),
      `${path} must be reachable from package.json files`,
    );
  }
  return [...files].sort();
}

function copyFixtureFile(fixture, path) {
  const target = join(fixture, path);
  mkdirSync(dirname(target), { recursive: true });
  copyFileSync(join(ROOT, path), target);
}

function makeBundleFixture() {
  const fixture = mkdtempSync(join(os.tmpdir(), "epic-harness-bundle-"));
  cpSync(join(ROOT, "src"), join(fixture, "src"), { recursive: true });
  for (const path of [
    "Cargo.toml",
    "Cargo.lock",
    "build.rs",
    "registry/scripts/bundle-spec.json",
    ...independentlyReachableRuntimeFiles(ROOT),
  ]) {
    copyFixtureFile(fixture, path);
  }
  return fixture;
}

test("matcher groups contain only supported matcher and hooks fields", () => {
  for (const [name, manifest] of [
    ["Codex", CODEX],
    ["Claude", CLAUDE],
  ]) {
    for (const [event, groups] of Object.entries(manifest.hooks)) {
      for (const group of groups) {
        assert.deepEqual(
          Object.keys(group).sort(),
          ["hooks", "matcher"],
          `${name} ${event} has unsupported matcher-group fields`,
        );
      }
    }
  }
});

test("Codex uses the Node runner and Windows status wrapper for every hook", () => {
  for (const { event, handler } of handlers(CODEX)) {
    assert.equal(handler.type, "command");
    assert.match(
      handler.command,
      /^node "\$\{PLUGIN_ROOT\}\/registry\/scripts\/install\.js" hook /,
      `${event} must quote PLUGIN_ROOT and use the Node runner`,
    );
    assert.match(
      handler.commandWindows,
      /^cmd\.exe \/d \/s \/c call "%PLUGIN_ROOT%\\registry\\scripts\\run-hook\.cmd" /,
      `${event} must define a Windows status-preserving wrapper`,
    );
    assert.doesNotMatch(handler.command, /(?:^|[;&|])\s*epic(?:-harness)?\b/);
    assert.doesNotMatch(
      handler.commandWindows,
      /(?:^|[;&|])\s*epic(?:-harness)?\b/,
    );
  }
});

test("Claude uses the same quoted Node runner and binary contract", () => {
  for (const { event, handler } of handlers(CLAUDE)) {
    assert.match(
      handler.command,
      /^node "\$\{CLAUDE_PLUGIN_ROOT\}\/registry\/scripts\/install\.js" hook /,
      `${event} must quote CLAUDE_PLUGIN_ROOT and use the Node runner`,
    );
    assert.doesNotMatch(handler.command, /(?:^|[;&|])\s*epic(?:-harness)?\b/);
  }
});

function assertManifestContract(name, manifest, contract, rootVariable) {
  assert.deepEqual(
    Object.keys(manifest.hooks),
    contract.map(([event]) => event),
    `${name} event order changed`,
  );

  for (const [event, expectedGroups] of contract) {
    const groups = manifest.hooks[event];
    assert.equal(groups.length, expectedGroups.length, `${name} ${event} group count`);

    for (const [index, [matcher, subcommand, properties = {}, runnerEvent = event]] of expectedGroups.entries()) {
      const group = groups[index];
      assert.equal(group.matcher, matcher, `${name} ${event} matcher ${index}`);
      assert.equal(group.hooks.length, 1, `${name} ${event} handler count ${index}`);

      const handler = group.hooks[0];
      assert.equal(handler.type, "command", `${name} ${event} handler type ${index}`);
      assert.equal(
        handler.command,
        `node "\${${rootVariable}}/registry/scripts/install.js" hook ${runnerEvent} ${subcommand}`,
        `${name} ${event} command ${index}`,
      );
      if (name === "Codex") {
        assert.equal(
          handler.commandWindows,
          `cmd.exe /d /s /c call "%${rootVariable}%\\registry\\scripts\\run-hook.cmd" ${runnerEvent} ${subcommand}`,
          `${name} ${event} Windows command ${index}`,
        );
      }

      for (const [key, value] of Object.entries(properties)) {
        assert.equal(handler[key], value, `${name} ${event} ${key} ${index}`);
      }
      const expectedKeys = [
        "command",
        "type",
        ...(name === "Codex" ? ["commandWindows"] : []),
        ...Object.keys(properties),
      ].sort();
      assert.deepEqual(
        Object.keys(handler).sort(),
        expectedKeys,
        `${name} ${event} handler properties ${index}`,
      );
    }
  }
}

test("Codex manifest has the exact lifecycle matcher, handler, and timeout contract", () => {
  assertManifestContract("Codex", CODEX, CODEX_CONTRACT, "PLUGIN_ROOT");
});

test("Codex guard and polish cover the same edit tools", () => {
  const editMatcherFor = (event, subcommand) => {
    const matchers =
      CODEX.hooks[event]
        .filter((group) =>
          group.hooks.some((handler) =>
            handler.command.endsWith(` ${subcommand}`),
          ),
        )
        .map((group) => group.matcher)
        .filter((matcher) => matcher.split("|").includes("apply_patch"));

    assert.equal(matchers.length, 1, `${event} ${subcommand} edit matcher`);
    return matchers[0];
  };

  assert.equal(
    editMatcherFor("PreToolUse", "guard"),
    editMatcherFor("PostToolUse", "polish"),
  );
});

test("Claude manifest has the exact lifecycle matcher, handler, and timeout contract", () => {
  assertManifestContract("Claude", CLAUDE, CLAUDE_CONTRACT, "CLAUDE_PLUGIN_ROOT");
});

test("manifest component paths resolve inside the plugin root", () => {
  for (const path of [
    JSON.parse(
      readFileSync(join(ROOT, ".codex-plugin", "plugin.json"), "utf8"),
    ).hooks,
    "./skills/",
    "./mcp_config.json",
  ]) {
    const resolved = resolve(ROOT, path);
    assert.ok(
      resolved === ROOT ||
        resolved.startsWith(
          `${ROOT}${process.platform === "win32" ? "\\" : "/"}`,
        ),
      `${path} escapes the plugin root`,
    );
    assert.ok(existsSync(resolved), `${path} does not exist`);
  }
});

test("hooks and the memory MCP use one executable authority", () => {
  const runner = readFileSync(
    join(ROOT, "registry", "scripts", "install.js"),
    "utf8",
  );
  const mcp = JSON.parse(readFileSync(join(ROOT, "mcp_config.json"), "utf8"));

  assert.match(runner, /const BINARY = "epic-harness";/);
  assert.equal(
    mcp.mcpServers["harness-mem"].command,
    "epic-harness",
    "hooks and harness-mem must not resolve different PATH executables",
  );
});

test("all package and plugin version owners agree", () => {
  const versions = new Map([
    [
      "Cargo.toml",
      readFileSync(join(ROOT, "Cargo.toml"), "utf8").match(
        /^\[package\][\s\S]*?^version = "([^"]+)"/m,
      )?.[1],
    ],
    [
      "Cargo.lock",
      readFileSync(join(ROOT, "Cargo.lock"), "utf8").match(
        /\[\[package\]\]\r?\nname = "epic-harness"\r?\nversion = "([^"]+)"/,
      )?.[1],
    ],
    [
      "package.json",
      JSON.parse(readFileSync(join(ROOT, "package.json"), "utf8")).version,
    ],
    [
      "app/package.json",
      JSON.parse(readFileSync(join(ROOT, "app", "package.json"), "utf8"))
        .version,
    ],
    [
      ".codex-plugin/plugin.json",
      JSON.parse(
        readFileSync(join(ROOT, ".codex-plugin", "plugin.json"), "utf8"),
      ).version,
    ],
    [
      ".claude-plugin/plugin.json",
      JSON.parse(
        readFileSync(join(ROOT, ".claude-plugin", "plugin.json"), "utf8"),
      ).version,
    ],
  ]);

  assert.equal(new Set(versions.values()).size, 1, JSON.stringify([...versions]));
});

test("canonical runtime revision is a positive integer", () => {
  const revision = readFileSync(join(ROOT, "runtime-revision.txt"), "utf8");
  assert.match(revision, /^[1-9]\d*\r?\n$/);
});

test("one declarative bundle inventory names the complete host runtime closure", () => {
  const spec = JSON.parse(
    readFileSync(join(ROOT, "registry", "scripts", "bundle-spec.json"), "utf8"),
  );

  assert.equal(spec.schema_version, 1);
  assert.equal(spec.logical.manifest_path, "registry/scripts/bundle-manifest.json");
  assert.equal(spec.outer.target_executable.selector, "target-executable-v1");
  assert.ok(
    spec.logical.artifacts.some((entry) => entry.path === "skills"),
    "the skill tree must be part of the declared runtime closure",
  );
  assert.ok(
    spec.logical.artifacts.some((entry) => entry.path === "registry/presets"),
    "the preset tree must be part of the declared runtime closure",
  );
  assert.ok(
    spec.logical.artifacts.some((entry) => entry.path === "hooks/hooks.json"),
    "the Claude hook manifest must be part of the declared runtime closure",
  );

  const declaredSelectors = new Set(spec.logical.artifacts.map((entry) => entry.path));
  for (const path of independentlyReachableRuntimeFiles(ROOT)) {
    assert.ok(
      [...declaredSelectors].some(
        (selector) => path === selector || path.startsWith(`${selector}/`),
      ),
      `${path} is reachable from a package or host manifest but absent from bundle-spec.json`,
    );
  }
});

test("checked bundle manifest independently binds each runtime artifact and build input", () => {
  const check = runBundleGenerator(ROOT, "--check");
  assert.equal(check.status, 0, check.stderr);

  const manifest = JSON.parse(readFileSync(BUNDLE_MANIFEST_PATH, "utf8"));
  assert.deepEqual(Object.keys(manifest).sort(), [
    "artifacts",
    "build_identity",
    "identity_inputs",
    "manifest_kind",
    "release_version",
    "runtime_revision",
    "schema_version",
  ]);
  assert.equal(manifest.schema_version, 1);
  assert.equal(manifest.manifest_kind, "logical-runtime-v1");
  assert.equal(
    manifest.release_version,
    JSON.parse(readFileSync(join(ROOT, "package.json"), "utf8")).version,
  );
  assert.equal(
    manifest.runtime_revision,
    readFileSync(join(ROOT, "runtime-revision.txt"), "utf8").trim(),
  );
  assert.match(manifest.build_identity, /^sha256:[a-f0-9]{64}$/);
  assert.deepEqual(Object.keys(manifest.identity_inputs).sort(), [
    "algorithm",
    "artifact_paths",
    "inventory_path",
    "source_paths",
  ]);
  assert.equal(
    manifest.identity_inputs.algorithm,
    "sha256-framed-logical-source-and-artifact-projection-v3",
  );
  assert.equal(manifest.identity_inputs.inventory_path, "registry/scripts/bundle-spec.json");

  const reachable = independentlyReachableRuntimeFiles(ROOT);
  assert.deepEqual(manifest.artifacts.map((artifact) => artifact.path), reachable);
  assert.equal(new Set(reachable).size, reachable.length, "closure must not duplicate paths");
  for (const artifact of manifest.artifacts) {
    assert.deepEqual(Object.keys(artifact).sort(), [
      "digest_mode",
      "path",
      "sha256",
      "type",
    ]);
    assert.equal(artifact.type, "file");
    assert.match(artifact.sha256, /^sha256:[a-f0-9]{64}$/);
  }

  assert.ok(
    manifest.identity_inputs.source_paths.includes("registry/scripts/bundle-spec.json"),
    "the inventory must be a build-identity input",
  );
  assert.deepEqual(manifest.identity_inputs.artifact_paths, reachable);
});

test("logical build identity normalizes source line endings but preserves semantic changes", (t) => {
  const lfFixture = makeBundleFixture();
  const crlfFixture = makeBundleFixture();
  t.after(() => {
    rmSync(lfFixture, { recursive: true, force: true });
    rmSync(crlfFixture, { recursive: true, force: true });
  });

  const lfGenerated = runBundleGenerator(lfFixture, "--write");
  assert.equal(lfGenerated.status, 0, lfGenerated.stderr);
  const lfManifest = JSON.parse(
    readFileSync(join(lfFixture, "registry", "scripts", "bundle-manifest.json"), "utf8"),
  );

  for (const path of lfManifest.identity_inputs.source_paths) {
    const source = join(crlfFixture, path);
    const text = readFileSync(source, "utf8").replace(/\r\n?/g, "\n");
    writeFileSync(source, text.replace(/\n/g, "\r\n"), "utf8");
  }
  const loneCrSource = join(crlfFixture, "build.rs");
  const loneCrText = readFileSync(loneCrSource, "utf8").replace(/\r\n?/g, "\n");
  writeFileSync(loneCrSource, loneCrText.replace(/\n/g, "\r"), "utf8");
  const crlfGenerated = runBundleGenerator(crlfFixture, "--write");
  assert.equal(crlfGenerated.status, 0, crlfGenerated.stderr);
  const crlfManifest = JSON.parse(
    readFileSync(join(crlfFixture, "registry", "scripts", "bundle-manifest.json"), "utf8"),
  );
  assert.equal(
    crlfManifest.build_identity,
    lfManifest.build_identity,
    "equivalent LF and CRLF source projections must have one build identity",
  );

  const semanticSource = crlfManifest.identity_inputs.source_paths.find(
    (path) => path.startsWith("src/") && path.endsWith(".rs"),
  );
  assert.ok(semanticSource, "the fixture must contain a Rust source identity input");
  const semanticPath = join(crlfFixture, semanticSource);
  const semanticText = readFileSync(semanticPath, "utf8");
  const semanticEdit = `${semanticText}\r\nconst _IDENTITY_SEMANTIC_EDIT: usize = 1;\r\n`;
  writeFileSync(semanticPath, semanticEdit, "utf8");
  const semanticGenerated = runBundleGenerator(crlfFixture, "--write");
  assert.equal(semanticGenerated.status, 0, semanticGenerated.stderr);
  const semanticManifest = JSON.parse(
    readFileSync(join(crlfFixture, "registry", "scripts", "bundle-manifest.json"), "utf8"),
  );
  assert.notEqual(
    semanticManifest.build_identity,
    lfManifest.build_identity,
    "a non-EOL source edit must change the build identity",
  );
});

test("logical bundle closure rejects drift until it is regenerated", (t) => {
  const fixture = makeBundleFixture();
  t.after(() => rmSync(fixture, { recursive: true, force: true }));

  const absent = runBundleGenerator(fixture, "--check");
  assert.notEqual(absent.status, 0, "a checked manifest must not be invented");
  assert.match(absent.stderr, /cannot read logical bundle manifest/);

  const generated = runBundleGenerator(fixture, "--write");
  assert.equal(generated.status, 0, generated.stderr);
  const clean = runBundleGenerator(fixture, "--check");
  assert.equal(clean.status, 0, clean.stderr);

  const codexPlugin = join(fixture, ".codex-plugin", "plugin.json");
  const cachedPlugin = JSON.parse(readFileSync(codexPlugin, "utf8"));
  cachedPlugin.version = `${cachedPlugin.version}+codex.cachebuster.1`;
  writeFileSync(codexPlugin, `\r\n${JSON.stringify(cachedPlugin)}\r\n`);
  const semanticallyMaterialized = runBundleGenerator(fixture, "--check");
  assert.equal(
    semanticallyMaterialized.status,
    0,
    semanticallyMaterialized.stderr,
  );

  const runner = join(fixture, "registry", "scripts", "install.js");
  const runnerBytes = readFileSync(runner);
  runnerBytes[0] ^= 1;
  writeFileSync(runner, runnerBytes);
  const runnerDrift = runBundleGenerator(fixture, "--check");
  assert.notEqual(runnerDrift.status, 0, "one runner byte must invalidate the manifest");
  assert.match(runnerDrift.stderr, /artifact sha256 mismatch: registry\/scripts\/install\.js/);

  assert.equal(runBundleGenerator(fixture, "--write").status, 0);
  assert.equal(runBundleGenerator(fixture, "--check").status, 0);

  const source = join(fixture, "src", "main.rs");
  const sourceBytes = readFileSync(source);
  sourceBytes[0] ^= 1;
  writeFileSync(source, sourceBytes);
  const sourceDrift = runBundleGenerator(fixture, "--check");
  assert.notEqual(sourceDrift.status, 0, "one source byte must invalidate the manifest");
  assert.match(sourceDrift.stderr, /build_identity mismatch/);

  assert.equal(runBundleGenerator(fixture, "--write").status, 0);
  const regenerated = runBundleGenerator(fixture, "--check");
  assert.equal(regenerated.status, 0, regenerated.stderr);
});

test("logical inventory rejects omitted host closure files and unsafe selectors", (t) => {
  const omissionPaths = [
    "hooks/hooks.json",
    independentlyReachableRuntimeFiles(ROOT).find((path) => path.startsWith("skills/")),
    independentlyReachableRuntimeFiles(ROOT).find((path) => path.startsWith("registry/presets/")),
  ];
  for (const path of omissionPaths) {
    assert.ok(path, "the fixture must contain each required closure class");
    const fixture = makeBundleFixture();
    t.after(() => rmSync(fixture, { recursive: true, force: true }));
    assert.equal(runBundleGenerator(fixture, "--write").status, 0);
    rmSync(join(fixture, path));
    const check = runBundleGenerator(fixture, "--check");
    assert.notEqual(check.status, 0, `${path} must invalidate the declared closure`);
    assert.match(check.stderr, /cannot inspect|selected no files|artifact path projection mismatch/);
  }

  const duplicate = makeBundleFixture();
  t.after(() => rmSync(duplicate, { recursive: true, force: true }));
  const duplicateSpec = JSON.parse(
    readFileSync(join(duplicate, "registry", "scripts", "bundle-spec.json"), "utf8"),
  );
  duplicateSpec.logical.artifacts.push({ ...duplicateSpec.logical.artifacts[0] });
  writeFileSync(
    join(duplicate, "registry", "scripts", "bundle-spec.json"),
    JSON.stringify(duplicateSpec),
  );
  const duplicateCheck = runBundleGenerator(duplicate, "--check");
  assert.notEqual(duplicateCheck.status, 0);
  assert.match(duplicateCheck.stderr, /duplicate logical artifact path/);

  const traversal = makeBundleFixture();
  t.after(() => rmSync(traversal, { recursive: true, force: true }));
  const traversalSpec = JSON.parse(
    readFileSync(join(traversal, "registry", "scripts", "bundle-spec.json"), "utf8"),
  );
  traversalSpec.logical.artifacts[0].path = "../runtime-revision.txt";
  writeFileSync(
    join(traversal, "registry", "scripts", "bundle-spec.json"),
    JSON.stringify(traversalSpec),
  );
  const traversalCheck = runBundleGenerator(traversal, "--check");
  assert.notEqual(traversalCheck.status, 0);
  assert.match(traversalCheck.stderr, /traversal|unsupported path/);

  const unpackaged = makeBundleFixture();
  t.after(() => rmSync(unpackaged, { recursive: true, force: true }));
  const unpackagedPackage = JSON.parse(readFileSync(join(unpackaged, "package.json"), "utf8"));
  unpackagedPackage.files = unpackagedPackage.files.filter(
    (entry) => entry !== "registry/presets/",
  );
  writeFileSync(join(unpackaged, "package.json"), JSON.stringify(unpackagedPackage));
  const unpackagedCheck = runBundleGenerator(unpackaged, "--check");
  assert.notEqual(unpackagedCheck.status, 0);
  assert.match(unpackagedCheck.stderr, /package\.json files does not ship required runtime path/);

  const symlinkStyle = makeBundleFixture();
  t.after(() => rmSync(symlinkStyle, { recursive: true, force: true }));
  const symlinkStyleSpec = JSON.parse(
    readFileSync(join(symlinkStyle, "registry", "scripts", "bundle-spec.json"), "utf8"),
  );
  symlinkStyleSpec.logical.artifacts[0].path = "skills\\linked.md";
  writeFileSync(
    join(symlinkStyle, "registry", "scripts", "bundle-spec.json"),
    JSON.stringify(symlinkStyleSpec),
  );
  const symlinkStyleCheck = runBundleGenerator(symlinkStyle, "--check");
  assert.notEqual(symlinkStyleCheck.status, 0);
  assert.match(symlinkStyleCheck.stderr, /backslash path separator/);

  const actualSymlink = makeBundleFixture();
  t.after(() => rmSync(actualSymlink, { recursive: true, force: true }));
  const link = join(actualSymlink, "skills", "symlinked-skill.md");
  try {
    symlinkSync(join(actualSymlink, "skills", "tdd", "SKILL.md"), link, "file");
  } catch (error) {
    t.diagnostic(`platform cannot create a file symlink: ${error.code}`);
    return;
  }
  const actualSymlinkCheck = runBundleGenerator(actualSymlink, "--check");
  assert.notEqual(actualSymlinkCheck.status, 0);
  assert.match(actualSymlinkCheck.stderr, /symbolic-link-style entry/);
});

test("post-link outer bundle binds executable bytes and every materialized file", (t) => {
  const fixture = makeBundleFixture();
  t.after(() => rmSync(fixture, { recursive: true, force: true }));
  assert.equal(runBundleGenerator(fixture, "--write").status, 0);

  const executable = join(fixture, "bin", "epic-harness");
  mkdirSync(dirname(executable), { recursive: true });
  writeFileSync(executable, Buffer.from("first executable bytes"));
  const outer = join(os.tmpdir(), `epic-harness-outer-${Date.now()}-${Math.random()}.json`);
  t.after(() => rmSync(outer, { force: true }));

  const outerArgs = [
    "--outer",
    "--materialized-root",
    fixture,
    "--executable",
    "bin/epic-harness",
    "--target",
    "x86_64-pc-windows-msvc",
    "--output",
    outer,
  ];
  const missingExecutable = runBundleGenerator(fixture, ...outerArgs.map((argument) =>
    argument === "bin/epic-harness" ? "bin/missing" : argument,
  ));
  assert.notEqual(missingExecutable.status, 0, "an outer bundle requires its executable");
  assert.match(missingExecutable.stderr, /target executable/);

  const first = runBundleGenerator(fixture, "--write", ...outerArgs);
  assert.equal(first.status, 0, first.stderr);
  const firstOuter = JSON.parse(readFileSync(outer, "utf8"));
  assert.equal(firstOuter.manifest_kind, "post-link-runtime-bundle-v1");
  assert.equal(firstOuter.selector_protocol, "epic-harness-materialized-runtime-v1");
  assert.equal(firstOuter.target_triple, "x86_64-pc-windows-msvc");
  assert.equal(firstOuter.executable.path, "bin/epic-harness");
  assert.equal(firstOuter.executable.type, "file");
  assert.match(firstOuter.executable.sha256, /^sha256:[a-f0-9]{64}$/);
  const expectedOuterFiles = [
    ...independentlyReachableRuntimeFiles(fixture),
    "registry/scripts/bundle-manifest.json",
  ].sort();
  assert.deepEqual(
    firstOuter.materialized_files.map((file) => file.path),
    expectedOuterFiles,
  );
  for (const file of firstOuter.materialized_files) {
    assert.deepEqual(Object.keys(file).sort(), [
      "digest_mode",
      "mode",
      "path",
      "sha256",
      "size",
      "type",
    ]);
    assert.equal(file.type, "file");
    assert.equal(file.digest_mode, "raw-bytes-v1");
    assert.match(file.mode, /^0[0-7]{3}$/);
    assert.equal(typeof file.size, "number");
    assert.match(file.sha256, /^sha256:[a-f0-9]{64}$/);
  }
  assert.equal(runBundleGenerator(fixture, "--check", ...outerArgs).status, 0);

  writeFileSync(executable, Buffer.from("second executable bytes"));
  const stale = runBundleGenerator(fixture, "--check", ...outerArgs);
  assert.notEqual(stale.status, 0, "changing executable bytes must invalidate the outer bundle");
  assert.match(stale.stderr, /post-link bundle manifest drift/);
  assert.equal(runBundleGenerator(fixture, "--write", ...outerArgs).status, 0);
  const secondOuter = JSON.parse(readFileSync(outer, "utf8"));
  assert.notEqual(secondOuter.bundle_id, firstOuter.bundle_id);
});

test("runtime changes after a release require a new package version", (t) => {
  const version = JSON.parse(
    readFileSync(join(ROOT, "package.json"), "utf8"),
  ).version;
  const tag = `v${version}`;
  const tagExists = spawnSync("git", ["rev-parse", "--verify", tag], {
    cwd: ROOT,
    encoding: "utf8",
  });
  if (tagExists.status !== 0) {
    t.skip(`${tag} is not a release tag yet`);
    return;
  }

  const runtimePaths = [
    "Cargo.toml",
    "Cargo.lock",
    "build.rs",
    "src",
  ];
  const diff = spawnSync("git", ["diff", "--quiet", tag, "--", ...runtimePaths], {
    cwd: ROOT,
    encoding: "utf8",
  });
  assert.equal(
    diff.status,
    0,
    `${tag} exists, but runtime inputs changed without a version bump`,
  );
});

test("CI runs manifest and bootstrap contracts on Linux, macOS, and Windows", () => {
  const workflow = readFileSync(
    join(ROOT, ".github", "workflows", "ci.yml"),
    "utf8",
  );
  assert.match(workflow, /^\s*workflow_dispatch:\s*$/m);
  assert.match(workflow, /plugin-contract:/);
  assert.match(
    workflow,
    /os:\s*\[ubuntu-latest,\s*macos-latest,\s*windows-latest\]/,
  );
  assert.match(workflow, /fetch-depth:\s*0/);
  assert.match(
    workflow,
    /node --test registry\/scripts\/install\.test\.js registry\/scripts\/manifest\.test\.js/,
  );
});

test("Codex hook setup documents the supported Node LTS prerequisite", () => {
  const readme = readFileSync(join(ROOT, "README.md"), "utf8");
  const quickstart = readFileSync(join(ROOT, "docs", "quickstart.md"), "utf8");

  assert.match(
    readme,
    /Node\.js 22 LTS or a newer LTS release.*required.*Codex.*hook/i,
    "Codex hooks execute the Node bootstrap; document the supported Node LTS range as an actionable prerequisite",
  );
  assert.match(
    quickstart,
    /Node\.js 22 LTS or a newer LTS release/,
    "quickstart must retain the supported Node LTS range for Codex hook users",
  );
  assert.match(
    readme,
    /node --version/,
    "tell users how to verify the required Node runtime before enabling hooks",
  );
  assert.doesNotMatch(
    readme,
    /does not require Node\.js/i,
    "the hook lifecycle is not Node-free; qualify binary-only installation separately if needed",
  );
});

test("CI runs the frozen dashboard install, checks, tests, build, and asset comparison", () => {
  const workflow = readFileSync(
    join(ROOT, ".github", "workflows", "ci.yml"),
    "utf8",
  );

  assert.match(workflow, /working-directory:\s*app\s*\n\s*run:\s*pnpm install --frozen-lockfile/);
  assert.match(workflow, /working-directory:\s*app\s*\n\s*run:\s*pnpm run check/);
  assert.match(workflow, /working-directory:\s*app\s*\n\s*run:\s*pnpm exec vitest run/);
  assert.match(workflow, /working-directory:\s*app\s*\n\s*run:\s*pnpm run build/);
  assert.match(workflow, /cmp -s app\/dist\/index\.html assets\/dashboard\.html/);
});

test("Cargo builds the checked-in dashboard asset without frontend tools or source writes", () => {
  const buildScript = readFileSync(join(ROOT, "build.rs"), "utf8");

  assert.doesNotMatch(buildScript, /Command::new\("pnpm"\)/);
  assert.doesNotMatch(buildScript, /pnpm install|node_modules|dist\/index\.html/);
  assert.doesNotMatch(buildScript, /SKIP_DASHBOARD_BUILD/);
  assert.doesNotMatch(buildScript, /fs::copy/);
  assert.doesNotMatch(buildScript, /cargo:rerun-if-changed=app\//);
});

test("Orbit dismissal uses one exact project-scoped backend", () => {
  const command = readFileSync(
    join(ROOT, "src-tauri", "src", "commands", "harness.rs"),
    "utf8",
  );
  const tauri = readFileSync(
    join(ROOT, "src-tauri", "src", "lib.rs"),
    "utf8",
  );
  const frontend = readFileSync(
    join(ROOT, "app", "src", "lib", "harness.ts"),
    "utf8",
  );

  assert.match(command, /pub async fn dismiss_orbit\s*\(/);
  assert.match(command, /resolve_external_harness_dir\(&project\)/);
  assert.match(command, /dismiss_pipeline_state_pool\s*\(/);
  assert.match(tauri, /commands::harness::dismiss_orbit/);
  assert.match(frontend, /invoke<[^>]+>\('dismiss_orbit',\s*\{\s*project,\s*id\s*\}\)/);
  assert.match(frontend, /\/api\/orbit\/\$\{encodeURIComponent\(id\)\}\?project=\$\{encodeURIComponent\(project\)\}/);
  assert.match(frontend, /project === '__all__'/);
});

test("Orbit completion requires PR, CI, retry, and durable SessionEnd evidence", () => {
  const orbit = readFileSync(join(ROOT, "skills", "orbit", "SKILL.md"), "utf8");

  assert.match(orbit, /"ci_status": null/);
  assert.match(orbit, /`pr_url` is a nonempty concrete GitHub pull-request URL/);
  assert.match(orbit, /`ci_status` is exactly `"success"`/);
  assert.match(orbit, /`audit_fail_count` and `max_retries` are integer evidence/);
  assert.match(orbit, /`phase` is exactly `"evolve"` and `evolution_session_id`/);
  assert.match(orbit, /set `"phase": "awaiting_evolution"`/);
  assert.match(orbit, /only component that records `phase: "evolve"` and completion/);
  assert.match(orbit, /`epic orbit complete` rejects/);
  assert.match(orbit, /CI failure[\s\S]*"ci_status": "failed"[\s\S]*STOP/);
  assert.match(orbit, /Only successful CI proceeds to Step 7/);
  assert.doesNotMatch(orbit, /phase_history.*ship.*status: complete/);
  assert.doesNotMatch(orbit, /Always run.*regardless of CI outcome/);
  assert.doesNotMatch(orbit, /evolve must always run, even if CI fails/);
});

test("all README translations reject removed hook and session contracts", () => {
  const readmes = [
    join(ROOT, "README.md"),
    ...readdirSync(join(ROOT, "i18n"), { withFileTypes: true })
      .filter((entry) => entry.isDirectory())
      .map((entry) => join(ROOT, "i18n", entry.name, "README.md"))
      .filter(existsSync),
  ];
  const stalePatterns = [
    /session_\{date\}_\{pid\}_\{random\}\.jsonl/,
    /plugin_hooks/,
    /hooks\/bin\/epic-harness/,
  ];

  for (const path of readmes) {
    const content = readFileSync(path, "utf8");
    for (const pattern of stalePatterns) {
      assert.doesNotMatch(content, pattern, path);
    }
  }
});

test("runtime owners do not retain the removed bundled hook binary", () => {
  for (const path of ["Makefile", "Cargo.toml", "AGENTS.md", "src/update.rs"]) {
    assert.doesNotMatch(
      readFileSync(join(ROOT, path), "utf8"),
      /hooks\/bin\/epic-harness/,
      path,
    );
  }
});

test("npm package includes the hook runners and every plugin manifest target", () => {
  const result = spawnSync("npm", ["pack", "--dry-run", "--json"], {
    cwd: ROOT,
    encoding: "utf8",
    shell: process.platform === "win32",
  });
  assert.equal(result.status, 0, result.stderr);

  const packed = JSON.parse(result.stdout);
  const files = new Set(packed[0]?.files?.map((file) => file.path));
  for (const path of [
    ".claude-plugin/plugin.json",
    ".codex-plugin/plugin.json",
    "hooks/hooks.json",
    "registry/scripts/install.js",
    "registry/scripts/run-hook.cmd",
    "registry/scripts/bundle-manifest.json",
    "runtime-revision.txt",
  ]) {
    assert.ok(files.has(path), `${path} is missing from the npm artifact`);
  }

  for (const manifestPath of [
    ".claude-plugin/plugin.json",
    ".codex-plugin/plugin.json",
  ]) {
    const manifest = JSON.parse(readFileSync(join(ROOT, manifestPath), "utf8"));
    for (const target of [manifest.skills, manifest.mcpServers, manifest.hooks].filter(Boolean)) {
      const artifactPath = target.replace(/^\.\//, "").replace(/\/$/, "");
      assert.ok(
        files.has(artifactPath) ||
          [...files].some((path) => path.startsWith(`${artifactPath}/`)),
        `${manifestPath} target ${target} is missing from the npm artifact`,
      );
    }
  }
  for (const path of [
    "registry/scripts/install.test.js",
    "registry/scripts/manifest.test.js",
    "Cargo.toml",
    "Cargo.lock",
    "pnpm-lock.yaml",
  ]) {
    assert.ok(!files.has(path), `${path} is source-only and must not ship in the npm artifact`);
  }
  assert.ok(!files.has("plugin.json"), "removed Agy manifest must not ship");
});

test("dashboard source advertises only the supported plugin hosts", () => {
  const integrations = readFileSync(
    join(ROOT, "app", "src", "pages", "Integrations.svelte"),
    "utf8",
  );
  const status = readFileSync(
    join(ROOT, "src-tauri", "src", "commands", "harness.rs"),
    "utf8",
  );
  const httpStatus = readFileSync(join(ROOT, "src", "serve.rs"), "utf8");
  const httpStatusHandler = httpStatus
    .split('"get_integration_status" =>')[1]
    ?.split('"get_graph" =>')[0];

  assert.match(integrations, /name: 'Claude Code'/);
  assert.match(integrations, /name: 'Codex'/);
  assert.doesNotMatch(integrations, /Gemini|Cursor|Cline|Aider|Antigravity/);
  assert.doesNotMatch(status, /Antigravity|Cursor|Cline|Aider/);
  assert.ok(httpStatusHandler, "HTTP integration-status handler must exist");
  assert.doesNotMatch(httpStatusHandler, /Antigravity|Cursor|Cline|Aider/);
});
