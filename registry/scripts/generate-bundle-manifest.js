#!/usr/bin/env node
// Generates or checks the logical runtime manifest.  The inventory lives in
// bundle-spec.json so Cargo, this generator, and the executable projection use
// the same closure.  `--outer` is intentionally a separate post-link manifest:
// it binds the real target executable without making the checked source
// manifest hash itself.

"use strict";

import { createHash } from "node:crypto";
import {
  existsSync,
  lstatSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  writeFileSync,
} from "node:fs";
import { dirname, join, relative, resolve } from "node:path";

const BUNDLE_SPEC_PATH = "registry/scripts/bundle-spec.json";
const OUTER_BUNDLE_ID_ALGORITHM =
  "sha256-canonical-post-link-runtime-bundle-v1";
const OUTER_BUNDLE_ID_DOMAIN = Buffer.from(
  "epic-harness-post-link-runtime-bundle\0v1\0",
  "utf8",
);

function fail(message) {
  throw new Error(message);
}

function exactKeys(value, expected, label) {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    fail(`${label} must be a JSON object`);
  }
  const actualKeys = Object.keys(value).sort();
  const expectedKeys = [...expected].sort();
  if (JSON.stringify(actualKeys) !== JSON.stringify(expectedKeys)) {
    fail(`${label} has unsupported or missing fields`);
  }
}

function assertPortablePath(path, label) {
  if (typeof path !== "string" || path.length === 0) {
    fail(`${label} must be a nonempty portable relative path`);
  }
  if (path.startsWith("/") || path.includes("\\")) {
    fail(`${label} must not be absolute or use a backslash path separator: ${path}`);
  }
  const components = path.split("/");
  if (
    components.some(
      (component) =>
        component.length === 0 ||
        component === "." ||
        component === ".." ||
        !/^[A-Za-z0-9._-]+$/.test(component),
    )
  ) {
    fail(`${label} has traversal or unsupported path components: ${path}`);
  }
  return path;
}

function pathFromRoot(root, path, label) {
  assertPortablePath(path, label);
  return join(root, ...path.split("/"));
}

function normalizedPath(root, path) {
  return relative(root, path).replaceAll("\\", "/");
}

function readJson(path, label) {
  try {
    return JSON.parse(readFileSync(path, "utf8"));
  } catch (error) {
    fail(`cannot read ${label} ${path}: ${error.message}`);
  }
}

