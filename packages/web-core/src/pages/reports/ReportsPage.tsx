import { Fragment, useCallback, useEffect, useMemo, useState } from 'react';
import {
  keepPreviousData,
  useMutation,
  useQuery,
  useQueryClient,
} from '@tanstack/react-query';
import {
  CaretDownIcon,
  CaretRightIcon,
  ChartBarIcon,
  CopySimpleIcon,
  SparkleIcon,
} from '@phosphor-icons/react';
import { PrimaryButton } from '@vibe/ui/components/PrimaryButton';
import type { ExecutorProfileId, WorkspaceActivity } from 'shared/types';
import { reportsApi } from '@/shared/lib/api';
import { writeClipboardViaBridge } from '@/shared/lib/clipboard';
import { cn } from '@/shared/lib/utils';
import { useUserSystem } from '@/shared/hooks/useUserSystem';
import { useAppNavigation } from '@/shared/hooks/useAppNavigation';
import ExecutorProfileSelector from '@/shared/components/settings/ExecutorProfileSelector';
import {
  RANGE_PRESETS,
  type DateRange,
  type RangePreset,
  formatDuration,
  isValidRange,
  MAX_REPORT_DAYS,
  presetRange,
  rangeToQuery,
  reportToMarkdown,
} from './reportUtils';

const DEFAULT_IDLE_CAP_MINUTES = 15;
const SUMMARY_PROFILE_STORAGE_KEY = 'vk.reports.summaryProfile';

const inputClassName =
  'px-half py-half bg-secondary rounded-sm border border-border text-base text-normal focus:outline-none focus:ring-1 focus:ring-brand';

function loadStoredProfile(): ExecutorProfileId | null {
  try {
    const raw = localStorage.getItem(SUMMARY_PROFILE_STORAGE_KEY);
    return raw ? (JSON.parse(raw) as ExecutorProfileId) : null;
  } catch {
    return null;
  }
}

function StatTile({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex-1 min-w-[120px] rounded-sm border border-border bg-secondary p-base">
      <p className="text-sm text-low">{label}</p>
      <p className="text-xl font-medium text-high mt-half">{value}</p>
    </div>
  );
}

function WorkspaceRow({
  workspace,
  expanded,
  onToggle,
  onOpen,
}: {
  workspace: WorkspaceActivity;
  expanded: boolean;
  onToggle: () => void;
  onOpen: () => void;
}) {
  return (
    <Fragment>
      <tr
        className="border-t border-border hover:bg-secondary cursor-pointer align-top"
        onClick={onToggle}
      >
        <td className="py-half pl-base pr-half w-6 text-low">
          {expanded ? (
            <CaretDownIcon size={12} weight="bold" />
          ) : (
            <CaretRightIcon size={12} weight="bold" />
          )}
        </td>
        <td className="py-half pr-base">
          <div className="flex items-center gap-half">
            <span className="text-base text-high">{workspace.name}</span>
            {workspace.merged_at && (
              <span className="text-xs uppercase tracking-wide text-success">
                merged
              </span>
            )}
            {workspace.archived && (
              <span className="text-xs uppercase tracking-wide text-low">
                archived
              </span>
            )}
          </div>
          {workspace.repos.length > 0 && (
            <p className="text-sm text-low">{workspace.repos.join(', ')}</p>
          )}
          {workspace.summary && (
            <p
              className={cn(
                'text-sm mt-half',
                workspace.summary.is_stale ? 'text-low' : 'text-normal'
              )}
            >
              {workspace.summary.text}
              {workspace.summary.is_stale && (
                <span className="ml-1 text-warning">(outdated)</span>
              )}
            </p>
          )}
        </td>
        <td className="py-half pr-base text-right tabular-nums text-normal">
          {formatDuration(workspace.user_seconds)}
        </td>
        <td className="py-half pr-base text-right tabular-nums text-low">
          {formatDuration(workspace.agent_seconds)}
        </td>
        <td className="py-half pr-base text-right tabular-nums text-low">
          {workspace.prompt_count}
        </td>
        <td className="py-half pr-base text-right text-sm text-low whitespace-nowrap">
          {workspace.last_activity_at
            ? new Date(workspace.last_activity_at).toLocaleString(undefined, {
                month: 'short',
                day: 'numeric',
                hour: '2-digit',
                minute: '2-digit',
              })
            : ''}
        </td>
      </tr>
      {expanded && (
        <tr className="bg-secondary/50">
          <td />
          <td colSpan={5} className="pb-base pr-base">
            <div className="flex flex-wrap gap-x-double gap-y-half text-sm text-low pt-half">
              <span>
                Branch:{' '}
                <span className="font-ibm-plex-mono text-normal">
                  {workspace.branch}
                </span>
              </span>
              {workspace.first_activity_at && (
                <span>
                  First activity:{' '}
                  {new Date(workspace.first_activity_at).toLocaleString()}
                </span>
              )}
              {workspace.merged_at && (
                <span>
                  Merged: {new Date(workspace.merged_at).toLocaleString()}
                </span>
              )}
              <button
                type="button"
                className="text-brand hover:underline cursor-pointer"
                onClick={(e) => {
                  e.stopPropagation();
                  onOpen();
                }}
              >
                Open workspace
              </button>
            </div>
          </td>
        </tr>
      )}
    </Fragment>
  );
}

