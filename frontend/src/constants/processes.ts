import type { ExecutionProcess, ExecutionProcessRunReason } from 'shared/types';

// Process run reasons
export const PROCESS_RUN_REASONS = {
  SETUP_SCRIPT: 'setupscript' as ExecutionProcessRunReason,
  CLEANUP_SCRIPT: 'cleanupscript' as ExecutionProcessRunReason,
  CODING_AGENT: 'codingagent' as ExecutionProcessRunReason,
  DEV_SERVER: 'devserver' as ExecutionProcessRunReason,
} as const;

export const affectsTaskStatus = (process: ExecutionProcess): boolean => {
  return process.executor_action.affects_task_status !== false;
};

export const isCodingAgent = (
  runReason: ExecutionProcessRunReason
): boolean => {
  return runReason === PROCESS_RUN_REASONS.CODING_AGENT;
};

export const shouldShowInLogs = (
  runReason: ExecutionProcessRunReason
): boolean => {
  return runReason !== PROCESS_RUN_REASONS.DEV_SERVER;
};
