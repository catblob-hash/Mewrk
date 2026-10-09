---
name: Mewrk SDK
description: How to configure Mewrk itself: skills, MCP servers, hooks, subagent roles, language servers, prompt profiles, dev-server launch configs, project instructions and memory, and which settings live only in the app
when_to_use: The user asks to set up, change, explain or troubleshoot a Mewrk feature, or a file under ~/.mewrk or a workspace's .mewrk
---

# Mewrk SDK

This guide is built into Mewrk {{MEWRK_VERSION}} and describes exactly that version. It is how you configure Mewrk for the user: almost everything is a plain file you can write with your file tools, and the rest is a setting the user changes in the app, where your job is to tell them precisely where.

## Ground rules

- **A file makes a capability available; it does not switch it on.** Skills, MCP servers, hooks and subagent roles only take effect in a conversation that selects them, and you cannot tick those boxes. After writing one, tell the user to open **More options** (⋮ in the top bar) → **Conversation settings**, go to the **Skills**, **MCP**, **Hooks** or **Agent roles** page, press **Rescan** if the page was already open, and tick the entry. Language servers and `launch.json` need no selection.
- **Pick the scope deliberately.** `~/.mewrk/` is the global level, and there is one: on this computer, the one Mewrk is installed on. Every conversation sees it; it is personal and never committed. `<workspace>/.mewrk/` is a project level and belongs to that one workspace: it is usually committed with the project, and other people who open the project get it. Put personal tools and credentials-bearing servers in `~/.mewrk`; put project conventions in the workspace.
- **A conversation uses the union of its levels.** A conversation can work in several workspaces — its project's, plus any it attached — each a folder on some machine. Its Skills, MCP, Hooks and Agent roles pages list the global level and then each workspace's, headed by the workspace's absolute path, and a conversation may tick entries from any of them. How far each kind reaches: a **skill** has no isolation — every ticked skill serves the whole conversation, and its scripts are in the folder it was declared in; an **MCP server** is bound to a machine — a workspace's server runs on that workspace's machine and may be used for any workspace there, a global one runs on this computer; a **hook** is bound to its workspace — it runs in that workspace's folder and sees that workspace's tool calls plus the conversation's own events (`SessionStart`, `UserPromptSubmit`, `Stop`, calls that work in no one workspace), while a global hook sees everything; a **role** belongs to its level — a workspace's role can be selected only by conversations working in that workspace.
- **Never write a secret into a file.** Reference it as `${VAR}` (MCP and LSP files expand `${VAR}` and `${VAR:-default}`) and tell the user to set the variable before starting Mewrk.
- **These are the user's files.** Read before you write, change only the entry you were asked about, keep every other key, entry and the existing formatting, and make sure the result is valid UTF-8 JSON. Mewrk itself never creates or rewrites them, except that the delete buttons in the settings pane remove one entry, one skill folder or one role file, and the role editor writes the role files under `agents/`.
- **Remote workspaces.** A workspace on a WSL distribution or an SSH machine works like a local one: its `.mewrk/skills/`, `agents/`, `mcp.json`, `hooks.json`, `lsp.json` and `launch.json` are read on that machine, so write them there with that workspace's file tools. Its stdio servers and hook commands run there, in the workspace folder (or the conversation's worktree of it), with that machine's environment plus the workspace's variables; its HTTP servers are reached through that machine's network, and its skills' scripts are on that machine. `~/.mewrk` is this computer's, and what it declares runs here. `tool-descriptions/` is read on this computer only. The temporary project's scratch folder has no `.mewrk` of its own.
- **Timing.** Every run re-reads the files, so a saved change applies from the next turn. While the settings pane is open its lists refresh by themselves a few seconds after a file or skill folder changes; **Rescan** refreshes them at once.
- **Plan mode.** While it is on, `write` and `edit` refuse files Git tracks or would track; a workspace `.mewrk/` inside a repository is such a file. Say so and wait for the plan to be approved.
- A legacy `.naiword` directory is read in place of `.mewrk` only when `.mewrk` lacks that file. Always create new files under `.mewrk`.

## Where everything lives

```text
~/.mewrk/                          <workspace>/.mewrk/
  skills/<dir>/SKILL.md               skills/<dir>/SKILL.md       skills (select per conversation)
  mcp.json                            mcp.json                    MCP servers (select per conversation)
  hooks.json                          hooks.json                  hooks (select per conversation)
  agents/*.json                       agents/*.json               subagent roles (select per conversation)
  lsp.json                            lsp.json                    language servers (by file extension)
  tool-descriptions/*.json                                        prompt profiles (global only; select per conversation)
  MEWRK.md, rules/**/*.md            MEWRK.md, rules/**/*.md    standing instructions
  memory/                             memory/                     long-term memory (use the memory tools)
                                      launch.json                 dev servers for the preview tools
```

