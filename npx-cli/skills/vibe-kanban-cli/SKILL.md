---
name: vibe-kanban-cli
description: Use the local Vibe Kanban CLI to discover workspace context, inspect projects and tasks, and create structured tasks. Use when the user asks to create a task, turn a plan into tasks, write work into Vibe Kanban, or inspect the current project's task board.
---

# Vibe Kanban CLI

Use the bundled CLI script. Do not use MCP or access the Vibe Kanban database directly.

```bash
VK_CLI="${HOME}/.agents/skills/vibe-kanban-cli/scripts/vibe-kanban-cli.js"
node "$VK_CLI" --help
```

Vibe Kanban must already be running. The CLI discovers its local backend through `VIBE_BACKEND_URL`, `BACKEND_PORT`, or the Vibe Kanban port file.

## Choose the project

Resolve the current workspace before any project-scoped operation:

```bash
node "$VK_CLI" context --json
```

Use `context.project.id` when context succeeds. If it fails because the current directory is not a Vibe Kanban workspace, list projects:

```bash
node "$VK_CLI" project list --json
```

Use the only project when exactly one exists. If multiple projects exist and the user did not identify one, ask the user to choose. Never guess a project.

## Inspect tasks

List tasks before creating them so obvious duplicates are visible:

```bash
node "$VK_CLI" task list --project-id <project-id> --limit 50 --json
node "$VK_CLI" task list --project-id <project-id> --status todo --limit 50 --json
```

Valid statuses are `todo`, `inprogress`, `inreview`, `done`, and `cancelled`.

Read a task when its description is needed:

```bash
node "$VK_CLI" task get --task-id <task-id> --json
```

Treat an exact normalized-title match as a likely duplicate. Do not silently skip it: report the match and ask whether to create another task unless the user already said duplicates are intentional.

## Create tasks

A task needs a non-empty title and a concrete description. Prefer descriptions that state the goal, relevant constraints, and observable acceptance criteria. Do not invent requirements that the user did not provide.

Pass task content as JSON through stdin so shell quoting cannot alter Markdown or multiline text:

```bash
node "$VK_CLI" task create --from-json - --json <<'JSON'
{
  "project_id": "<project-id>",
  "title": "<task title>",
  "description": "<Markdown description>"
}
JSON
```

Create multiple tasks with separate calls. Record each returned `task.id` and report partial success explicitly if a later call fails.

## Confirmation rules

- If the user asks only to plan, decompose, or propose tasks, show the draft and do not write it.
- If the user explicitly asks to create, add, record, or write tasks into Vibe Kanban, that is authorization to write; do not ask for redundant confirmation.
- Ask before writing when the destination project is ambiguous or a likely duplicate exists.
- Never claim success unless the CLI returns `ok: true` and a task ID.

## Failure rules

CLI errors are JSON on stderr and use a nonzero exit code.

- `BACKEND_NOT_FOUND`: ask the user to start Vibe Kanban; do not start it silently.
- `TIMEOUT` or `CONNECTION_ERROR` after a create request: list tasks and check for the intended title before retrying, because the task may have been created even when the response was lost.
- `BACKEND_ERROR`: report the backend message without converting it into success.
- Invalid or missing context: fall back to `project list`; do not fabricate IDs.

## Boundaries

This skill can read context, list projects, list and read tasks, and create tasks. It does not update or delete tasks, change repository scripts, start task execution, or manage workspaces.
