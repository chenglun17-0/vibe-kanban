import { FolderKanban, Plus } from 'lucide-react';
import { useTranslation } from 'react-i18next';

import { ProjectFormDialog } from '@/components/dialogs/projects/ProjectFormDialog';
import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Loader } from '@/components/ui/loader';
import { useProjects } from '@/hooks/useProjects';

interface TaskProjectPickerDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onSelectProject: (projectId: string) => void;
}

export function TaskProjectPickerDialog({
  open,
  onOpenChange,
  onSelectProject,
}: TaskProjectPickerDialogProps) {
  const { t } = useTranslation('tasks');
  const { projects, isLoading } = useProjects();

  const handleCreateProject = async () => {
    const result = await ProjectFormDialog.show({});
    if (result.status === 'saved') {
      onOpenChange(false);
      onSelectProject(result.project.id);
    }
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogHeader>
        <DialogTitle>{t('overview.chooseProjectTitle')}</DialogTitle>
        <DialogDescription>
          {t('overview.chooseProjectDescription')}
        </DialogDescription>
      </DialogHeader>
      <DialogContent className="gap-2">
        {isLoading ? (
          <Loader message={t('loading')} size={20} className="py-6" />
        ) : (
          projects.map((project) => (
            <Button
              key={project.id}
              variant="outline"
              className="h-auto justify-start px-4 py-3 text-left"
              onClick={() => {
                onOpenChange(false);
                onSelectProject(project.id);
              }}
            >
              <FolderKanban className="mr-3 h-4 w-4 shrink-0" />
              <span className="truncate">{project.name}</span>
            </Button>
          ))
        )}
        <Button
          variant="ghost"
          className="mt-1 justify-start"
          onClick={handleCreateProject}
        >
          <Plus className="mr-3 h-4 w-4" />
          {t('overview.createProject')}
        </Button>
      </DialogContent>
    </Dialog>
  );
}