A workspace's root may also hold `MEWRK.md` and a personal, uncommitted `MEWRK.local.md`.

## Skills

A skill is a folder holding `SKILL.md`: instructions for one kind of task, optionally with scripts and reference files beside it. The format is Claude Code's, so existing skills work unchanged.

```markdown
---
name: Commit helper
description: Prepare a conventional commit from the current diff
when_to_use: The user asks to commit or to write a commit message
---

# Commit helper

1. Run `git status` and `git diff --staged`.
2. Write a conventional commit message and check it with `scripts/check-message.sh`.
```

- The frontmatter is top-level `key: value` lines; `description` and `when_to_use` may also run over several lines with YAML's `|` or `>`. Only `name` (the label in the list), `description` and `when_to_use` matter: the model is shown `description - when_to_use` as the trigger, so write them as "what it does" and "when to load it". `author`, `version` and `tags` are parsed and dropped; every other Claude Code key (`allowed-tools`, `model`, `disable-model-invocation`, …) is ignored.
- The body is what the model reads, verbatim: write it as instructions to a model. Relative paths in it are relative to the skill's folder, which the model is told in both delivery modes.
- **The folder name is the skill's identity**: the model loads it by that name. It must be a direct child of a `skills/` folder, not a symlink, non-empty, without leading or trailing spaces, and free of `(`, `)`, `,` and control characters. `SKILL.md` must be a regular file of at most 256 KiB of UTF-8.
- Two selected skills with the same folder name make on-demand loading fail; rename one.
- The conversation's **Load skills on demand** switch (under the Skills list) decides delivery. Off: the bodies are pasted into the system prompt. On: the prompt lists `name: trigger` lines and the `skill` tool loads a body when needed.
- A skill selected after a conversation has started arrives at that point in the transcript as a host message; the earlier prompt is left alone.
- A selected skill whose folder was moved, renamed or deleted shows as *Dangling*, and every run fails, naming it, until the user unticks it; one that is found but unreadable fails the run with the reason.

This skill, `mewrk-sdk`, is built into the app. It has no folder, cannot be edited or deleted, and is replaced by each update.

## MCP servers