function parseBundleSpec(root) {
  const path = pathFromRoot(root, BUNDLE_SPEC_PATH, "bundle spec path");
  const spec = readJson(path, "bundle spec");
  exactKeys(spec, ["schema_version", "logical", "outer"], "bundle spec");
  if (spec.schema_version !== 1) {
    fail(`bundle spec schema_version is ${spec.schema_version}, expected 1`);
  }

  exactKeys(
    spec.logical,
    ["schema_version", "manifest_kind", "manifest_path", "identity", "artifacts"],
    "bundle spec logical",
  );
  if (spec.logical.schema_version !== 1) {
    fail(`logical manifest schema_version is ${spec.logical.schema_version}, expected 1`);
  }
  if (spec.logical.manifest_kind !== "logical-runtime-v1") {
    fail("logical manifest_kind must be logical-runtime-v1");
  }
  assertPortablePath(spec.logical.manifest_path, "logical manifest_path");
  exactKeys(spec.logical.identity, ["algorithm", "domain"], "bundle spec logical identity");
  if (
    spec.logical.identity.algorithm !==
    "sha256-framed-logical-source-and-artifact-projection-v3"
  ) {
    fail("logical identity algorithm is unsupported");
  }
  if (typeof spec.logical.identity.domain !== "string") {
    fail("logical identity domain must be a string");
  }
  if (!Array.isArray(spec.logical.artifacts) || spec.logical.artifacts.length === 0) {
    fail("bundle spec logical artifacts must be a nonempty array");
  }
  for (const [index, artifact] of spec.logical.artifacts.entries()) {
    exactKeys(artifact, ["path", "selector", "digest_mode"], `logical artifact ${index}`);
    assertPortablePath(artifact.path, `logical artifact ${index} path`);
    if (!["file-v1", "recursive-files-v1"].includes(artifact.selector)) {
      fail(`logical artifact ${artifact.path} has unsupported selector ${artifact.selector}`);
    }
    if (
      ![
        "canonical-plugin-json-v1",
        "canonical-json-v1",
        "normalized-lf-text-v1",
      ].includes(artifact.digest_mode)
    ) {
      fail(`logical artifact ${artifact.path} has unsupported digest mode ${artifact.digest_mode}`);
    }
    if (artifact.path === spec.logical.manifest_path) {
      fail("the logical manifest cannot be its own logical artifact");
    }
  }

  exactKeys(
    spec.outer,
    [
      "schema_version",
      "manifest_kind",
      "selector_protocol",
      "target_executable",
    ],
    "bundle spec outer",
  );
  if (spec.outer.schema_version !== 1) {
    fail(`outer manifest schema_version is ${spec.outer.schema_version}, expected 1`);
  }
  if (spec.outer.manifest_kind !== "post-link-runtime-bundle-v1") {
    fail("outer manifest_kind must be post-link-runtime-bundle-v1");
  }
  if (spec.outer.selector_protocol !== "epic-harness-materialized-runtime-v1") {
    fail("outer selector_protocol is unsupported");
  }
  exactKeys(
    spec.outer.target_executable,
    ["selector", "type", "digest_mode"],
    "bundle spec outer target_executable",
  );
  if (
    spec.outer.target_executable.selector !== "target-executable-v1" ||
    spec.outer.target_executable.type !== "file" ||
    spec.outer.target_executable.digest_mode !== "raw-bytes-v1"
  ) {
    fail("outer target_executable must select one raw file");
  }
  return spec;
}

function assertRegularFile(path, label) {
  let metadata;
  try {
    metadata = lstatSync(path);
  } catch (error) {
    fail(`cannot inspect ${label} ${path}: ${error.message}`);
  }
  if (metadata.isSymbolicLink() || !metadata.isFile()) {
    fail(`${label} must be a regular non-symlink file: ${path}`);
  }
  return metadata;
}

function assertDirectory(path, label) {
  let metadata;
  try {
    metadata = lstatSync(path);
  } catch (error) {
    fail(`cannot inspect ${label} ${path}: ${error.message}`);
  }
  if (metadata.isSymbolicLink() || !metadata.isDirectory()) {
    fail(`${label} must be a real non-symlink directory: ${path}`);
  }
}

function recursiveFiles(root, directory, label) {
  const files = [];
  const visit = (current) => {
    assertDirectory(current, label);
    const entries = readdirSync(current, { withFileTypes: true }).sort((left, right) =>
      left.name.localeCompare(right.name),
    );
    for (const entry of entries) {
      const path = join(current, entry.name);
      if (entry.isSymbolicLink()) {
        fail(`${label} contains a symbolic-link-style entry: ${normalizedPath(root, path)}`);
      }
      if (entry.isDirectory()) {
        visit(path);
      } else if (entry.isFile()) {
        assertRegularFile(path, label);
        files.push(normalizedPath(root, path));
      } else {
        fail(`${label} contains an unsupported entry: ${normalizedPath(root, path)}`);
      }
    }
  };
  visit(directory);
  return files;
}

