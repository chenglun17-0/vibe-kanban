import {
  CommandExitStatus,
  ExecutionProcess,
  ExecutionProcessStatus,
  NormalizedEntry,
  PatchType,
  TokenUsageInfo,
  ToolStatus,
} from 'shared/types';
import { useExecutionProcessesContext } from '@/contexts/ExecutionProcessesContext';
import { useEntries } from '@/contexts/EntriesContext';
import { useCallback, useEffect, useMemo, useRef } from 'react';
import { NativeHistoryError, sessionsApi } from '@/lib/api';
import { streamJsonPatchEntries } from '@/utils/streamJsonPatchEntries';
import type {
  AddEntryType,
  ExecutionProcessStateStore,
  OnEntriesUpdated,
  PatchTypeWithKey,
  UseConversationHistoryParams,
  UseConversationHistoryResult,
} from './types';
import { makeLoadingPatch, nextActionPatch } from './constants';
import { affectsTaskStatus } from '@/constants/processes';

export type {
  AddEntryType,
  OnEntriesUpdated,
  PatchTypeWithKey,
  DisplayEntry,
  AggregatedPatchGroup,
  AggregatedDiffGroup,
} from './types';

export { isAggregatedGroup, isAggregatedDiffGroup } from './types';

/** Pseudo state key holding the native (materialized) session history block. */
const NATIVE_STATE_KEY = 'native';

// Native history is fetched once per session and retried briefly to ride out
// the agent's final file flush after a process completes.
const fetchNativeHistory = async (sessionId: string): Promise<PatchType[]> => {
  const delays = [0, 100, 250, 500, 1000];
  let lastError: unknown;
  for (const delay of delays) {
    if (delay > 0) {
      await new Promise((resolve) => setTimeout(resolve, delay));
    }
    try {
      return await sessionsApi.getConversationHistory(sessionId);
    } catch (err) {
      lastError = err;
      const retryable =
        err instanceof NativeHistoryError &&
        (err.retryable || err.code === 'native_session_file_not_found');
      if (!retryable) break;
    }
  }
  throw lastError;
};

const makeNativeErrorEntries = (message: string): PatchTypeWithKey[] => [
  {
    type: 'NORMALIZED_ENTRY',
    content: {
      timestamp: null,
      entry_type: {
        type: 'error_message',
        error_type: { type: 'other' },
      },
      content: message,
    },
    patchKey: `${NATIVE_STATE_KEY}:error`,
    executionProcessId: NATIVE_STATE_KEY,
  },
];

