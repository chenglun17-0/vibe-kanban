-- Remove exec-plan linkage used by the runnable-plans listener.
ALTER TABLE tasks DROP COLUMN plan_path;
