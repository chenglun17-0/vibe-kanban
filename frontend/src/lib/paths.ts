export const paths = {
  projects: () => '/projects',
  tasks: () => '/tasks',
  globalTask: (taskId: string) => `/tasks/${taskId}`,
  globalAttempt: (taskId: string, attemptId: string) =>
    `/tasks/${taskId}/attempts/${attemptId}`,
  projectTasks: (projectId: string) => `/projects/${projectId}/tasks`,
  task: (projectId: string, taskId: string) =>
    `/projects/${projectId}/tasks/${taskId}`,
  attempt: (projectId: string, taskId: string, attemptId: string) =>
    `/projects/${projectId}/tasks/${taskId}/attempts/${attemptId}`,
  attemptFull: (projectId: string, taskId: string, attemptId: string) =>
    `/projects/${projectId}/tasks/${taskId}/attempts/${attemptId}/full`,
};
