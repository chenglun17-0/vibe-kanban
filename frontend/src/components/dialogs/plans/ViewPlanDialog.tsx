import NiceModal, { useModal } from '@ebay/nice-modal-react';
import { useQuery } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';
import WYSIWYGEditor from '@/components/ui/wysiwyg';
import { Loader } from '@/components/ui/loader';
import { plansApi } from '@/lib/api';
import { defineModal } from '@/lib/modals';
import { StartPlanDialog } from './StartPlanDialog';
import type { RunnablePlan } from 'shared/types';

export interface ViewPlanDialogProps {
  projectId: string;
  plan: RunnablePlan;
}

const ViewPlanDialogImpl = NiceModal.create<ViewPlanDialogProps>(
  ({ projectId, plan }) => {
    const modal = useModal();
    const { t } = useTranslation('tasks');

    const { data: content, isLoading } = useQuery({
      queryKey: ['planContent', projectId, plan.repo_id, plan.path],
      queryFn: () => plansApi.getContent(projectId, plan.repo_id, plan.path),
      enabled: modal.visible,
    });

    const handleOpenChange = (open: boolean) => {
      if (!open) modal.hide();
    };

    const handleStart = () => {
      modal.hide();
      StartPlanDialog.show({ projectId, plan });
    };

    return (
      <Dialog open={modal.visible} onOpenChange={handleOpenChange}>
        <DialogContent className="sm:max-w-[720px] max-h-[85vh] flex flex-col">
          <DialogHeader>
            <DialogTitle>{plan.title}</DialogTitle>
            <p className="text-xs text-muted-foreground break-all">
              {plan.repo_name}:{plan.path}
            </p>
          </DialogHeader>
          <div className="flex-1 min-h-0 overflow-y-auto py-2">
            {isLoading ? (
              <div className="flex justify-center py-8">
                <Loader />
              </div>
            ) : (
              <WYSIWYGEditor
                value={content ?? ''}
                disabled
                className="whitespace-pre-wrap break-words"
              />
            )}
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => modal.hide()}>
              {t('common:buttons.close')}
            </Button>
            <Button onClick={handleStart}>
              {t('runnablePlans.startExecution')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    );
  }
);

export const ViewPlanDialog = defineModal<ViewPlanDialogProps, void>(
  ViewPlanDialogImpl
);
