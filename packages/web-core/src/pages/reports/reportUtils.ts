import type { ActivityReport } from 'shared/types';

export type RangePreset =
  | 'today'
  | 'yesterday'
  | 'this-week'
  | 'last-week'
  | 'this-month'
  | 'last-month'
  | 'custom';

export const RANGE_PRESETS: { value: RangePreset; label: string }[] = [
  { value: 'today', label: 'Today' },
  { value: 'yesterday', label: 'Yesterday' },
  { value: 'this-week', label: 'This week' },
  { value: 'last-week', label: 'Last week' },
  { value: 'this-month', label: 'This month' },
  { value: 'last-month', label: 'Last month' },
  { value: 'custom', label: 'Custom' },
];

/** Inclusive range of local calendar dates, as `YYYY-MM-DD`. */
export interface DateRange {
  start: string;
  end: string;
}

export function toDateInput(date: Date): string {
  const y = date.getFullYear();
  const m = String(date.getMonth() + 1).padStart(2, '0');
  const d = String(date.getDate()).padStart(2, '0');
  return `${y}-${m}-${d}`;
}

function parseDateInput(value: string): Date {
  const [y, m, d] = value.split('-').map(Number);
  return new Date(y, m - 1, d);
}

function addDays(date: Date, days: number): Date {
  const next = new Date(date);
  next.setDate(next.getDate() + days);
  return next;
}

/** Weeks start on Monday. */
function startOfWeek(date: Date): Date {
  const day = (date.getDay() + 6) % 7;
  return addDays(date, -day);
}

export function presetRange(preset: RangePreset, now = new Date()): DateRange {
  const today = new Date(now.getFullYear(), now.getMonth(), now.getDate());
  switch (preset) {
    case 'yesterday': {
      const y = toDateInput(addDays(today, -1));
      return { start: y, end: y };
    }
    case 'this-week':
      return {
        start: toDateInput(startOfWeek(today)),
        end: toDateInput(today),
      };
    case 'last-week': {
      const start = addDays(startOfWeek(today), -7);
      return { start: toDateInput(start), end: toDateInput(addDays(start, 6)) };
    }
    case 'this-month':
      return {
        start: toDateInput(new Date(today.getFullYear(), today.getMonth(), 1)),
        end: toDateInput(today),
      };
    case 'last-month':
      return {
        start: toDateInput(
          new Date(today.getFullYear(), today.getMonth() - 1, 1)
        ),
        end: toDateInput(new Date(today.getFullYear(), today.getMonth(), 0)),
      };
    case 'today':
    case 'custom':
      return { start: toDateInput(today), end: toDateInput(today) };
  }
}

const DATE_INPUT_PATTERN = /^\d{4}-\d{2}-\d{2}$/;
export const MAX_REPORT_DAYS = 400;

function dayCount(range: DateRange): number {
  const start = parseDateInput(range.start);
  const end = parseDateInput(range.end);
  return Math.round((end.getTime() - start.getTime()) / 86_400_000) + 1;
}

/** Both dates are set (date inputs yield '' when cleared) and in order. */
export function isValidRange(range: DateRange): boolean {
  if (
    !DATE_INPUT_PATTERN.test(range.start) ||
    !DATE_INPUT_PATTERN.test(range.end) ||
    range.start > range.end
  ) {
    return false;
  }
  return dayCount(range) <= MAX_REPORT_DAYS;
}

/**
 * Converts an inclusive local date range to the API's half-open UTC period:
 * from the start of the first day to the start of the day after the last.
 * `new Date(y, m, d)` applies the browser's time zone rules, including days
 * whose midnight is skipped by a daylight-saving change.
 */
export function rangeToQuery(range: DateRange) {
  const [sy, sm, sd] = range.start.split('-').map(Number);
  const [ey, em, ed] = range.end.split('-').map(Number);
  return {
    from: new Date(sy, sm - 1, sd).toISOString(),
    to: new Date(ey, em - 1, ed + 1).toISOString(),
  };
}

export function formatDuration(seconds: number): string {
  if (seconds <= 0) return '0m';
  if (seconds < 60) return '<1m';
  const totalMinutes = Math.round(seconds / 60);
  const hours = Math.floor(totalMinutes / 60);
  const minutes = totalMinutes % 60;
  if (hours === 0) return `${minutes}m`;
  return minutes === 0 ? `${hours}h` : `${hours}h ${minutes}m`;
}

function escapeMarkdownCell(text: string): string {
  return text.replace(/\|/g, '\\|').replace(/\r?\n/g, ' ');
}

export function reportToMarkdown(report: ActivityReport, range: DateRange) {
  const title =
    range.start === range.end
      ? `Activity report: ${range.start}`
      : `Activity report: ${range.start} – ${range.end}`;
  const lines = [
    `# ${title}`,
    '',
    `- Your time (estimated): ${formatDuration(report.user_seconds)}`,
    `- Agent time: ${formatDuration(report.agent_seconds)}`,
    `- Tasks: ${report.workspaces.length}`,
    '',
    '| Task | Repos | Your time | Agent time | Prompts | Merged | Summary |',
    '| --- | --- | --- | --- | --- | --- | --- |',
  ];
  for (const ws of report.workspaces) {
    lines.push(
      `| ${[
        escapeMarkdownCell(ws.name),
        escapeMarkdownCell(ws.repos.join(', ')),
        formatDuration(ws.user_seconds),
        formatDuration(ws.agent_seconds),
        String(ws.prompt_count),
        ws.merged_at ? 'yes' : 'no',
        escapeMarkdownCell(ws.summary?.text ?? ''),
      ].join(' | ')} |`
    );
  }
  return lines.join('\n') + '\n';
}
