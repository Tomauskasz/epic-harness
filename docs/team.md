# `epic team` — Org-Level Agent Teams

> Implementation: `src/team/`
> Spec: `docs/research/team-spec.md`

---

## Overview

`epic team` manages **org-level agent teams** — persistent team definitions that accumulate
knowledge across projects and are never silently overwritten.

Core model:
```
Org  ──owns──▶  Team  ──has──▶  Agent(s)  (synced to Claude; additionally to Codex when installed)
                  │
                  ├──has──▶  Playbook     (global, append-only)
                  └──has──▶  Mission      (one-line purpose)
```

Teams live in `~/.harness/orgs/{org}/teams/{team}/` — independent of any project.
Each sync writes Claude Markdown agents to `.claude/agents/{team}/` in the current
project (or `~/.claude/agents/{team}/` with `--global`), where Claude Code auto-discovers
them. It also writes native, flat Codex TOML agents to `~/.codex/agents/` only if
`~/.codex` already exists; `--global` does not change that Codex destination.

---

## Storage Layout

```
~/.harness/orgs/
└── {org}/                            # default: "epic"  (--org flag to override)
    └── teams/
        └── {team}/
            ├── config.json           # name, org, type, projects[], created, updated
            ├── mission.md            # one-line domain ownership statement
            ├── playbook.md           # accumulated knowledge (append-only, never truncated)
            ├── agents/
            │   ├── domain-expert.md  # canonical role definition
            │   └── reviewer.md
            └── .history/             # backups before each agent replacement
                └── domain-expert-2026-04-16.md
```

### config.json

```json
{
  "name": "backend",
  "org": "epic",
  "type": "stream",
  "projects": ["epic-harness", "my-api"],
  "created": "2026-04-16T00:00:00Z",
  "updated": "2026-04-16T00:00:00Z"
}
```

`projects[]` is for traceability — stale entries cause no harm.

---

## Usage

### Interactive design (primary flow)

```
epic team
```

Launches a 4-phase interactive flow:

```
Phase 1 — Resolve context
  - Org from --org flag | default "epic"
  - Scan project: detect stack (Rust/Node/Python/Go/Java), read README excerpt
  - List existing teams in org

Phase 2 — Design
  - Prompt: team name (default: sanitized project name)
  - Prompt: team type (stream/platform/enabling/subsystem)
  - Prompt: mission (one-line domain ownership)
  - Show proposed agent composition from type template

Phase 3 — Write / merge
  - New team: create all files
  - Existing team: merge per strategy (no silent overwrites)

Phase 4 — Sync
  - Copy agents to ./.claude/agents/{team}/
  - Inject ## Team Context into each copy
  - Also write native Codex TOML agents to ~/.codex/agents/ when ~/.codex exists
```

### Subcommands

```bash
epic team list                       # list teams in current org
epic team list --org netflix         # list teams in named org
epic team show backend               # config + agents + mission
epic team show backend --playbook    # also print full playbook
epic team sync backend               # sync Claude project agents; also Codex agents when ~/.codex exists
epic team sync backend --global     # sync Claude agents to ~/.claude/agents/; Codex location is unchanged
epic team link backend               # attach existing team (sync + add to config.projects)
epic team unlink backend             # remove synced Claude and owned Codex agents (keeps global store)
epic team delete backend             # remove current project's Claude and owned Codex agents
epic team delete backend --global    # permanently delete from org store + synced agents
epic team history backend reviewer   # list .history/ backups for an agent
```

### Flags

| Flag | Description |
|---|---|
| `--org <name>` | Target a specific org (default: `"epic"`) |
| `--playbook` | `show` only: print full accumulated playbook |
| `--global` | `sync` only: install Claude agents to `~/.claude/agents/` instead of project-local `.claude/agents/`; Codex stays at `~/.codex/agents/` |

---

## Team Types

Type drives default agent composition proposals. User can override at design time.

| Type | Keyword | Default agents |
|---|---|---|
| Stream-aligned | `stream` | `domain-expert`, `reviewer`, `tester` |
| Platform | `platform` | `api-designer`, `infra-specialist`, `dx-agent` |
| Enabling | `enabling` | `specialist` |
| Complicated Subsystem | `subsystem` | `domain-specialist`, `integration-tester` |

---

## Merge Strategy