export const useConversationHistory = ({
  attempt,
  onEntriesUpdated,
}: UseConversationHistoryParams): UseConversationHistoryResult => {
  const { executionProcessesVisible: executionProcessesRaw } =
    useExecutionProcessesContext();
  const { setTokenUsageInfo } = useEntries();
  const executionProcesses = useRef<ExecutionProcess[]>(executionProcessesRaw);
  const displayedExecutionProcesses = useRef<ExecutionProcessStateStore>({});
  const loadedInitialEntries = useRef(false);
  const streamingProcessIdsRef = useRef<Set<string>>(new Set());
  const onEntriesUpdatedRef = useRef<OnEntriesUpdated | null>(null);
  const previousStatusMapRef = useRef<Map<string, ExecutionProcessStatus>>(
    new Map()
  );

  const mergeIntoDisplayed = (
    mutator: (state: ExecutionProcessStateStore) => void
  ) => {
    const state = displayedExecutionProcesses.current;
    mutator(state);
  };
  useEffect(() => {
    onEntriesUpdatedRef.current = onEntriesUpdated;
  }, [onEntriesUpdated]);

  // Keep executionProcesses up to date
  useEffect(() => {
    executionProcesses.current = executionProcessesRaw.filter(
      (ep) =>
        affectsTaskStatus(ep) &&
        (ep.run_reason === 'setupscript' ||
          ep.run_reason === 'cleanupscript' ||
          ep.run_reason === 'codingagent')
    );
  }, [executionProcessesRaw]);

  const loadEntriesForHistoricExecutionProcess = (
    executionProcess: ExecutionProcess
  ) => {
    let url = '';
    if (executionProcess.executor_action.typ.type === 'ScriptRequest') {
      url = `/api/execution-processes/${executionProcess.id}/raw-logs/ws`;
    } else {
      url = `/api/execution-processes/${executionProcess.id}/normalized-logs/ws`;
    }

    return new Promise<PatchType[]>((resolve) => {
      const controller = streamJsonPatchEntries<PatchType>(url, {
        onFinished: (allEntries) => {
          controller.close();
          resolve(allEntries);
        },
        onError: (err) => {
          console.warn(
            `Error loading entries for historic execution process ${executionProcess.id}`,
            err
          );
          controller.close();
          resolve([]);
        },
      });
    });
  };

  const getLiveExecutionProcess = (
    executionProcessId: string
  ): ExecutionProcess | undefined => {
    return executionProcesses?.current.find(
      (executionProcess) => executionProcess.id === executionProcessId
    );
  };

  const patchWithKey = (
    patch: PatchType,
    executionProcessId: string,
    index: number | 'user'
  ) => {
    return {
      ...patch,
      patchKey: `${executionProcessId}:${index}`,
      executionProcessId,
    };
  };

  const getActiveAgentProcesses = (): ExecutionProcess[] => {
    return (
      executionProcesses?.current.filter(
        (p) =>
          affectsTaskStatus(p) &&
          p.status === ExecutionProcessStatus.running &&
          p.run_reason !== 'devserver'
      ) ?? []
    );
  };

  const flattenEntriesForEmit = useCallback(
    (executionProcessState: ExecutionProcessStateStore): PatchTypeWithKey[] => {
      // Flags to control Next Action bar emit
      let hasPendingApproval = false;
      let hasRunningProcess = false;
      let lastProcessFailedOrKilled = false;
      let needsSetup = false;
      let setupHelpText: string | undefined;
      let latestTokenUsageInfo: TokenUsageInfo | null = null;

      // Create user messages + tool calls for setup/cleanup scripts
      const allEntries = Object.values(executionProcessState)
        .sort(
          (a, b) =>
            new Date(
              a.executionProcess.created_at as unknown as string
            ).getTime() -
            new Date(
              b.executionProcess.created_at as unknown as string
            ).getTime()
        )
        .flatMap((p, index) => {
          // Native history entries are already final: no prompt synthesis.
          // Token usage is lifted out for the separate display, as with
          // streamed processes.
          if (p.native) {
            const tokenUsageEntry = p.entries.findLast(
              (e) =>
                e.type === 'NORMALIZED_ENTRY' &&
                e.content.entry_type.type === 'token_usage_info'
            );
            if (tokenUsageEntry?.type === 'NORMALIZED_ENTRY') {
              latestTokenUsageInfo = tokenUsageEntry.content
                .entry_type as TokenUsageInfo;
            }
            return p.entries.filter(
              (e) =>
                e.type !== 'NORMALIZED_ENTRY' ||
                e.content.entry_type.type !== 'token_usage_info'
            );
          }
          const entries: PatchTypeWithKey[] = [];
          if (
            p.executionProcess.executor_action.typ.type ===
              'CodingAgentInitialRequest' ||
            p.executionProcess.executor_action.typ.type ===
              'CodingAgentFollowUpRequest' ||
            p.executionProcess.executor_action.typ.type === 'ReviewRequest'
          ) {
            // New user message
            const actionType = p.executionProcess.executor_action.typ;
            const userNormalizedEntry: NormalizedEntry = {
              entry_type: {
                type: 'user_message',
              },
              content: actionType.prompt,
              timestamp: null,
            };
            const userPatch: PatchType = {
              type: 'NORMALIZED_ENTRY',
              content: userNormalizedEntry,
            };
            const userPatchTypeWithKey = patchWithKey(
              userPatch,
              p.executionProcess.id,
              'user'
            );
            entries.push(userPatchTypeWithKey);

            // Extract latest token usage info before filtering
            const tokenUsageEntry = p.entries.findLast(
              (e) =>
                e.type === 'NORMALIZED_ENTRY' &&
                e.content.entry_type.type === 'token_usage_info'
            );
            if (tokenUsageEntry?.type === 'NORMALIZED_ENTRY') {
              latestTokenUsageInfo = tokenUsageEntry.content
                .entry_type as TokenUsageInfo;
            }

            // Remove user messages (replaced with custom one) and token usage info (displayed separately)
            const entriesExcludingUser = p.entries.filter(
              (e) =>
                e.type !== 'NORMALIZED_ENTRY' ||
                (e.content.entry_type.type !== 'user_message' &&
                  e.content.entry_type.type !== 'token_usage_info')
            );

            const hasPendingApprovalEntry = entriesExcludingUser.some(
              (entry) => {
                if (entry.type !== 'NORMALIZED_ENTRY') return false;
                const entryType = entry.content.entry_type;
                return (
                  entryType.type === 'tool_use' &&
                  entryType.status.status === 'pending_approval'
                );
              }
            );

            if (hasPendingApprovalEntry) {
              hasPendingApproval = true;
            }

            entries.push(...entriesExcludingUser);

            const liveProcessStatus = getLiveExecutionProcess(
              p.executionProcess.id
            )?.status;
            const isProcessRunning =
              liveProcessStatus === ExecutionProcessStatus.running;
            const processFailedOrKilled =
              liveProcessStatus === ExecutionProcessStatus.failed ||
              liveProcessStatus === ExecutionProcessStatus.killed;

            if (isProcessRunning) {
              hasRunningProcess = true;
            }

            if (
              processFailedOrKilled &&
              index === Object.keys(executionProcessState).length - 1
            ) {
              lastProcessFailedOrKilled = true;

              // Check if this failed process has a SetupRequired entry
              const hasSetupRequired = entriesExcludingUser.some((entry) => {
                if (entry.type !== 'NORMALIZED_ENTRY') return false;
                if (
                  entry.content.entry_type.type === 'error_message' &&
                  entry.content.entry_type.error_type.type === 'setup_required'
                ) {
                  setupHelpText = entry.content.content;
                  return true;
                }
                return false;
              });

              if (hasSetupRequired) {
                needsSetup = true;
              }
            }

            if (isProcessRunning && !hasPendingApprovalEntry) {
              entries.push(makeLoadingPatch(p.executionProcess.id));
            }
          } else if (
            p.executionProcess.executor_action.typ.type === 'ScriptRequest'
          ) {
            // Add setup and cleanup script as a tool call
            let toolName = '';
            switch (p.executionProcess.executor_action.typ.context) {
              case 'SetupScript':
                toolName = 'Setup Script';
                break;
              case 'CleanupScript':
                toolName = 'Cleanup Script';
                break;
              case 'ToolInstallScript':
                toolName = 'Tool Install Script';
                break;
              default:
                return [];
            }

            const executionProcess = getLiveExecutionProcess(
              p.executionProcess.id
            );

            if (executionProcess?.status === ExecutionProcessStatus.running) {
              hasRunningProcess = true;
            }

            if (
              (executionProcess?.status === ExecutionProcessStatus.failed ||
                executionProcess?.status === ExecutionProcessStatus.killed) &&
              index === Object.keys(executionProcessState).length - 1
            ) {
              lastProcessFailedOrKilled = true;
            }

            const exitCode = Number(executionProcess?.exit_code) || 0;
            const exit_status: CommandExitStatus | null =
              executionProcess?.status === 'running'
                ? null
                : {
                    type: 'exit_code',
                    code: exitCode,
                  };

            const toolStatus: ToolStatus =
              executionProcess?.status === ExecutionProcessStatus.running
                ? { status: 'created' }
                : exitCode === 0
                  ? { status: 'success' }
                  : { status: 'failed' };

            const output = p.entries.map((line) => line.content).join('\n');

            const toolNormalizedEntry: NormalizedEntry = {
              entry_type: {
                type: 'tool_use',
                tool_name: toolName,
                action_type: {
                  action: 'command_run',
                  command: p.executionProcess.executor_action.typ.script,
                  result: {
                    output,
                    exit_status,
                  },
                },
                status: toolStatus,
              },
              content: toolName,
              timestamp: null,
            };
            const toolPatch: PatchType = {
              type: 'NORMALIZED_ENTRY',
              content: toolNormalizedEntry,
            };
            const toolPatchWithKey: PatchTypeWithKey = patchWithKey(
              toolPatch,
              p.executionProcess.id,
              0
            );

            entries.push(toolPatchWithKey);
          }

          return entries;
        });

      // Native-owned history removes per-process state; recover the "last
      // process failed" signal from the live process list so the next-action
      // bar still offers retry.
      if (!lastProcessFailedOrKilled) {
        const lastCodingProcess = (executionProcesses?.current ?? [])
          .filter((process) => process.run_reason === 'codingagent')
          .sort(
            (a, b) =>
              new Date(a.created_at as unknown as string).getTime() -
              new Date(b.created_at as unknown as string).getTime()
          )
          .at(-1);
        if (
          lastCodingProcess &&
          (lastCodingProcess.status === ExecutionProcessStatus.failed ||
            lastCodingProcess.status === ExecutionProcessStatus.killed) &&
          !executionProcessState[lastCodingProcess.id]
        ) {
          lastProcessFailedOrKilled = true;
        }
      }

      // Emit the next action bar if no process running
      if (!hasRunningProcess && !hasPendingApproval) {
        allEntries.push(
          nextActionPatch(
            lastProcessFailedOrKilled,
            Object.keys(executionProcessState).filter(
              (key) => key !== NATIVE_STATE_KEY
            ).length,
            needsSetup,
            setupHelpText
          )
        );
      }

      // Update token usage info in context
      setTokenUsageInfo(latestTokenUsageInfo);

      return allEntries;
    },
    [setTokenUsageInfo]
  );

  const emitEntries = useCallback(
    (
      executionProcessState: ExecutionProcessStateStore,
      addEntryType: AddEntryType,
      loading: boolean
    ) => {
      const entries = flattenEntriesForEmit(executionProcessState);
      let modifiedAddEntryType = addEntryType;

      // Modify so that if last entry is ExitPlanMode, emit special plan type
      if (entries.length > 0) {
        const lastEntry = entries[entries.length - 1];
        if (
          lastEntry.type === 'NORMALIZED_ENTRY' &&
          lastEntry.content.entry_type.type === 'tool_use' &&
          lastEntry.content.entry_type.tool_name === 'ExitPlanMode'
        ) {
          modifiedAddEntryType = 'plan';
        }
      }

      onEntriesUpdatedRef.current?.(entries, modifiedAddEntryType, loading);
    },
    [flattenEntriesForEmit]
  );

  // This emits its own events as they are streamed
  const loadRunningAndEmit = useCallback(
    (executionProcess: ExecutionProcess): Promise<void> => {
      return new Promise((resolve, reject) => {
        let url = '';
        if (executionProcess.executor_action.typ.type === 'ScriptRequest') {
          url = `/api/execution-processes/${executionProcess.id}/raw-logs/ws`;
        } else {
          url = `/api/execution-processes/${executionProcess.id}/normalized-logs/ws`;
        }
        const controller = streamJsonPatchEntries<PatchType>(url, {
          onEntries(entries) {
            const patchesWithKey = entries.map((entry, index) =>
              patchWithKey(entry, executionProcess.id, index)
            );
            mergeIntoDisplayed((state) => {
              state[executionProcess.id] = {
                executionProcess,
                entries: patchesWithKey,
              };
            });
            emitEntries(displayedExecutionProcesses.current, 'running', false);
          },
          onFinished: () => {
            emitEntries(displayedExecutionProcesses.current, 'running', false);
            controller.close();
            resolve();
          },
          onError: () => {
            controller.close();
            reject();
          },
        });
      });
    },
    [emitEntries]
  );

  // Sometimes it can take a few seconds for the stream to start, wrap the loadRunningAndEmit method
  const loadRunningAndEmitWithBackoff = useCallback(
    async (executionProcess: ExecutionProcess) => {
      for (let i = 0; i < 20; i++) {
        try {
          await loadRunningAndEmit(executionProcess);
          break;
        } catch (_) {
          await new Promise((resolve) => setTimeout(resolve, 500));
        }
      }
    },
    [loadRunningAndEmit]
  );

  // Completed-session history: one native fetch for coding-agent content
  // (replacing per-process normalized-log replay), plus per-process raw-log
  // cards for setup/cleanup scripts.
  const loadHistoricEntries =
    useCallback(async (): Promise<ExecutionProcessStateStore> => {
      const local: ExecutionProcessStateStore = {};
      const processes = [...(executionProcesses?.current ?? [])];

      for (const process of processes) {
        if (process.status === ExecutionProcessStatus.running) continue;
        if (process.executor_action.typ.type !== 'ScriptRequest') continue;
        const entries = await loadEntriesForHistoricExecutionProcess(process);
        local[process.id] = {
          executionProcess: process,
          entries: entries.map((entry, idx) =>
            patchWithKey(entry, process.id, idx)
          ),
        };
      }

      const completedCoding = processes.filter(
        (p) =>
          p.run_reason === 'codingagent' &&
          p.status !== ExecutionProcessStatus.running
      );
      const sessionId = attempt.session?.id;
      if (completedCoding.length === 0 || !sessionId) return local;

      try {
        const entries = await fetchNativeHistory(sessionId);
        local[NATIVE_STATE_KEY] = {
          executionProcess: completedCoding[0],
          entries: entries.map((entry, idx) =>
            patchWithKey(entry, NATIVE_STATE_KEY, idx)
          ),
          native: true,
        };
      } catch (err) {
        const message =
          err instanceof NativeHistoryError
            ? `${err.message} (${err.code})`
            : 'Failed to load conversation history';
        local[NATIVE_STATE_KEY] = {
          executionProcess: completedCoding[0],
          entries: makeNativeErrorEntries(message),
          native: true,
        };
      }
      return local;
    }, [attempt.session?.id]);

  const ensureProcessVisible = useCallback((p: ExecutionProcess) => {
    mergeIntoDisplayed((state) => {
      if (!state[p.id]) {
        state[p.id] = {
          executionProcess: {
            id: p.id,
            created_at: p.created_at,
            updated_at: p.updated_at,
            executor_action: p.executor_action,
          },
          entries: [],
        };
      }
    });
  }, []);

  const idListKey = useMemo(
    () => executionProcessesRaw?.map((p) => p.id).join(','),
    [executionProcessesRaw]
  );

  const idStatusKey = useMemo(
    () => executionProcessesRaw?.map((p) => `${p.id}:${p.status}`).join(','),
    [executionProcessesRaw]
  );

  // Initial load when attempt changes
  useEffect(() => {
    let cancelled = false;
    (async () => {
      // Waiting for execution processes to load
      if (
        executionProcesses?.current.length === 0 ||
        loadedInitialEntries.current
      )
        return;

      // Historic entries: one native fetch + script cards.
      const historic = await loadHistoricEntries();
      if (cancelled) return;
      mergeIntoDisplayed((state) => {
        Object.assign(state, historic);
      });
      emitEntries(displayedExecutionProcesses.current, 'initial', false);
      loadedInitialEntries.current = true;
    })();
    return () => {
      cancelled = true;
    };
  }, [attempt.id, idListKey, loadHistoricEntries, emitEntries]); // include idListKey so new processes trigger reload

  // A finished coding-agent process: drop its streamed overlay entries and
  // re-read the native session file, which now owns the completed turn.
  const handleCodingProcessFinished = useCallback(
    async (process: ExecutionProcess) => {
      streamingProcessIdsRef.current.delete(process.id);
      mergeIntoDisplayed((state) => {
        delete state[process.id];
      });

      const sessionId = attempt.session?.id;
      if (!sessionId) {
        emitEntries(displayedExecutionProcesses.current, 'historic', false);
        return;
      }
      // Grace period for the agent's final flush of its session file.
      await new Promise((resolve) => setTimeout(resolve, 300));
      try {
        const entries = await fetchNativeHistory(sessionId);
        mergeIntoDisplayed((state) => {
          const existing = state[NATIVE_STATE_KEY];
          state[NATIVE_STATE_KEY] = {
            executionProcess: existing?.executionProcess ?? process,
            entries: entries.map((entry, idx) =>
              patchWithKey(entry, NATIVE_STATE_KEY, idx)
            ),
            native: true,
          };
        });
      } catch (err) {
        const message =
          err instanceof NativeHistoryError
            ? `${err.message} (${err.code})`
            : 'Failed to load conversation history';
        mergeIntoDisplayed((state) => {
          const existing = state[NATIVE_STATE_KEY];
          state[NATIVE_STATE_KEY] = {
            executionProcess: existing?.executionProcess ?? process,
            entries: makeNativeErrorEntries(message),
            native: true,
          };
        });
      }
      emitEntries(displayedExecutionProcesses.current, 'historic', false);
    },
    [attempt.session?.id, emitEntries]
  );

  useEffect(() => {
    const activeProcesses = getActiveAgentProcesses();
    if (activeProcesses.length === 0) return;

    for (const activeProcess of activeProcesses) {
      if (!displayedExecutionProcesses.current[activeProcess.id]) {
        const runningOrInitial =
          Object.keys(displayedExecutionProcesses.current).length > 1
            ? 'running'
            : 'initial';
        ensureProcessVisible(activeProcess);
        emitEntries(
          displayedExecutionProcesses.current,
          runningOrInitial,
          false
        );
      }

      if (
        activeProcess.status === ExecutionProcessStatus.running &&
        !streamingProcessIdsRef.current.has(activeProcess.id)
      ) {
        streamingProcessIdsRef.current.add(activeProcess.id);
        loadRunningAndEmitWithBackoff(activeProcess).finally(() => {
          streamingProcessIdsRef.current.delete(activeProcess.id);
        });
      }
    }
  }, [
    attempt.id,
    idStatusKey,
    emitEntries,
    ensureProcessVisible,
    loadRunningAndEmitWithBackoff,
  ]);

  useEffect(() => {
    if (!executionProcessesRaw) return;

    const finishedScripts: ExecutionProcess[] = [];

    for (const process of executionProcessesRaw) {
      const previousStatus = previousStatusMapRef.current.get(process.id);
      const currentStatus = process.status;
      const justFinished =
        previousStatus === ExecutionProcessStatus.running &&
        currentStatus !== ExecutionProcessStatus.running;

      if (justFinished && process.run_reason === 'codingagent') {
        // Coding turns are re-read from the native session file.
        void handleCodingProcessFinished(process);
      } else if (
        justFinished &&
        displayedExecutionProcesses.current[process.id]
      ) {
        // Scripts keep their raw-log cards, refreshed on completion.
        finishedScripts.push(process);
      }

      previousStatusMapRef.current.set(process.id, currentStatus);
    }

    if (finishedScripts.length === 0) return;

    (async () => {
      let anyUpdated = false;

      for (const process of finishedScripts) {
        const entries = await loadEntriesForHistoricExecutionProcess(process);
        if (entries.length === 0) continue;

        const entriesWithKey = entries.map((e, idx) =>
          patchWithKey(e, process.id, idx)
        );

        mergeIntoDisplayed((state) => {
          state[process.id] = {
            executionProcess: process,
            entries: entriesWithKey,
          };
        });
        anyUpdated = true;
      }

      if (anyUpdated) {
        emitEntries(displayedExecutionProcesses.current, 'running', false);
      }
    })();
  }, [
    idStatusKey,
    executionProcessesRaw,
    emitEntries,
    handleCodingProcessFinished,
  ]);

  // If an execution process is removed, remove it from the state
  useEffect(() => {
    if (!executionProcessesRaw) return;

    const removedProcessIds = Object.keys(
      displayedExecutionProcesses.current
    ).filter(
      (id) =>
        id !== NATIVE_STATE_KEY &&
        !executionProcessesRaw.some((p) => p.id === id)
    );

    if (removedProcessIds.length > 0) {
      mergeIntoDisplayed((state) => {
        removedProcessIds.forEach((id) => {
          delete state[id];
        });
      });
    }
  }, [attempt.id, idListKey, executionProcessesRaw]);

  useEffect(() => {
    displayedExecutionProcesses.current = {};
    loadedInitialEntries.current = false;
    streamingProcessIdsRef.current.clear();
    previousStatusMapRef.current.clear();
    emitEntries(displayedExecutionProcesses.current, 'initial', true);
  }, [attempt.id, emitEntries]);

  return {};
};
