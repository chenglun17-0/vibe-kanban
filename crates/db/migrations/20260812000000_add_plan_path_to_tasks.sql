-- Link a task to the exec-plan document it was created from.
-- Format: "<repo_id>:<repo-relative plan path>" (e.g. "<uuid>:docs/exec-plan/agents/foo.md").
-- Used to filter runnable plans that already have a task out of the To Do column.
ALTER TABLE tasks ADD COLUMN plan_path TEXT;
