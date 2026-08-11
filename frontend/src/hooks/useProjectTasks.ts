import { useCallback, useMemo } from 'react';
import { useJsonPatchWsStream } from './useJsonPatchWsStream';
import type { TaskStatus, TaskWithAttemptStatus } from 'shared/types';

type TasksState = {
  tasks: Record<string, TaskWithAttemptStatus>;
};

export interface UseProjectTasksResult {
  tasks: TaskWithAttemptStatus[];
  tasksById: Record<string, TaskWithAttemptStatus>;
  tasksByStatus: Record<TaskStatus, TaskWithAttemptStatus[]>;
  isLoading: boolean;
  isConnected: boolean;
  error: string | null;
}

/**
 * Stream tasks via WebSocket (JSON Patch) and expose them as arrays and maps.
 * Omitting projectId subscribes to tasks from every project; an empty projectId
 * keeps project-scoped consumers disabled until their route is ready.
 */
export const useTasks = (projectId?: string): UseProjectTasksResult => {
  const endpoint = projectId
    ? `/api/tasks/stream/ws?project_id=${encodeURIComponent(projectId)}`
    : '/api/tasks/stream/ws';

  const initialData = useCallback((): TasksState => ({ tasks: {} }), []);

  const { data, isConnected, isInitialized, error } = useJsonPatchWsStream(
    endpoint,
    projectId !== '',
    initialData
  );

  const localTasksById = useMemo(() => data?.tasks ?? {}, [data?.tasks]);

  const { tasks, tasksById, tasksByStatus } = useMemo(() => {
    const merged: Record<string, TaskWithAttemptStatus> = { ...localTasksById };
    const byStatus: Record<TaskStatus, TaskWithAttemptStatus[]> = {
      todo: [],
      inprogress: [],
      inreview: [],
      done: [],
      cancelled: [],
    };

    Object.values(merged).forEach((task) => {
      byStatus[task.status]?.push(task);
    });

    const sorted = Object.values(merged).sort(
      (a, b) =>
        new Date(b.created_at as string).getTime() -
        new Date(a.created_at as string).getTime()
    );

    (Object.values(byStatus) as TaskWithAttemptStatus[][]).forEach((list) => {
      list.sort(
        (a, b) =>
          new Date(b.created_at as string).getTime() -
          new Date(a.created_at as string).getTime()
      );
    });

    return { tasks: sorted, tasksById: merged, tasksByStatus: byStatus };
  }, [localTasksById]);

  const isLoading = !isInitialized && !error; // until first snapshot

  return {
    tasks,
    tasksById,
    tasksByStatus,
    isLoading,
    isConnected,
    error,
  };
};

export const useProjectTasks = (projectId: string): UseProjectTasksResult =>
  useTasks(projectId);