Mewrk is an MCP client for **stdio** and **Streamable HTTP** servers. Declare servers under a top-level `mcpServers` object, the shape of Claude Code's `.mcp.json`:

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/Users/me/projects"],
      "env": { "LOG_LEVEL": "info" }
    },
    "docs": {
      "type": "http",
      "url": "https://docs.example.com/mcp",
      "headers": { "Authorization": "Bearer ${DOCS_TOKEN}" }
    }
  }
}
```

| Key | Meaning |
|---|---|
| `type` | `stdio`, or `http` (also `streamable-http`, `streamable_http`). Omitted with a `command`, it is stdio. A `url` needs `"type": "http"`. |
| `command`, `args`, `env`, `cwd` | stdio. `command` runs directly, never through a shell, so give an executable name, a full path, or a path relative to the folder it starts in, not a pipeline. `cwd` must be an existing absolute directory; without it a workspace's server starts in that workspace's folder, and a global server in the conversation's first workspace on this computer (the home folder when it has none here). |
| `workspace` | Global servers only (`~/.mewrk/mcp.json`), stdio only: `true` says the server works on a workspace's files or depends on the folder it runs in. When the conversation has two or more workspaces on this computer, each of its tools gains a `workspace` parameter naming one of them, and the call goes to an instance of the server started in that workspace's folder (in place of `cwd`). Mewrk takes the parameter out before the server sees the call. |
| `envPassthrough` | Names of Mewrk's own environment variables to pass on. The process otherwise starts from a cleared environment plus `PATH`, `HOME`, temp and similar basics. Names starting with `MEWRK_`, `ANTHROPIC_`, `OPENAI_`, `CLAUDE_`, `CODEX_`, `AWS_`, `AZURE_`, `GOOGLE_`, `GEMINI_` or `DEEPSEEK_` are refused. |
| `url`, `headers` | http. Public hosts need `https`; loopback and private addresses may use `http`. |
| `timeoutSeconds` | Per-request timeout in seconds, at most 300; `0` means the 45 s default. Wins over Claude Code's `timeout`, which is in milliseconds. |
| `longRunning` | `true` gives calls the 5-minute ceiling instead of 45 s when no timeout is set. |
| `description` | Shown in the list and to the model when selected (240 characters). |
| `registryUrl` | Package mirror for `npx`/`npm`/`bun`/`pnpm`/`yarn` and `uv`/`pip`/`python` commands. |
| `disabledTools` | Tool names never offered to the model. |
| `disabledAutoApproveTools` | Tool names that ask for approval on every call, even at Full access. |

- Server names may contain only letters, digits, `-` and `_`.
- Not supported, and listed as unavailable with the reason: `"type": "sse"`, `"ws"`, `"sdk"`, `headersHelper`, `oauth`, and a `${VAR}` that is unset and has no default. A top-level `servers` key or an array is not read at all.
- Only a server's tools reach the model, not its prompts or resources. Its tools are named `mcp__<server>_<10 hex>__<tool>__<10 hex>`: the server name lowercased and cut to 12 letters, digits and `_`, the tool name to 18, and two 10-digit hex digests. To match every tool of one server in a hook, use `^mcp__<server>_`.
- When the conversation's workspaces are not all one folder on this computer, the model is told which machine each selected server runs on and which workspaces are there; a server cannot reach the files of a workspace on another machine.
- With the conversation's **Tool discovery** switch on, MCP tool schemas are withheld and fetched with `tool_search`; off, they are all declared up front.
- Below Full access every MCP call asks for approval unless a `PreToolUse` or `PermissionRequest` hook allows it.
- To check a server, have the user press **Test connection** on its row in the MCP page: it reports the tool count, or the error plus the server's last stderr lines. Running the same command in a terminal is the next step.

## Hooks

Hooks are commands Mewrk runs at fixed points of a conversation. They read a JSON event on stdin and answer with their exit code and stdout. The format follows Claude Code's `hooks` block.

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "^(bash|zsh|sh|pwsh|powershell)$",
        "hooks": [
          {
            "type": "command",
            "name": "No force push",
            "command": "python3 .mewrk/hooks/no_force_push.py",
            "commandWindows": "python .mewrk/hooks/no_force_push.py",
            "timeout": 10
          }
        ]
      }
    ]
  }
}
```

- Events: `SessionStart`, `InstructionsLoaded`, `UserPromptSubmit`, `PreToolUse`, `PermissionRequest`, `PostToolUse`, `Stop`. Other event names are skipped.
- `matcher` is an unanchored regular expression tested against the tool name (`bash`, `zsh`, `sh`, `pwsh` for PowerShell 7, `powershell` for Windows PowerShell 5.1, `write`, `edit`, `read`, an `mcp__…` name, …) for the three tool events, against `startup` for `SessionStart`, and against the load reason for `InstructionsLoaded`. Claude Code's tool names also match the Mewrk tools doing the same job (`Bash` matches `bash`, `zsh` and `sh`; `PowerShell` matches both `pwsh` and `powershell`; `Read`, `Write`, `Edit`, `Glob` for `find`, `Grep`, `LS`, `WebFetch`, `WebSearch`, `Task`/`Agent` for `agent_spawn`), so a block copied from Claude Code works unchanged. Absent or `"*"` matches everything; an invalid regex disables the group.
- Handler fields: `type` (only `"command"`), `command`, `commandWindows` (replaces `command` on Windows), `name`, `statusMessage`, `timeout` in seconds 1–600 (default 30; out of range drops the handler). `async` is honoured only for `InstructionsLoaded`; `asyncRewake` handlers are skipped.
- Commands run through `bash -lc`, or through `pwsh`/`powershell -NoProfile -Command` on Windows, with the workspace's variables added to the environment: a workspace's hook in that workspace's folder (the conversation's worktree of it when there is one), a global hook on this computer in workspace 1's folder (a scratch folder when workspace 1 is on another machine). A hook from a WSL or SSH workspace's `hooks.json` runs on that machine, in its shell; `commandWindows` is used when that machine runs Windows. Give both `command` and `commandWindows` when the user is on Windows or shares the file across systems.
- A workspace's hook watches that workspace: the tool events of a call that works in another workspace (a `read`, a shell command or a project-memory tool naming it, that workspace's MCP servers) never reach it. Calls that work in no one workspace (`web_search`, a subagent) and the conversation's own events reach every workspace's hooks. `InstructionsLoaded` reaches the global hooks and workspace 1's, whose instruction files they are.
- Stdin carries `session_id`, `cwd`, `hook_event_name`, `model`, `permission_mode` (`default`, `acceptEdits` or `bypassPermissions`), `turn_id`, plus per event: `prompt` (`UserPromptSubmit`); `tool_name`, `tool_use_id`, `tool_input` (tool events); `tool_response` (`PostToolUse`); `stop_hook_active`, `last_assistant_message` (`Stop`). `tool_input` and `tool_response` carry Claude Code's field names beside Mewrk's (`file_path`, `old_string`, `new_string`, `command`, `filePath`, `stdout`, …), and an `updatedInput` may use either. `MEWRK_HOOK_EVENT` holds the event name and `CLAUDE_PROJECT_DIR` the folder the hook runs in.
- Exit `0`: stdout is the answer. For `SessionStart` and `UserPromptSubmit` plain text becomes added context. Exit `2`: block, with stderr as the reason. Any other exit is a failure that decides nothing.
- JSON on stdout may carry `continue: false` (halt the turn), `decision: "block"` with `reason`, `systemMessage` (shown to the user only), and `hookSpecificOutput` with a `hookEventName` equal to the event and, for `PreToolUse`, `permissionDecision` (`allow`, `ask` or `deny`), `permissionDecisionReason`, `updatedInput` (a rewritten argument object), or `additionalContext`. A `PermissionRequest` answers with `hookSpecificOutput.decision: {"behavior": "allow" | "deny"}`. A `Stop` hook that blocks makes the model keep working, at most 3 times in a row; it must print JSON if it prints anything.
- `allow` never overrides another hook's `ask` or `deny`, and some confirmations no hook can skip: dangerous recursive deletes, writes to global memory, MCP tools that require user interaction, and acting on a page the user logged into.
- **A hook's identity is what it runs and when**: its file, event, matcher, `command` and `commandWindows`. Inserting, deleting or reordering other handlers leaves a selection on the command that was ticked; changing a handler's command, event or matcher makes it a different hook, and the old selection shows as *Dangling* and fails every run until the user unticks it and ticks the new row. Its `name`, `timeout` and `statusMessage` can change freely.
- Each hook run leaves a diagnostic card in the timeline with its output and decision.

