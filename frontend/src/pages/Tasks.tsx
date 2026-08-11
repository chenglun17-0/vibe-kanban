import { useCallback, useEffect, useMemo, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { AlertTriangle, FolderKanban, Plus } from 'lucide-react';
import { useTranslation } from 'react-i18next';

import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert';
import { Button } from '@/components/ui/button';
import { Card, CardContent } from '@/components/ui/card';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Loader } from '@/components/ui/loader';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectSeparator,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import TaskKanbanBoard, {
  type KanbanColumns,
} from '@/components/tasks/TaskKanbanBoard';
import type { DragEndEvent } from '@/components/ui/shadcn-io/kanban';
import { ProjectFormDialog } from '@/components/dialogs/projects/ProjectFormDialog';
import { useSearch } from '@/contexts/SearchContext';
import { useProjects } from '@/hooks/useProjects';
import { useTasks } from '@/hooks/useProjectTasks';
import { tasksApi } from '@/lib/api';
import { openTaskForm } from '@/lib/openTaskForm';
import { paths } from '@/lib/paths';
import type { TaskStatus, TaskWithAttemptStatus } from 'shared/types';

const ALL_PROJECTS = 'all';
const CREATE_PROJECT = 'create-project';
const TASK_STATUSES: TaskStatus[] = [
  'todo',
  'inprogress',
  'inreview',
  'done',
  'cancelled',
];

