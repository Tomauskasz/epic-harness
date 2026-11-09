import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";

const [command, ...args] = process.argv.slice(2);
if (!command) throw new Error("usage: node run-isolated.mjs <command> [args...]");
const root = fs.mkdtempSync(path.join(os.tmpdir(), "epic-source-check-"));
const home = path.join(root, "home");
const data = path.join(root, "data");
fs.mkdirSync(path.join(home, ".config", "epic-harness"), { recursive: true });
fs.mkdirSync(data);
fs.writeFileSync(path.join(home, ".config", "epic-harness", "telemetry-consent"), "off\n");
fs.writeFileSync(path.join(data, "config.toml"), "[dashboard]\nport = 0\nauto_open = false\n");
const originalHome = process.env.HOME || process.env.USERPROFILE || os.homedir();
const env = {
  ...process.env,
  HOME: home,
  USERPROFILE: home,
  XDG_CONFIG_HOME: path.join(home, ".config"),
  APPDATA: path.join(home, "AppData", "Roaming"),
  LOCALAPPDATA: path.join(home, "AppData", "Local"),
  HARNESS_DIR: data,
  HARNESS_ROOT: root,
  CARGO_HOME: process.env.CARGO_HOME || path.join(originalHome, ".cargo"),
  RUSTUP_HOME: process.env.RUSTUP_HOME || path.join(originalHome, ".rustup"),
  DO_NOT_TRACK: "1",
};
for (const key of ["HARNESS_MEM_URL", "HARNESS_DB_URL", "DATABASE_URL", "EPIC_AGENT_ID", "EPIC_ORG", "CLAUDE_MODEL", "ANTHROPIC_MODEL"]) delete env[key];
const result = spawnSync(command, args, { env, stdio: "inherit", windowsHide: true });
if (result.error) throw result.error;
process.exitCode = result.status ?? 1;