Re-running `epic team` on an existing team never silently overwrites.

| Object | Action |
|---|---|
| Agent — new name | **Add** automatically |
| Agent — content unchanged | **Skip** (no-op) |
| Agent — content changed | **Prompt** (default: keep existing). Replaces → backs up to `.history/` |
| `playbook.md` | **Always append** `---` separator + new section. Never truncated. |
| `mission.md` — unchanged | **Skip** |
| `mission.md` — changed | **Prompt** (default: keep existing) |
| `config.json → projects[]` | Append project name if absent. Never removes. |

All prompts default to **skip** (safe). Destructive ops require explicit `y`.

---

## Project Integration

At sync time, agents are **copied** to `.claude/agents/{team}/` with a `## Team Context`
section injected. This gives each Claude Code agent orientation without loading the full
playbook into the context window. If `~/.codex` already exists, the same sync additionally
renders each agent as a flat TOML file in `~/.codex/agents/` with `name`, `description`,
and `developer_instructions`; Claude-only `model`, `tools`, and `skills` frontmatter is
dropped.

```markdown
## Team Context
**Team**: backend (Stream-aligned)
**Mission**: Own the API layer end-to-end across all backend services
**Full playbook**: `epic team show backend --playbook`
```

The global store holds canonical definitions (no Team Context).
The project copy holds canonical definition + injected context.

Re-running `epic team sync backend` refreshes the Claude project copy and, when Codex is
installed, its native agent files (e.g. after a mission update).

`.claude/agents/` is **not** gitignored by default — teams may want to version-control
their project-local copies. Add to `.gitignore` explicitly if undesired.

---

## Multi-Org Example

```bash
# Default org accumulates across all personal/work projects
epic team                          # creates in "epic" org

# Model a Netflix-style topology in a separate org
epic team --org netflix            # creates in "netflix" org

# List orgs
ls ~/.harness/orgs/
# epic/  netflix/  startup-x/
```

Same team name in same org = intentional cross-project sharing. `epic/teams/backend`
accumulates knowledge from every project that creates or links it.

---

## Agent Integration

Each supported tool's `/team` command delegates to `epic team` (Claude Code or Codex). No team logic lives in the plugin layer — the CLI is the source of truth.

---

## Implementation

```
src/team/
├── mod.rs      entry point — pub fn run(args) -> i32
├── store.rs    storage layer — TeamConfig, path helpers, CRUD, content builders
├── cli.rs      dispatch + 7 subcommands, interactive flow, scan_project, sync_to_project
└── codex.rs    native Codex TOML rendering and safe file writes
```

### Key types (`store.rs`)

```rust
pub struct TeamConfig {
    pub name: String,
    pub org: String,
    pub team_type: String,     // "stream" | "platform" | "enabling" | "subsystem"
    pub projects: Vec<String>,
    pub created: String,
    pub updated: String,
}
```

### Key functions (`store.rs`)

| Function | Purpose |
|---|---|
| `orgs_base_dir()` | `~/.harness/orgs/` |
| `team_store_dir(org, team)` | `~/.harness/orgs/{org}/teams/{team}/` |
| `save_agent(org, team, name, content, backup)` | Write agent; if `backup=true` copies old to `.history/` |
| `inject_team_context(content, team, type, mission)` | Inject/replace `## Team Context` section |
| `build_playbook_section(...)` | Generate typed playbook section with coordination notes |
| `default_agents_for_type(type)` | Return `(role, description)` pairs for the type template |

### Key functions (`cli.rs`)

| Function | Purpose |
|---|---|
| `cmd_default()` | Interactive 4-phase design flow |
| `sync_to_project(org, team)` | Copy + inject Claude agents into `.claude/agents/{team}/`; write Codex TOML agents only if `~/.codex` already exists |
| `cmd_list` | List teams with type + project count |
| `cmd_show` | Show config, mission, agents (+ playbook with `--playbook`) |
| `cmd_sync` | Re-sync from global store to project |
| `cmd_link` | Sync + register project in config |
| `cmd_unlink` | Remove the project Claude copy and any owned Codex TOML agents |
| `cmd_delete` | No flag: remove the project Claude copy and owned Codex TOML agents. `--global`: permanently delete from org store (prompts confirmation) |
| `cmd_history` | List `.history/` backups for an agent |