export function Tasks() {
  const { t } = useTranslation('tasks');
  const navigate = useNavigate();
  const { query } = useSearch();
  const {
    projects,
    projectsById,
    isLoading: projectsLoading,
    error: projectsError,
  } = useProjects();
  const { tasks, tasksById, isLoading, error: tasksError } = useTasks();
  const [projectFilter, setProjectFilter] = useState(ALL_PROJECTS);
  const [isProjectPickerOpen, setIsProjectPickerOpen] = useState(false);

  useEffect(() => {
    if (projectFilter !== ALL_PROJECTS && !projectsById[projectFilter]) {
      setProjectFilter(ALL_PROJECTS);
    }
  }, [projectFilter, projectsById]);

  const projectNames = useMemo(
    () =>
      Object.fromEntries(projects.map((project) => [project.id, project.name])),
    [projects]
  );

  const normalizedQuery = query.trim().toLowerCase();
  const kanbanColumns = useMemo(() => {
    const columns: KanbanColumns = {
      todo: [],
      inprogress: [],
      inreview: [],
      done: [],
      cancelled: [],
    };

    tasks.forEach((task) => {
      if (projectFilter !== ALL_PROJECTS && task.project_id !== projectFilter) {
        return;
      }

      if (normalizedQuery) {
        const matchesTitle = task.title.toLowerCase().includes(normalizedQuery);
        const matchesDescription = task.description
          ?.toLowerCase()
          .includes(normalizedQuery);
        const matchesProject = projectNames[task.project_id]
          ?.toLowerCase()
          .includes(normalizedQuery);
        if (!matchesTitle && !matchesDescription && !matchesProject) return;
      }

      columns[task.status].push(task);
    });

    TASK_STATUSES.forEach((status) => {
      columns[status].sort(
        (a, b) =>
          new Date(b.created_at).getTime() - new Date(a.created_at).getTime()
      );
    });

    return columns;
  }, [normalizedQuery, projectFilter, projectNames, tasks]);

  const visibleTaskCount = useMemo(
    () =>
      TASK_STATUSES.reduce(
        (count, status) => count + kanbanColumns[status].length,
        0
      ),
    [kanbanColumns]
  );

  const openCreateTask = useCallback(
    (projectId?: string) => {
      const targetProjectId =
        projectId ??
        (projectFilter !== ALL_PROJECTS ? projectFilter : undefined) ??
        (projects.length === 1 ? projects[0].id : undefined);

      if (targetProjectId) {
        openTaskForm({ mode: 'create', projectId: targetProjectId });
        return;
      }

      if (projects.length > 1) {
        setIsProjectPickerOpen(true);
      }
    },
    [projectFilter, projects]
  );

  const handleCreateProject = useCallback(async () => {
    try {
      await ProjectFormDialog.show({});
    } catch {
      // Closing the dialog does not require follow-up work.
    }
  }, []);

  const handleProjectFilterChange = useCallback(
    (value: string) => {
      if (value === CREATE_PROJECT) {
        void handleCreateProject();
        return;
      }
      setProjectFilter(value);
    },
    [handleCreateProject]
  );

  const handleSelectProjectForTask = useCallback((projectId: string) => {
    setIsProjectPickerOpen(false);
    openTaskForm({ mode: 'create', projectId });
  }, []);

  const handleViewTaskDetails = useCallback(
    (task: TaskWithAttemptStatus) => {
      navigate(paths.task(task.project_id, task.id));
    },
    [navigate]
  );

  const handleDragEnd = useCallback(
    async (event: DragEndEvent) => {
      const { active, over } = event;
      if (!over || !active.data.current) return;

      const task = tasksById[active.id as string];
      const newStatus = over.id as TaskStatus;
      if (!task || task.status === newStatus) return;

      try {
        await tasksApi.update(task.id, {
          title: task.title,
          description: task.description,
          status: newStatus,
          parent_workspace_id: task.parent_workspace_id,
          image_ids: null,
        });
      } catch (error) {
        console.error('Failed to update task status:', error);
      }
    },
    [tasksById]
  );

  const initialLoading = (isLoading || projectsLoading) && tasks.length === 0;
  const loadError = tasksError || projectsError?.message;
  const hasFilters = projectFilter !== ALL_PROJECTS || Boolean(normalizedQuery);

  if (initialLoading) {
    return <Loader message={t('loading')} size={32} className="py-8" />;
  }

  return (
    <div className="flex h-full min-h-0 flex-col bg-muted/20">
      <header className="relative shrink-0 overflow-hidden border-b bg-background px-4 py-4 sm:px-6">
        <div className="pointer-events-none absolute inset-y-0 right-0 w-1/3 bg-[radial-gradient(circle_at_top_right,hsl(var(--primary)/0.10),transparent_68%)]" />
        <div className="relative flex flex-col gap-4 lg:flex-row lg:items-center lg:justify-between">
          <div className="flex items-start gap-3">
            <div className="mt-0.5 flex h-10 w-10 shrink-0 items-center justify-center rounded-xl border bg-muted">
              <FolderKanban className="h-5 w-5" />
            </div>
            <div>
              <h1 className="text-2xl font-semibold tracking-tight">
                {t('overview.title')}
              </h1>
              <p className="text-sm text-muted-foreground">
                {t('overview.subtitle')}
              </p>
              <div className="mt-2 flex items-center gap-3 text-xs font-medium text-muted-foreground">
                <span>{t('overview.taskCount', { count: tasks.length })}</span>
                <span aria-hidden="true">/</span>
                <span>
                  {t('overview.projectCount', { count: projects.length })}
                </span>
              </div>
            </div>
          </div>

          <div className="flex flex-col gap-2 sm:flex-row sm:items-center">
            <Select
              value={projectFilter}
              onValueChange={handleProjectFilterChange}
            >
              <SelectTrigger
                className="w-full bg-background sm:w-52"
                aria-label={t('overview.filterProject')}
              >
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value={ALL_PROJECTS}>
                  {t('overview.allProjects')}
                </SelectItem>
                {projects.map((project) => (
                  <SelectItem key={project.id} value={project.id}>
                    {project.name}
                  </SelectItem>
                ))}
                <SelectSeparator />
                <SelectItem value={CREATE_PROJECT}>
                  <span className="flex items-center">
                    <Plus className="mr-2 h-4 w-4" />
                    {t('overview.createProject')}
                  </span>
                </SelectItem>
              </SelectContent>
            </Select>
            <Button
              onClick={() => openCreateTask()}
              disabled={projects.length === 0}
              className="shrink-0"
            >
              <Plus className="mr-2 h-4 w-4" />
              {t('overview.createTask')}
            </Button>
          </div>
        </div>
      </header>

      {loadError && (
        <Alert className="shrink-0 rounded-none border-x-0 border-t-0">
          <AlertTriangle className="h-4 w-4" />
          <AlertTitle>{t('overview.loadError')}</AlertTitle>
          <AlertDescription>{loadError}</AlertDescription>
        </Alert>
      )}

      <main className="min-h-0 flex-1">
        {projects.length === 0 ? (
          <div className="mx-auto flex h-full max-w-xl items-center px-4">
            <Card className="w-full border-dashed bg-background/80">
              <CardContent className="px-6 py-12 text-center">
                <div className="mx-auto flex h-12 w-12 items-center justify-center rounded-xl bg-muted">
                  <FolderKanban className="h-6 w-6" />
                </div>
                <h2 className="mt-4 text-lg font-semibold">
                  {t('overview.noProjectsTitle')}
                </h2>
                <p className="mt-2 text-sm text-muted-foreground">
                  {t('overview.noProjectsDescription')}
                </p>
                <Button className="mt-5" onClick={handleCreateProject}>
                  <Plus className="mr-2 h-4 w-4" />
                  {t('overview.createProject')}
                </Button>
              </CardContent>
            </Card>
          </div>
        ) : visibleTaskCount === 0 ? (
          <div className="mx-auto flex h-full max-w-xl items-center px-4">
            <Card className="w-full border-dashed bg-background/80">
              <CardContent className="px-6 py-10 text-center">
                <p className="text-sm text-muted-foreground">
                  {hasFilters
                    ? t('overview.noFilteredTasks')
                    : t('overview.noTasks')}
                </p>
                {!hasFilters && (
                  <Button className="mt-4" onClick={() => openCreateTask()}>
                    <Plus className="mr-2 h-4 w-4" />
                    {t('overview.createTask')}
                  </Button>
                )}
              </CardContent>
            </Card>
          </div>
        ) : (
          <div className="h-full w-full overflow-auto overscroll-contain">
            <TaskKanbanBoard
              columns={kanbanColumns}
              onDragEnd={handleDragEnd}
              onViewTaskDetails={handleViewTaskDetails}
              onCreateTask={() => openCreateTask()}
              projectNames={projectNames}
            />
          </div>
        )}
      </main>

      <Dialog open={isProjectPickerOpen} onOpenChange={setIsProjectPickerOpen}>
        <DialogHeader>
          <DialogTitle>{t('overview.chooseProjectTitle')}</DialogTitle>
          <DialogDescription>
            {t('overview.chooseProjectDescription')}
          </DialogDescription>
        </DialogHeader>
        <DialogContent className="gap-2">
          {projects.map((project) => (
            <Button
              key={project.id}
              variant="outline"
              className="h-auto justify-start px-4 py-3 text-left"
              onClick={() => handleSelectProjectForTask(project.id)}
            >
              <FolderKanban className="mr-3 h-4 w-4 shrink-0" />
              <span className="truncate">{project.name}</span>
            </Button>
          ))}
        </DialogContent>
      </Dialog>
    </div>
  );
}