## Subagent roles

A role is a named subagent the main agent spawns by name (`agent_spawn`'s `agent_type`, a workflow step's `agentType`): the model it runs on, the tools it holds, its own skills, MCP servers, hooks and web search, and the line the main agent reads about what it is for. Each role is one JSON file directly inside `agents/`:

```json
{
  "name": "reviewer",
  "description": "Adversarial review: refutes existing conclusions and finds faults; does not write the main proposal.",
  "modelSelection": { "kind": "inherit" },
  "effort": "high",
  "tools": ["ls", "read", "find", "grep"],
  "skillIds": [],
  "mcpIds": [],
  "hookIds": [],
  "templateId": null
}
```

- `name` is what the main agent calls it by: free text, at most 64 characters, no control characters. Without one, the file name stands in. `description` is appended to the subagent and workflow tool descriptions, one line per role; empty says nothing.
- `modelSelection`: `{"kind": "inherit"}` runs on the calling conversation's model; `{"kind": "explicit", "providerId": …, "modelId": …}` binds one model of one provider. Provider ids differ per installation, so let the user pick the model in the role editor rather than writing one. `effort`: `null` follows the caller, otherwise a reasoning effort such as `low`, `medium` or `high`.
- `tools` is the role's own list, independent of what the calling conversation enables. Leave the key out to give every tool a role can hold; `[]` gives none. Orchestration tools (subagents, workflows, questions to the user, the to-do list) and the memory tools never reach a subagent, whatever the list says.
- `skillIds`, `mcpIds` and `hookIds` are the role's own selections, by catalog id; the ids are minted from file locations, so leave them empty and have the user tick them in the role editor. A role never inherits the conversation's skills or MCP servers. Hooks are different: a role's subagents always run the calling conversation's tool and permission hooks (`PreToolUse`, `PermissionRequest`, `PostToolUse`, `InstructionsLoaded`), so a guard cannot be bypassed by spawning a role; `hookIds` adds hooks of the role's own on top.
- `webSearch` is the role's own search and fetch backends, result shaping and domain filter, in the shape of a conversation's web search settings; left out, native search and fetch with the default shaping. Whether the subagent reaches the web at all still follows the calling conversation's web search switch.
- `templateId` names an opening history kept in the app; set it from the role editor's **Conversation template** page, never by hand.
- A role does nothing until a conversation selects it: Conversation settings → **Agent roles**, tick the row. The gear on a row opens the role editor and **New role** under the list creates one; both write these files. A file that does not parse is listed as unavailable, with the reason in its tooltip.
- Built-in roles (Opus, Sonnet, Sol, Luna) ship with the app and are never on disk; the built-in preset selects them. A file role named the same takes precedence over a built-in, and a workspace role over a global one; two roles of one name at the same level make that name fail as ambiguous.

