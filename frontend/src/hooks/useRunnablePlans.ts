import { useQuery } from '@tanstack/react-query';
import { plansApi } from '@/lib/api';

export const runnablePlansKeys = {
  all: ['runnablePlans'] as const,
  byProject: (projectId: string | undefined) =>
    ['runnablePlans', projectId] as const,
};

/**
 * Tech-solution plan docs marked 待运行 in the project's repos, excluding
 * plans already linked to a task. Surfaced in the kanban To Do column.
 */
export function useRunnablePlans(projectId?: string) {
  return useQuery({
    queryKey: runnablePlansKeys.byProject(projectId),
    queryFn: () => plansApi.listRunnable(projectId!),
    enabled: !!projectId,
    refetchOnWindowFocus: true,
  });
}
