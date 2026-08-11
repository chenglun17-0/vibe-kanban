import {
  createContext,
  useContext,
  ReactNode,
  useMemo,
  useEffect,
} from 'react';
import { useLocation } from 'react-router-dom';
import type { Project } from 'shared/types';
import { useProjects } from '@/hooks/useProjects';
import { useTask } from '@/hooks/useTask';

interface ProjectContextValue {
  projectId: string | undefined;
  project: Project | undefined;
  isLoading: boolean;
  error: Error | null;
  isError: boolean;
}

const ProjectContext = createContext<ProjectContextValue | null>(null);

interface ProjectProviderProps {
  children: ReactNode;
}

export function ProjectProvider({ children }: ProjectProviderProps) {
  const location = useLocation();

  const { routeProjectId, routeTaskId } = useMemo(() => {
    const projectMatch = location.pathname.match(/^\/projects\/([^/]+)/);
    const taskMatch = location.pathname.match(/^\/tasks\/([^/]+)/);
    return {
      routeProjectId: projectMatch?.[1],
      routeTaskId: taskMatch?.[1],
    };
  }, [location.pathname]);

  const {
    projectsById,
    isLoading: projectsLoading,
    error: projectsError,
  } = useProjects();
  const {
    data: routeTask,
    isLoading: taskLoading,
    error: taskError,
  } = useTask(routeTaskId);
  const projectId = routeProjectId ?? routeTask?.project_id;
  const project = projectId ? projectsById[projectId] : undefined;
  const isLoading = projectsLoading || (!!routeTaskId && taskLoading);
  const error = projectsError ?? taskError;

  const value = useMemo(
    () => ({
      projectId,
      project,
      isLoading,
      error,
      isError: !!error,
    }),
    [projectId, project, isLoading, error]
  );

  // Centralized page title management
  useEffect(() => {
    if (project) {
      document.title = `${project.name} | vibe-kanban`;
    } else {
      document.title = 'vibe-kanban';
    }
  }, [project]);

  return (
    <ProjectContext.Provider value={value}>{children}</ProjectContext.Provider>
  );
}

export function useProject(): ProjectContextValue {
  const context = useContext(ProjectContext);
  if (!context) {
    throw new Error('useProject must be used within a ProjectProvider');
  }
  return context;
}