export function ReportsPage() {
  const appNavigation = useAppNavigation();
  const queryClient = useQueryClient();
  const { profiles, config } = useUserSystem();

  const [preset, setPreset] = useState<RangePreset>('this-week');
  const [range, setRange] = useState<DateRange>(() => presetRange('this-week'));
  const [idleCap, setIdleCap] = useState(DEFAULT_IDLE_CAP_MINUTES);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [copied, setCopied] = useState(false);
  const [summaryProfile, setSummaryProfile] =
    useState<ExecutorProfileId | null>(loadStoredProfile);

  const effectiveProfile = summaryProfile ?? config?.executor_profile ?? null;

  // Cleared or reversed date inputs must not reach `rangeToQuery`.
  const query = useMemo(
    () =>
      isValidRange(range)
        ? { ...rangeToQuery(range), idle_cap_minutes: idleCap }
        : null,
    [range, idleCap]
  );

  const reportQuery = useQuery({
    queryKey: ['reports', 'activity', query],
    queryFn: () =>
      query
        ? reportsApi.getActivity(query)
        : Promise.reject(new Error('Invalid period')),
    placeholderData: keepPreviousData,
    enabled: query !== null,
  });
  const report = reportQuery.data;
  // While a new period loads, the previous report is still shown; exporting it
  // would label old numbers with the new dates.
  const canExport = !!report && !reportQuery.isPlaceholderData;

  const statusQuery = useQuery({
    queryKey: ['reports', 'summary-status'],
    queryFn: () => reportsApi.getSummaryStatus(),
    refetchInterval: (q) => (q.state.data?.running ? 2000 : false),
  });
  const job = statusQuery.data;

  // Refresh the report whenever a generation run finishes.
  const [wasRunning, setWasRunning] = useState(false);
  useEffect(() => {
    if (job?.running) {
      setWasRunning(true);
    } else if (wasRunning) {
      setWasRunning(false);
      void queryClient.invalidateQueries({
        queryKey: ['reports', 'activity'],
      });
    }
  }, [job?.running, wasRunning, queryClient]);

  const generateMutation = useMutation({
    mutationFn: (workspaceIds: string[]) => {
      if (!effectiveProfile) throw new Error('Select an agent first');
      return reportsApi.generateSummaries({
        workspace_ids: workspaceIds,
        executor_profile_id: effectiveProfile,
      });
    },
    onSuccess: (status) => {
      queryClient.setQueryData(['reports', 'summary-status'], status);
    },
  });

  const handlePresetChange = (value: RangePreset) => {
    setPreset(value);
    if (value !== 'custom') setRange(presetRange(value));
  };

  const handleProfileSelect = (profile: ExecutorProfileId) => {
    setSummaryProfile(profile);
    localStorage.setItem(SUMMARY_PROFILE_STORAGE_KEY, JSON.stringify(profile));
  };

  const toggleExpanded = useCallback((id: string) => {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }, []);

  const missingSummaryIds = useMemo(
    () =>
      (report?.workspaces ?? [])
        .filter((ws) => !ws.summary || ws.summary.is_stale)
        .map((ws) => ws.workspace_id),
    [report]
  );
  const allIds = useMemo(
    () => (report?.workspaces ?? []).map((ws) => ws.workspace_id),
    [report]
  );

  const promptCount = useMemo(
    () =>
      (report?.workspaces ?? []).reduce((sum, ws) => sum + ws.prompt_count, 0),
    [report]
  );

  const handleCopyMarkdown = async () => {
    if (!report || !canExport) return;
    await writeClipboardViaBridge(reportToMarkdown(report, range));
    setCopied(true);
    setTimeout(() => setCopied(false), 2000);
  };

  const isGenerating = !!job?.running || generateMutation.isPending;
  const generateError =
    generateMutation.error instanceof Error
      ? generateMutation.error.message
      : null;

  return (
    <div className="flex flex-col h-full overflow-hidden">
      <div className="flex flex-wrap items-center justify-between gap-base px-double py-base border-b border-border">
        <h1 className="text-xl font-medium text-high">Activity report</h1>
        <div className="flex flex-wrap items-center gap-half">
          <select
            value={preset}
            onChange={(e) => handlePresetChange(e.target.value as RangePreset)}
            className={inputClassName}
            aria-label="Period"
          >
            {RANGE_PRESETS.map((p) => (
              <option key={p.value} value={p.value}>
                {p.label}
              </option>
            ))}
          </select>
          <input
            type="date"
            value={range.start}
            max={range.end}
            onChange={(e) => {
              setPreset('custom');
              setRange((r) => ({ ...r, start: e.target.value }));
            }}
            className={inputClassName}
            aria-label="From"
          />
          <span className="text-low">–</span>
          <input
            type="date"
            value={range.end}
            min={range.start}
            onChange={(e) => {
              setPreset('custom');
              setRange((r) => ({ ...r, end: e.target.value }));
            }}
            className={inputClassName}
            aria-label="To"
          />
          <label
            className="flex items-center gap-half text-sm text-low ml-base"
            title="A gap longer than this between the agent finishing and your next action counts as time away"
          >
            Idle after
            <input
              type="number"
              min={1}
              max={240}
              value={idleCap}
              onChange={(e) => {
                const value = Number(e.target.value);
                if (value >= 1 && value <= 240) setIdleCap(value);
              }}
              className={cn(inputClassName, 'w-16')}
            />
            min
          </label>
          <PrimaryButton
            variant="tertiary"
            actionIcon={CopySimpleIcon}
            onClick={() => void handleCopyMarkdown()}
            disabled={!canExport}
            value={copied ? 'Copied' : 'Copy Markdown'}
          />
        </div>
      </div>

      <div className="flex-1 overflow-y-auto">
        <div className="mx-auto max-w-5xl p-double space-y-double">
          {reportQuery.error ? (
            <p className="text-error">
              Failed to load report:{' '}
              {reportQuery.error instanceof Error
                ? reportQuery.error.message
                : String(reportQuery.error)}
            </p>
          ) : null}

          {report && (
            <>
              <div className="flex flex-wrap gap-base">
                <StatTile
                  label="Your time (estimated)"
                  value={formatDuration(report.user_seconds)}
                />
                <StatTile
                  label="Agent time"
                  value={formatDuration(report.agent_seconds)}
                />
                <StatTile
                  label="Tasks"
                  value={String(report.workspaces.length)}
                />
                <StatTile label="Prompts" value={String(promptCount)} />
              </div>

              <p className="text-sm text-low">
                Your time is estimated from when you sent prompts, merged or
                started dev servers: each action counts the time since the agent
                last finished, up to {report.idle_cap_minutes} minutes. Time
                spent on several tasks at once is split between them.
              </p>
              <div className="rounded-sm border border-border bg-secondary p-base flex flex-wrap items-end gap-base">
                <div className="flex-1 min-w-[280px]">
                  <p className="text-sm font-medium text-normal mb-half">
                    Task summaries
                  </p>
                  <ExecutorProfileSelector
                    profiles={profiles}
                    selectedProfile={effectiveProfile}
                    onProfileSelect={handleProfileSelect}
                    disabled={isGenerating}
                    showLabel={false}
                  />
                </div>
                <div className="flex items-center gap-half">
                  <PrimaryButton
                    actionIcon={isGenerating ? 'spinner' : SparkleIcon}
                    onClick={() => generateMutation.mutate(missingSummaryIds)}
                    disabled={
                      isGenerating ||
                      !effectiveProfile ||
                      missingSummaryIds.length === 0
                    }
                    value={
                      missingSummaryIds.length > 0
                        ? `Summarize ${missingSummaryIds.length} task${missingSummaryIds.length === 1 ? '' : 's'}`
                        : 'All summarized'
                    }
                  />
                  <PrimaryButton
                    variant="tertiary"
                    onClick={() => generateMutation.mutate(allIds)}
                    disabled={
                      isGenerating || !effectiveProfile || allIds.length === 0
                    }
                    value="Regenerate all"
                  />
                </div>
                {(job?.running || (job && job.total > 0)) && (
                  <p className="w-full text-sm text-low">
                    {job.running
                      ? `Generating summaries… ${job.completed + job.failed} of ${job.total} done`
                      : `Last run: ${job.completed} summarized${job.failed > 0 ? `, ${job.failed} failed` : ''}`}
                    {job.last_error && (
                      <span className="text-error"> — {job.last_error}</span>
                    )}
                  </p>
                )}
                {generateError && (
                  <p className="w-full text-sm text-error">{generateError}</p>
                )}
              </div>

              {report.workspaces.length === 0 ? (
                <div className="flex flex-col items-center justify-center gap-2 py-double text-low">
                  <ChartBarIcon size={32} weight="light" />
                  <p className="text-base">No activity in this period</p>
                </div>
              ) : (
                <table className="w-full text-base">
                  <thead>
                    <tr className="text-sm text-low text-left">
                      <th />
                      <th className="py-half pr-base font-medium">Task</th>
                      <th className="py-half pr-base font-medium text-right whitespace-nowrap">
                        Your time
                      </th>
                      <th className="py-half pr-base font-medium text-right whitespace-nowrap">
                        Agent time
                      </th>
                      <th className="py-half pr-base font-medium text-right">
                        Prompts
                      </th>
                      <th className="py-half pr-base font-medium text-right whitespace-nowrap">
                        Last activity
                      </th>
                    </tr>
                  </thead>
                  <tbody>
                    {report.workspaces.map((ws) => (
                      <WorkspaceRow
                        key={ws.workspace_id}
                        workspace={ws}
                        expanded={expanded.has(ws.workspace_id)}
                        onToggle={() => toggleExpanded(ws.workspace_id)}
                        onOpen={() =>
                          appNavigation.goToWorkspace(ws.workspace_id)
                        }
                      />
                    ))}
                  </tbody>
                </table>
              )}
            </>
          )}

          {!query && (
            <p className="text-low">
              Choose a start and end date (up to {MAX_REPORT_DAYS} days).
            </p>
          )}

          {!report && reportQuery.isLoading && (
            <p className="text-low">Loading…</p>
          )}
        </div>
      </div>
    </div>
  );
}
