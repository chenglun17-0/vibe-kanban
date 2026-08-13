import { memo } from 'react';
import { useTranslation } from 'react-i18next';
import { FileText, Play } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { useRunnablePlans } from '@/hooks/useRunnablePlans';
import { ViewPlanDialog } from '@/components/dialogs/plans/ViewPlanDialog';
import { StartPlanDialog } from '@/components/dialogs/plans/StartPlanDialog';
import type { RunnablePlan } from 'shared/types';

interface RunnablePlansSectionProps {
  projectId?: string;
}

/**
 * Tech-solution plans marked 待运行, listed at the top of the kanban
 * To Do column. "开始" creates a task linked to the plan and starts it.
 */
function RunnablePlansSection({ projectId }: RunnablePlansSectionProps) {
  const { t } = useTranslation('tasks');
  const { data: plans = [] } = useRunnablePlans(projectId);

  if (plans.length === 0) return null;

  const handleView = (plan: RunnablePlan) => {
    ViewPlanDialog.show({ projectId: plan.project_id, plan });
  };

  const handleStart = (plan: RunnablePlan) => {
    StartPlanDialog.show({ projectId: plan.project_id, plan });
  };

  return (
    <div className="px-3 pb-2 space-y-2">
      <div className="text-xs font-medium text-muted-foreground">
        {t('runnablePlans.sectionTitle', { count: plans.length })}
      </div>
      {plans.map((plan) => (
        <div
          key={`${plan.repo_id}:${plan.path}`}
          className="rounded-md border bg-card p-2 space-y-2"
        >
          <div className="flex items-start gap-2">
            <FileText
              aria-hidden
              className="h-4 w-4 mt-0.5 shrink-0 text-muted-foreground"
            />
            <div className="min-w-0">
              <p className="text-sm font-medium leading-5 break-words">
                {plan.title}
              </p>
              {!projectId && (
                <p className="text-xs text-muted-foreground">
                  {plan.project_name}
                </p>
              )}
              <p className="text-xs text-muted-foreground break-all">
                {plan.path}
              </p>
            </div>
          </div>
          <div className="flex justify-end gap-2">
            <Button
              variant="outline"
              size="sm"
              onClick={() => handleView(plan)}
            >
              {t('runnablePlans.viewPlan')}
            </Button>
            <Button size="sm" onClick={() => handleStart(plan)}>
              <Play aria-hidden className="h-3.5 w-3.5 mr-1" />
              {t('runnablePlans.start')}
            </Button>
          </div>
        </div>
      ))}
    </div>
  );
}

export default memo(RunnablePlansSection);