## Language servers

The `lsp` tool (the **Code navigation** switch in the conversation's tools) talks to language servers. Mewrk offers these when their command is on the `PATH` it was started with: `rust-analyzer`, `typescript-language-server`, `pyright`, `gopls`, `clangd`, `lua-language-server`, `bash-language-server`. Install one in the usual way and restart Mewrk if that changed `PATH`. Anything else goes in `lsp.json`:

```json
{
  "lspServers": {
    "zls": {
      "command": "zls",
      "extensionToLanguage": { ".zig": "zig" }
    }
  }
}
```

- Required: `command` (run directly, not through a shell; arguments go in `args`) and `extensionToLanguage`. Optional: `args`, `env`, `initializationOptions`, `settings`, `workspaceFolder`, `startupTimeout` and `shutdownTimeout` (ms), `restartOnCrash`, `maxRestarts`, `diagnostics` (`false` keeps navigation but stops reporting problems), `description`. Only the stdio transport works.
- The first entry that claims an extension wins: the workspace's `lsp.json`, then `~/.mewrk/lsp.json`, then the built-in table. An entry named like a built-in replaces it, which is how to add flags.
- A workspace `.mewrk/lsp.json` makes every `lsp` call ask for approval below Full access, because the project chooses which command runs.

## Prompt profiles

Everything Mewrk itself says to the model, such as tool descriptions, receipts and the sections listing skills, MCP servers and hooks, comes from a prompt profile. The built-in English profile is the default; a JSON file in `~/.mewrk/tool-descriptions/` overrides any subset of it:

```json
{
  "name": "Terse",
  "prompts": { "task.wait_idle": "Nothing is running." },
  "tools": [
    { "toolName": "grep", "description": "Search file contents with a regular expression." }
  ]
}
```

- `prompts` maps registry keys to text; omitted keys keep the built-in wording, and `""` removes a text. Keep the `{placeholders}` a key declares; you cannot invent new ones.
- `tools[].description` replaces what a tool is said to be; an entry without one is ignored. `toolName` is a built-in tool name or a full `mcp__…` name. Files written for the field's old name, `schemaNotes`, are still read.
- A built-in tool's description (and its parameters' descriptions) may mention sibling tools through markers that are resolved on every request against the tools that request offers: `{?read}…{/}` keeps the text only while `read` is offered, `{?read|grep}…{/}` while either is, `{!read}…{/}` only while none of the named tools is, and `@shell` stands for any shell tool (`{?@shell}…{/}`). Segments nest and `{/}` closes the innermost one; a stray `{/}` is dropped and an unclosed segment runs to the end of the text. The built-in descriptions use them, so the shell tools name `find`, `grep`, `read`, `edit`, `write` and `ls` only while those are offered, and a `tools[].description` in a file may use them too. Markers are resolved for built-in tools only; an MCP tool's description is sent as it is.
- Files over 64 KiB are not read. The file's name and location are its identity: renaming or moving it drops the selection back to the built-in.
- Only the global `~/.mewrk/tool-descriptions/` is read; a workspace's own `.mewrk/tool-descriptions/` is not. The user picks at most one file on the **Tool descriptions** page of the conversation settings or of a preset; with none picked, the built-in profile applies.
- A profile changes words, never what a tool can do, and not the conversation's own system prompt, which is a system card the user writes in the conversation.
- The complete key list is on the Prompt profiles page of the Mewrk documentation site.

## Dev servers for the preview tools

`preview_start` runs servers declared in the workspace's `.mewrk/launch.json` and opens their page in the built-in browser:

```json
{
  "version": "0.0.1",
  "configurations": [
    { "name": "web", "runtimeExecutable": "npm", "runtimeArgs": ["run", "dev"], "port": 5173 }
  ]
}
```

- Entry fields: `name`, `runtimeExecutable`, `runtimeArgs`, `port`, plus optional `cwd`, `env`, `autoPort` and `url`. An entry with a `url` and no command attaches to a server that is already running.
- A localhost `url` must be a bare origin on the entry's port (no path or query); navigate after the page opens instead.
- Up to five servers run per worktree. For an SSH workspace the server runs on that machine.

