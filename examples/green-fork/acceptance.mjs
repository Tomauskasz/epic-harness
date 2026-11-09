import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import net from "node:net";
import { spawn, spawnSync } from "node:child_process";

const binDir = path.resolve(process.argv[2] || "target/release");
const suffix = process.platform === "win32" ? ".exe" : "";
const root = fs.mkdtempSync(path.join(os.tmpdir(), "epic-artifact-acceptance-"));
const home = path.join(root, "home"), data = path.join(root, "data");
const main = path.join(root, "main project"), linked = path.join(root, "linked checkout");
for (const dir of [home, data, main, path.join(main, "nested directory"), path.join(home, ".config", "epic-harness")]) fs.mkdirSync(dir, { recursive: true });
fs.writeFileSync(path.join(home, ".config", "epic-harness", "telemetry-consent"), "off");
fs.writeFileSync(path.join(data, "config.toml"), "[dashboard]\nport=0\nauto_open=false\n");
const env = { ...process.env, HOME: home, USERPROFILE: home, HARNESS_DIR: data, HARNESS_ROOT: root, XDG_CONFIG_HOME: path.join(home, ".config"), DO_NOT_TRACK: "1" };
for (const key of ["HARNESS_MEM_URL", "HARNESS_DB_URL", "DATABASE_URL", "EPIC_AGENT_ID", "CLAUDE_MODEL", "ANTHROPIC_MODEL"]) delete env[key];
function run(command, args, cwd = main, input = "") {
  const result = spawnSync(command, args, { cwd, env, input, encoding: "utf8", windowsHide: true, timeout: 20000 });
  assert.ifError(result.error);
  return result;
}
function git(args) { const result = run("git", args); assert.equal(result.status, 0, result.stderr); }
git(["init", "-b", "main"]);
git(["-c", "user.name=Synthetic QA", "-c", "user.email=synthetic@example.invalid", "commit", "--allow-empty", "-m", "test: seed artifact fixture"]);
git(["worktree", "add", "--detach", linked]);
for (const name of ["epic", "epic-harness"]) {
  const binary = path.join(binDir, name + suffix);
  assert.ok(fs.existsSync(binary), "missing source-built executable: " + name);
  const identity = run(binary, ["path"]);
  assert.equal(identity.status, 0, identity.stderr);
  for (const cwd of [path.join(main, "nested directory"), linked]) assert.equal(run(binary, ["path"], cwd).stdout.trim(), identity.stdout.trim());
  const resume = run(binary, ["resume"], main, JSON.stringify({ hook_event_name: "SessionStart", session_id: "artifact-" + name, source: "startup" }));
  assert.equal(resume.status, 0, resume.stderr);
  assert.ok(JSON.parse(resume.stdout));
  const reflect = run(binary, ["reflect", "--context", "--project", "main_project"]);
  assert.equal(reflect.status, 0, reflect.stderr);
  assert.ok(JSON.parse(reflect.stdout));
  const escape = run(binary, ["reflect", "--context", "--project", "../outside"]);
  assert.notEqual(escape.status, 0, "reflection must reject traversal");
  const add = run(binary, ["mem", "add", "--title", "Artifact policy " + name, "--type", "decision", "--project", "main_project", "--body", "SOURCE_ARTIFACT_MEMORY_SENTINEL_" + name]);
  assert.equal(add.status, 0, add.stderr);
  const recall = run(binary, ["mem", "recall", "Artifact policy " + name, "--project", "main_project"]);
  assert.equal(recall.status, 0, recall.stderr);
  assert.ok(recall.stdout.includes("SOURCE_ARTIFACT_MEMORY_SENTINEL_" + name));
  const requests = [
    { jsonrpc: "2.0", id: 1, method: "initialize", params: { protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "source-artifact-qa", version: "1" } } },
    { jsonrpc: "2.0", method: "notifications/initialized" },
    { jsonrpc: "2.0", id: 2, method: "tools/list" },
    { jsonrpc: "2.0", id: 3, method: "ping" },
  ];
  const rpc = run(binary, ["mem", "mcp"], main, requests.map(JSON.stringify).join("\n") + "\n");
  assert.equal(rpc.status, 0, rpc.stderr);
  const replies = rpc.stdout.trim().split(/\r?\n/).filter(Boolean).map(line => JSON.parse(line));
  assert.ok(replies.find(r => r.id === 1)?.result);
  assert.ok(replies.find(r => r.id === 2)?.result?.tools?.length >= 6);
  assert.deepEqual(replies.find(r => r.id === 3)?.result, {});
  assert.equal(replies.length, 3, "notifications must not receive responses");
  const serverSocket = net.createServer();
  const port = await new Promise((resolve, reject) => { serverSocket.once("error", reject); serverSocket.listen(0, "127.0.0.1", () => { const value = serverSocket.address().port; serverSocket.close(() => resolve(value)); }); });
  const child = spawn(binary, ["serve", "--port=" + port], { cwd: main, env, windowsHide: true, stdio: ["ignore", "pipe", "pipe"] });
  let stderr = ""; child.stderr.on("data", bytes => { stderr += bytes; });
  try {
    let response;
    for (let attempt = 0; attempt < 80; attempt++) {
      try { response = await fetch("http://127.0.0.1:" + port + "/api/projects", { signal: AbortSignal.timeout(1000) }); if (response.ok) break; } catch {}
      await new Promise(resolve => setTimeout(resolve, 50));
    }
    assert.ok(response?.ok, "owned dashboard failed to start: " + stderr);
    assert.ok(Array.isArray(await response.json()));
    const html = await fetch("http://127.0.0.1:" + port + "/").then(r => r.text());
    assert.ok(html.includes("harness-version"), "dashboard version provenance missing");
    const activity = await fetch("http://127.0.0.1:" + port + "/api/harness?cmd=get_live_activity&project=main_project").then(r => r.json());
    assert.ok(Array.isArray(activity.sessions) && Array.isArray(activity.runs));
  } finally {
    if (child.exitCode === null) {
      const exited = new Promise(resolve => child.once("exit", resolve)); child.kill(); await exited;
    }
  }
  console.log("PASS source artifact: " + name + " identity/reflection/hooks/memory/MCP/dashboard");
}
