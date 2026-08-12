import { useMemo, useState } from 'react';
import NiceModal, { useModal } from '@ebay/nice-modal-react';
import { useTranslation } from 'react-i18next';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';
import RepoBranchSelector from '@/components/tasks/RepoBranchSelector';
import { ExecutorProfileSelector } from '@/components/settings';
import { useProjectRepos, useRepoBranchSelection } from '@/hooks';
import { useTaskMutations } from '@/hooks/useTaskMutations';
import { useUserSystem } from '@/components/ConfigProvider';
import { useKeySubmitTask, Scope } from '@/keyboard';
import { defineModal } from '@/lib/modals';
import type { ExecutorProfileId, RunnablePlan } from 'shared/types';

export interface StartPlanDialogProps {
  projectId: string;
  plan: RunnablePlan;
}

/** Task description prompt: the executing agent reads and follows the plan. */
function buildPlanTaskDescription(plan: RunnablePlan): string {
  return [
    `实施计划文档：${plan.path}`,
    '',
    '要求：先完整阅读该计划文档，按其中的 Phase 拆解执行；每个 Phase 的验收以计划中写明的「验收命令 / 预期证据」为准。',
    '开始实施时将计划文档的状态从「待运行」更新为「进行中」，并按 tech-solution 技能的生命周期要求持续维护文档状态。',
  ].join('\n');
}

const StartPlanDialogImpl = NiceModal.create<StartPlanDialogProps>(
  ({ projectId, plan }) => {
    const modal = useModal();
    const { t } = useTranslation('tasks');
    const { profiles, config } = useUserSystem();
    const { createAndStart } = useTaskMutations(projectId);

    const [userSelectedProfile, setUserSelectedProfile] =
      useState<ExecutorProfileId | null>(null);
    const effectiveProfile =
      userSelectedProfile ?? config?.executor_profile ?? null;

    // The plan doc lives in one repo; the attempt runs against that repo only.
    const { data: projectRepos = [], isLoading: isLoadingRepos } =
      useProjectRepos(projectId, { enabled: modal.visible });
    const planRepos = useMemo(
      () => projectRepos.filter((r) => r.id === plan.repo_id),
      [projectRepos, plan.repo_id]
    );

    const {
      configs: repoBranchConfigs,
      isLoading: isLoadingBranches,
      setRepoBranch,
      getWorkspaceRepoInputs,
    } = useRepoBranchSelection({
      repos: planRepos,
      enabled: modal.visible && planRepos.length > 0,
    });

    const allBranchesSelected = repoBranchConfigs.every(
      (c) => c.targetBranch !== null
    );
    const canStart = Boolean(
      effectiveProfile &&
        allBranchesSelected &&
        planRepos.length > 0 &&
        !createAndStart.isPending &&
        !isLoadingRepos &&
        !isLoadingBranches
    );

    const handleStart = async () => {
      if (!effectiveProfile || !allBranchesSelected || planRepos.length === 0)
        return;
      try {
        await createAndStart.mutateAsync({
          task: {
            project_id: projectId,
            title: plan.title,
            description: buildPlanTaskDescription(plan),
            status: null,
            parent_workspace_id: null,
            plan_path: `${plan.repo_id}:${plan.path}`,
            image_ids: null,
          },
          executor_profile_id: effectiveProfile,
          repos: getWorkspaceRepoInputs(),
        });
        modal.remove();
      } catch (err) {
        console.error('Failed to start plan execution:', err);
      }
    };

    const handleOpenChange = (open: boolean) => {
      if (!open) modal.hide();
    };

    useKeySubmitTask(handleStart, {
      enabled: modal.visible && canStart,
      scope: Scope.DIALOG,
      preventDefault: true,
    });

    return (
      <Dialog open={modal.visible} onOpenChange={handleOpenChange}>
        <DialogContent className="sm:max-w-[500px]">
          <DialogHeader>
            <DialogTitle>{t('runnablePlans.startDialog.title')}</DialogTitle>
            <DialogDescription>
              {t('runnablePlans.startDialog.description')}
            </DialogDescription>
          </DialogHeader>

          <div className="space-y-4 py-4">
            <div className="space-y-1">
              <p className="text-sm font-medium break-words">{plan.title}</p>
              <p className="text-xs text-muted-foreground break-all">
                {plan.repo_name}:{plan.path}
              </p>
            </div>

            {profiles && (
              <ExecutorProfileSelector
                profiles={profiles}
                selectedProfile={effectiveProfile}
                onProfileSelect={setUserSelectedProfile}
                showLabel={true}
              />
            )}

            <RepoBranchSelector
              configs={repoBranchConfigs}
              onBranchChange={setRepoBranch}
              isLoading={isLoadingBranches}
              className="space-y-2"
            />
          </div>

          <DialogFooter>
            <Button
              variant="outline"
              onClick={() => modal.hide()}
              disabled={createAndStart.isPending}
            >
              {t('common:buttons.cancel')}
            </Button>
            <Button onClick={handleStart} disabled={!canStart}>
              {createAndStart.isPending
                ? t('runnablePlans.startDialog.starting')
                : t('runnablePlans.startDialog.start')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    );
  }
);

export const StartPlanDialog = defineModal<StartPlanDialogProps, void>(
  StartPlanDialogImpl
);