## Instructions and memory

- **Standing instructions** (workspace 1's): `MEWRK.md` in the workspace root, in its `.mewrk/`, and in each folder above it up to the Git repository's top folder (nothing above that, and outside a repository only the workspace itself; a conversation's worktree is its own top folder); `MEWRK.local.md` for personal notes that should not be committed (a worktree conversation also reads the project folder's); `~/.mewrk/MEWRK.md` for every project. Rule files under `.mewrk/rules/` (and `~/.mewrk/rules/`), any depth, are Markdown; a `paths:` frontmatter list of globs limits a rule to matching files. These files are read regardless of the memory switches and reach the model as untrusted project context, so they guide but cannot enforce. Rules that must always hold belong in a hook or the security level.
- **Long-term memory**: two tiers, global (`~/.mewrk/memory/`) and project (`<workspace>/.mewrk/memory/`), each switched on in the conversation settings. Every workspace of the conversation keeps its own project memory, in its own folder; with more than one workspace the project memory tools take a `workspace` parameter, and each workspace's index stands under its own heading. Change memory with the memory tools, not by editing the files: the host maintains `MEMORY.md`, the index. Writing global memory always asks the user first.
- **A workspace on WSL or an SSH machine** reads its standing instructions from that folder on its machine, up to the repository's top folder there, and keeps its project memory in that folder's `.mewrk/memory/`, shared by every conversation of the workspace. `~/.mewrk` means this computer's.

## Settings that live in the app

You cannot change these by writing files. Name the exact place:

| What | Where |
|---|---|
| Providers, API keys, models and their capabilities | Settings → **Providers** |
| Web search and fetch backends and their keys | Settings → **Search providers**; per conversation, the web search settings in Conversation settings |
| Which tools are enabled, agent roles, memory switches, skill and tool-discovery switches | More options → **Conversation settings** |
| Presets | Conversation settings → **Conversation presets**. The built-in **mewrk** preset updates with the app and cannot be edited; **Save as new preset** makes an editable copy. A new conversation copies a preset once; later edits to either side do not propagate. |
| A workspace's default preset | ⋯ menu on the workspace's sidebar row → **Default conversation preset** |
| Security level (Manual, Accept edits, Full access) | Security-level menu in the composer |
| Plan mode | **Plan** button under the composer |
| Reasoning effort; auto-compact threshold | The **Reasoning effort** menu in the composer; the **Auto-compact** menu of the composer's context meter |
| Workspace environment variables; extra working directories | Gear beside the workspace chip; folder-plus button at the end of the chip row |
| Remote machines (WSL, SSH) and their shells | The machine's settings, with **Probe shells again** |
| Local helper model, theme, language, fonts | Settings → **Appearance** (the **Local model** card for the helper) |
| Keyboard shortcuts; dependency checks | Settings → **Keyboard shortcuts**; Settings → **Dependencies** |

Built-in presets select no skills, MCP servers or hooks, this one included: everything in those pages is opt-in per conversation. The built-in preset does select the four built-in subagent roles.

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| A skill, server, hook or role is missing from its page | Wrong place or name (a skill folder must be a direct child of `skills/`; a role is a `.json` file directly inside `agents/`; MCP needs `mcpServers`; LSP needs `lspServers`), invalid JSON, a symlink, or a WSL or SSH workspace's files written on this computer instead of on its machine. Then press **Rescan**. |
| Listed but marked unavailable | The row's tooltip gives the reason: unsupported transport, missing `${VAR}`, refused `envPassthrough`, unreadable `SKILL.md`. |
| Marked *Dangling* | A selected id is no longer found: the skill folder or `mcp.json` entry was renamed, moved or deleted, or the hook's command, event or matcher changed. Every run fails, naming it, until the user unticks it. A dangling role is simply not offered to the model; renaming a role inside its file keeps its selection, renaming or moving the file does not. |
| Configured but the model never uses it | It is not ticked in this conversation, or the model has no tool calling. |
| A run fails naming a skill, server or hook | The selection is *Dangling* or unavailable; the user unticks it, or ticks the current entry. |
| The model says a tool's schema is not loaded | Tool discovery is on; call `tool_search` with `select:<name>`. |
| `lsp` says no server is available | Nothing claims the extension: add an `lsp.json` entry or install a built-in server. |

When the user wants to see exactly what reached the model, point them to More options → **History**, which records every request, reply, hook run and tool call of the conversation.
