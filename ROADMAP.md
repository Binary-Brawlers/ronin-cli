# Ronin CLI roadmap

This roadmap records the planned release sequence after v0.3.0. It is a living document: completed items should be checked off, and scope changes should be explained in the relevant release section.

## v0.4 — Workflow completeness

- [x] Complete session lifecycle commands: rename, fork, archive, unarchive, delete, restore, trash listing, trash emptying, and integrity scanning.
- [x] Add an interactive session picker when `--resume` is used without an ID.
- [x] Expose stored permission grants with list, revoke, and workspace reset commands.
- [x] Track changes made by native file tools per turn.
- [x] Add `/diff` for the latest turn's native file changes.
- [x] Add conflict-safe `/undo` for the latest turn's native file changes.
- [x] Show a concise post-turn summary of changed files and executed commands.

## v0.5 — Extensibility

- [ ] Add MCP server configuration, discovery, testing, and permission-aware tool execution.
- [ ] Add lifecycle hooks for session, prompt, tool, edit, stop, and error events.
- [ ] Add streaming JSON output for editor, CI, and automation integrations.
- [ ] Add automation controls for maximum rounds and explicitly allowed tools.

## v0.6 — Intelligence and autonomy

- [ ] Add project and user skills.
- [ ] Add custom agents with scoped prompts, tools, models, budgets, and permissions.
- [ ] Add isolated subagent execution and optional parallel delegation.
- [ ] Add LSP-backed symbol navigation, references, and diagnostics.
- [ ] Add isolated Git worktrees and stronger OS-level sandbox profiles.

## Later candidates

- Git-native review, commit, pull-request, and issue workflows.
- Structured-output schemas for non-interactive runs.
- Detailed per-model token, cache, latency, and credit analytics.
- Team policy bundles and auditable permission exports.