function expandLogicalArtifacts(root, spec) {
  const artifacts = [];
  const seen = new Set();
  for (const descriptor of spec.logical.artifacts) {
    const absolute = pathFromRoot(root, descriptor.path, "logical artifact path");
    const paths =
      descriptor.selector === "file-v1"
        ? (assertRegularFile(absolute, `logical artifact ${descriptor.path}`), [descriptor.path])
        : recursiveFiles(root, absolute, `logical artifact ${descriptor.path}`);
    if (paths.length === 0) {
      fail(`logical artifact ${descriptor.path} selected no files`);
    }
    for (const path of paths) {
      if (seen.has(path)) {
        fail(`duplicate logical artifact path: ${path}`);
      }
      seen.add(path);
      artifacts.push({
        path,
        type: "file",
        digest_mode: descriptor.digest_mode,
      });
    }
  }
  return artifacts.sort((left, right) => left.path.localeCompare(right.path));
}

function rustSourcePaths(root) {
  const paths = recursiveFiles(root, join(root, "src"), "Rust source tree")
    .filter((path) => path.endsWith(".rs"));
  return paths.sort();
}

function sourceIdentityPaths(root) {
  return [
    "Cargo.lock",
    "Cargo.toml",
    "build.rs",
    BUNDLE_SPEC_PATH,
    ...rustSourcePaths(root),
  ].sort();
}

function sha256(bytes) {
  return `sha256:${createHash("sha256").update(bytes).digest("hex")}`;
}

function frame(hash, bytes) {
  const length = Buffer.alloc(8);
  length.writeBigUInt64BE(BigInt(bytes.length));
  hash.update(length);
  hash.update(bytes);
}

function normalizeSourceIdentity(bytes) {
  const normalized = [];
  for (let index = 0; index < bytes.length; index += 1) {
    if (bytes[index] === 0x0d) {
      normalized.push(0x0a);
      if (bytes[index + 1] === 0x0a) index += 1;
    } else {
      normalized.push(bytes[index]);
    }
  }
  return Buffer.from(normalized);
}

