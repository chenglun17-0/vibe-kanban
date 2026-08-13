import { useCallback, useEffect, useRef, useState } from 'react';
import { KanbanCard } from '@/components/ui/shadcn-io/kanban';
import { Link, Loader2, XCircle } from 'lucide-react';
import type { TaskWithAttemptStatus } from 'shared/types';
import { ActionsDropdown } from '@/components/ui/actions-dropdown';
import { Button } from '@/components/ui/button';
import { useNavigateWithSearch } from '@/hooks';
import { paths } from '@/lib/paths';
import { attemptsApi } from '@/lib/api';
import { TaskCardHeader } from './TaskCardHeader';
import { useTranslation } from 'react-i18next';
import { useLocation } from 'react-router-dom';

type Task = TaskWithAttemptStatus;

interface TaskCardProps {
  task: Task;
  index: number;
  status: string;
  onViewDetails: (task: Task) => void;
  isOpen?: boolean;
  projectId: string;
  projectName?: string;
}

export function TaskCard({
  task,
  index,
  status,
  onViewDetails,
  isOpen,
  projectId,
  projectName,
}: TaskCardProps) {
  const { t } = useTranslation('tasks');
  const navigate = useNavigateWithSearch();
  const location = useLocation();
  const isGlobalTasksRoute = /^\/tasks(?:\/|$)/.test(location.pathname);
  const [isNavigatingToParent, setIsNavigatingToParent] = useState(false);

  const handleClick = useCallback(() => {
    onViewDetails(task);
  }, [task, onViewDetails]);

  const handleParentClick = useCallback(
    async (e: React.MouseEvent) => {
      e.stopPropagation();
      if (!task.parent_workspace_id || isNavigatingToParent) return;

      setIsNavigatingToParent(true);
      try {
        const parentAttempt = await attemptsApi.get(task.parent_workspace_id);
        navigate(
          isGlobalTasksRoute
            ? paths.globalAttempt(
                parentAttempt.task_id,
                task.parent_workspace_id
              )
            : paths.attempt(
                projectId,
                parentAttempt.task_id,
                task.parent_workspace_id
              )
        );
      } catch (error) {
        console.error('Failed to navigate to parent task attempt:', error);
        setIsNavigatingToParent(false);
      }
    },
    [
      task.parent_workspace_id,
      projectId,
      navigate,
      isNavigatingToParent,
      isGlobalTasksRoute,
    ]
  );

  const localRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!isOpen || !localRef.current) return;
    const el = localRef.current;
    requestAnimationFrame(() => {
      el.scrollIntoView({
        block: 'center',
        inline: 'nearest',
        behavior: 'smooth',
      });
    });
  }, [isOpen]);

  const cardActions = (
    <>
      {task.has_in_progress_attempt && (
        <Loader2 className="h-4 w-4 animate-spin text-blue-500" />
      )}
      {task.last_attempt_failed && (
        <XCircle className="h-4 w-4 text-destructive" />
      )}
      {task.parent_workspace_id && (
        <Button
          variant="icon"
          onClick={handleParentClick}
          onPointerDown={(e) => e.stopPropagation()}
          onMouseDown={(e) => e.stopPropagation()}
          disabled={isNavigatingToParent}
          title={t('navigateToParent')}
        >
          <Link className="h-4 w-4" />
        </Button>
      )}
      <ActionsDropdown task={task} projectId={projectId} />
    </>
  );

  return (
    <KanbanCard
      key={task.id}
      id={task.id}
      name={task.title}
      index={index}
      parent={status}
      onClick={handleClick}
      isOpen={isOpen}
      forwardedRef={localRef}
    >
      <div className="flex flex-col gap-2">
        {projectName && (
          <div className="flex min-w-0 items-center justify-between gap-2">
            <span className="truncate text-[11px] font-medium tracking-wide text-muted-foreground">
              #{projectName}
            </span>
            <div className="flex shrink-0 items-center gap-1">
              {cardActions}
            </div>
          </div>
        )}
        <TaskCardHeader
          title={task.title}
          right={projectName ? undefined : cardActions}
        />
        {task.description && (
          <p className="text-sm text-secondary-foreground break-words">
            {task.description.length > 130
              ? `${task.description.substring(0, 130)}...`
              : task.description}
          </p>
        )}
      </div>
    </KanbanCard>
  );
}