function canonicalJson(value) {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) {
    return `[${value.map((entry) => canonicalJson(entry)).join(",")}]`;
  }
  return `{${Object.keys(value)
    .sort()
    .map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`)
    .join(",")}}`;
}

function canonicalPluginJson(bytes, path) {
  const plugin = JSON.parse(bytes.toString("utf8"));
  if (plugin === null || typeof plugin !== "object" || Array.isArray(plugin)) {
    fail(`${path} must contain a JSON object`);
  }
  if (typeof plugin.version !== "string") {
    fail(`${path} must declare a string version`);
  }
  const version = /^(\d+\.\d+\.\d+)(?:\+codex\.[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/.exec(
    plugin.version,
  );
  if (!version) {
    fail(`${path} has an invalid plugin version: ${plugin.version}`);
  }
  return canonicalJson({ ...plugin, version: version[1] });
}

function artifactDigest(root, artifact) {
  const bytes = readFileSync(pathFromRoot(root, artifact.path, "artifact path"));
  switch (artifact.digest_mode) {
    case "canonical-plugin-json-v1":
      return sha256(Buffer.from(canonicalPluginJson(bytes, artifact.path), "utf8"));
    case "canonical-json-v1":
      return sha256(Buffer.from(canonicalJson(JSON.parse(bytes.toString("utf8"))), "utf8"));
    case "normalized-lf-text-v1":
      return sha256(Buffer.from(bytes.toString("utf8").replace(/\r\n?/g, "\n"), "utf8"));
    default:
      fail(`unsupported artifact digest mode: ${artifact.digest_mode}`);
  }
}

function buildIdentity(root, spec, sourcePaths, artifacts) {
  const hash = createHash("sha256");
  hash.update(Buffer.from(spec.logical.identity.domain, "utf8"));
  for (const path of sourcePaths) {
    frame(hash, Buffer.from("source", "utf8"));
    frame(hash, Buffer.from(path, "utf8"));
    frame(
      hash,
      normalizeSourceIdentity(
        readFileSync(pathFromRoot(root, path, "source identity path")),
      ),
    );
  }
  for (const artifact of artifacts) {
    frame(hash, Buffer.from("artifact", "utf8"));
    frame(hash, Buffer.from(artifact.path, "utf8"));
    frame(hash, Buffer.from(artifact.type, "utf8"));
    frame(hash, Buffer.from(artifact.digest_mode, "utf8"));
    frame(hash, Buffer.from(artifact.sha256, "utf8"));
  }
  return `sha256:${hash.digest("hex")}`;
}

function releaseVersion(root) {
  const packageJson = readJson(join(root, "package.json"), "package.json");
  if (typeof packageJson.version !== "string" || !/^\d+\.\d+\.\d+$/.test(packageJson.version)) {
    fail("package.json must declare a semantic release version");
  }
  return packageJson.version;
}

function runtimeRevision(root) {
  const revision = readFileSync(join(root, "runtime-revision.txt"), "utf8").trim();
  if (!/^[1-9]\d*$/.test(revision)) {
    fail("runtime-revision.txt must contain one positive integer");
  }
  return revision;
}

function expectedLogicalManifest(root, spec = parseBundleSpec(root)) {
  const sourcePaths = sourceIdentityPaths(root);
  const artifacts = expandLogicalArtifacts(root, spec).map((artifact) => ({
    ...artifact,
    sha256: artifactDigest(root, artifact),
  }));
  return {
    schema_version: spec.logical.schema_version,
    manifest_kind: spec.logical.manifest_kind,
    release_version: releaseVersion(root),
    runtime_revision: runtimeRevision(root),
    build_identity: buildIdentity(root, spec, sourcePaths, artifacts),
    artifacts,
    identity_inputs: {
      algorithm: spec.logical.identity.algorithm,
      inventory_path: BUNDLE_SPEC_PATH,
      source_paths: sourcePaths,
      artifact_paths: artifacts.map((artifact) => artifact.path),
    },
  };
}

function packageDeclaresPath(packageJson, path) {
  return packageJson.files.some((entry) => {
    if (typeof entry !== "string") return false;
    const declared = entry.replace(/\/$/, "");
    return path === declared || path.startsWith(`${declared}/`);
  });
}

function assertPackageClosure(root, spec) {
  const packageJson = readJson(join(root, "package.json"), "package.json");
  if (!Array.isArray(packageJson.files)) {
    fail("package.json files must be an array");
  }
  const paths = [
    ...expandLogicalArtifacts(root, spec).map((artifact) => artifact.path),
    spec.logical.manifest_path,
  ];
  for (const path of paths) {
    if (path === "package.json") continue;
    if (!packageDeclaresPath(packageJson, path)) {
      fail(`package.json files does not ship required runtime path: ${path}`);
    }
  }
}

function logicalValidationErrors(actual, expected) {
  if (actual === null || typeof actual !== "object" || Array.isArray(actual)) {
    return ["logical bundle manifest must be a JSON object"];
  }
  const errors = [];
  const expectedKeys = Object.keys(expected).sort();
  if (JSON.stringify(Object.keys(actual).sort()) !== JSON.stringify(expectedKeys)) {
    errors.push("logical bundle manifest schema fields differ");
  }
  for (const key of [
    "schema_version",
    "manifest_kind",
    "release_version",
    "runtime_revision",
    "build_identity",
  ]) {
    if (actual[key] !== expected[key]) errors.push(`${key} mismatch`);
  }
  if (!Array.isArray(actual.artifacts)) {
    errors.push("artifacts must be an array");
  } else {
    const actualPaths = actual.artifacts.map((artifact) => artifact?.path);
    const expectedPaths = expected.artifacts.map((artifact) => artifact.path);
    if (JSON.stringify(actualPaths) !== JSON.stringify(expectedPaths)) {
      errors.push("artifact path projection mismatch");
    }
    for (const artifact of expected.artifacts) {
      const candidate = actual.artifacts.find((entry) => entry?.path === artifact.path);
      if (!candidate) continue;
      if (
        JSON.stringify(Object.keys(candidate).sort()) !==
        JSON.stringify(["digest_mode", "path", "sha256", "type"])
      ) {
        errors.push(`artifact schema mismatch: ${artifact.path}`);
      }
      for (const key of ["type", "digest_mode", "sha256"]) {
        if (candidate[key] !== artifact[key]) {
          errors.push(`artifact ${key} mismatch: ${artifact.path}`);
        }
      }
    }
  }
  if (
    actual.identity_inputs === null ||
    typeof actual.identity_inputs !== "object" ||
    Array.isArray(actual.identity_inputs)
  ) {
    errors.push("identity_inputs must be an object");
  } else if (
    canonicalJson(actual.identity_inputs) !== canonicalJson(expected.identity_inputs)
  ) {
    errors.push("identity input projection mismatch");
  }
  return errors;
}

function readLogicalManifest(root, spec) {
  const path = pathFromRoot(root, spec.logical.manifest_path, "logical manifest path");
  return readJson(path, "logical bundle manifest");
}

function assertCurrentLogicalManifest(root, spec, expected) {
  const errors = logicalValidationErrors(readLogicalManifest(root, spec), expected);
  if (errors.length > 0) {
    fail(
      `logical bundle manifest drift: ${errors.join("; ")}\n` +
        "run: node registry/scripts/generate-bundle-manifest.js --write",
    );
  }
}

function modeString(mode) {
  return `0${(mode & 0o777).toString(8).padStart(3, "0")}`;
}

function materializedFile(root, path, label) {
  const absolute = pathFromRoot(root, path, label);
  const metadata = assertRegularFile(absolute, label);
  return {
    path,
    type: "file",
    mode: modeString(metadata.mode),
    size: metadata.size,
    digest_mode: "raw-bytes-v1",
    sha256: sha256(readFileSync(absolute)),
  };
}

function postLinkBundleId(manifest) {
  const withoutId = { ...manifest };
  delete withoutId.bundle_id;
  const hash = createHash("sha256");
  hash.update(OUTER_BUNDLE_ID_DOMAIN);
  hash.update(Buffer.from(canonicalJson(withoutId), "utf8"));
  return `sha256:${hash.digest("hex")}`;
}

function expectedOuterManifest(root, options, spec) {
  const logical = expectedLogicalManifest(root, spec);
  assertCurrentLogicalManifest(root, spec, logical);
  const materializedRoot = options.materializedRoot;
  assertDirectory(materializedRoot, "materialized runtime root");
  const executable = materializedFile(
    materializedRoot,
    options.executable,
    "target executable",
  );
  const materializedPaths = [
    ...logical.artifacts.map((artifact) => artifact.path),
    spec.logical.manifest_path,
  ].sort();
  if (new Set(materializedPaths).size !== materializedPaths.length) {
    fail("logical materialized file projection contains a duplicate path");
  }
  const materialized_files = materializedPaths.map((path) =>
    materializedFile(materializedRoot, path, "materialized runtime artifact"),
  );
  const manifest = {
    schema_version: spec.outer.schema_version,
    manifest_kind: spec.outer.manifest_kind,
    bundle_id_algorithm: OUTER_BUNDLE_ID_ALGORITHM,
    selector_protocol: spec.outer.selector_protocol,
    logical_build_identity: logical.build_identity,
    target_triple: options.target,
    executable: {
      selector: spec.outer.target_executable.selector,
      ...executable,
    },
    materialized_files,
  };
  return { ...manifest, bundle_id: postLinkBundleId(manifest) };
}

function outerValidationErrors(actual, expected) {
  if (actual === null || typeof actual !== "object" || Array.isArray(actual)) {
    return ["post-link bundle manifest must be a JSON object"];
  }
  if (canonicalJson(actual) !== canonicalJson(expected)) {
    return ["post-link bundle manifest projection mismatch"];
  }
  return [];
}

function parseArguments(argv) {
  const options = {
    mode: "write",
    root: resolve("."),
    outer: false,
    executable: undefined,
    target: undefined,
    materializedRoot: undefined,
    output: undefined,
  };
  let modeSpecified = false;
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === "--check" || argument === "--write") {
      if (modeSpecified) fail("use only one of --check or --write");
      options.mode = argument.slice(2);
      modeSpecified = true;
      continue;
    }
    if (argument === "--outer") {
      options.outer = true;
      continue;
    }
    if (argument === "--help") {
      return { help: true };
    }
    const next = argv[index + 1];
    if (["--root", "--executable", "--target", "--materialized-root", "--output"].includes(argument)) {
      if (!next) fail(`${argument} requires a value`);
      switch (argument) {
        case "--root":
          options.root = resolve(next);
          break;
        case "--executable":
          options.executable = next;
          break;
        case "--target":
          options.target = next;
          break;
        case "--materialized-root":
          options.materializedRoot = resolve(next);
          break;
        case "--output":
          options.output = resolve(next);
          break;
        default:
          fail(`unknown argument: ${argument}`);
      }
      index += 1;
      continue;
    }
    fail(`unknown argument: ${argument}`);
  }
  if (!options.outer && (options.executable || options.target || options.materializedRoot || options.output)) {
    fail("post-link options require --outer");
  }
  if (options.outer) {
    if (!options.executable || !options.target || !options.output) {
      fail("--outer requires --executable, --target, and --output");
    }
    assertPortablePath(options.executable, "target executable path");
    if (!/^[A-Za-z0-9._-]+$/.test(options.target)) {
      fail("target triple has unsupported characters");
    }
    options.materializedRoot ??= options.root;
  }
  return options;
}

function usage() {
  return [
    "Usage:",
    "  node registry/scripts/generate-bundle-manifest.js [--write|--check] [--root <source-root>]",
    "  node registry/scripts/generate-bundle-manifest.js --outer [--write|--check] --root <source-root> --materialized-root <bundle-root> --executable <relative-path> --target <target-triple> --output <outer-manifest>",
    "",
    "The logical manifest is checked source metadata. The post-link outer manifest",
    "binds raw materialized files and executable bytes and is deliberately excluded",
    "from its own file list to avoid a recursive self-hash.",
  ].join("\n");
}

function main() {
  const options = parseArguments(process.argv.slice(2));
  if (options.help) {
    process.stdout.write(`${usage()}\n`);
    return;
  }
  if (!existsSync(options.root)) fail(`bundle root does not exist: ${options.root}`);
  const spec = parseBundleSpec(options.root);
  assertPackageClosure(options.root, spec);
  if (!options.outer) {
    const path = pathFromRoot(options.root, spec.logical.manifest_path, "logical manifest path");
    const manifest = expectedLogicalManifest(options.root, spec);
    if (options.mode === "write") {
      mkdirSync(dirname(path), { recursive: true });
      writeFileSync(path, `${JSON.stringify(manifest, null, 2)}\n`, "utf8");
      process.stdout.write(`wrote ${normalizedPath(options.root, path)}\n`);
      return;
    }
    assertCurrentLogicalManifest(options.root, spec, manifest);
    return;
  }

  const manifest = expectedOuterManifest(options.root, options, spec);
  if (options.mode === "write") {
    mkdirSync(dirname(options.output), { recursive: true });
    writeFileSync(options.output, `${JSON.stringify(manifest, null, 2)}\n`, "utf8");
    process.stdout.write(`wrote ${options.output}\n`);
    return;
  }
  const errors = outerValidationErrors(readJson(options.output, "post-link bundle manifest"), manifest);
  if (errors.length > 0) {
    fail(
      `post-link bundle manifest drift: ${errors.join("; ")}\n` +
        "run: node registry/scripts/generate-bundle-manifest.js --outer --write ...",
    );
  }
}

try {
  main();
} catch (error) {
  process.stderr.write(`${error.message}\n`);
  process.exitCode = 1;
}
