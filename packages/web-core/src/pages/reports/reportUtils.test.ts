import { describe, it, expect } from 'vitest';
import type { ActivityReport } from 'shared/types';
import {
  formatDuration,
  isValidRange,
  presetRange,
  rangeToQuery,
  reportToMarkdown,
} from './reportUtils';

// Thursday, 23 October 2026, local time.
const NOW = new Date(2026, 9, 23, 15, 30);

function reportFixture(): ActivityReport {
  return {
    from: '2026-10-20T00:00:00Z',
    to: '2026-10-21T00:00:00Z',
    idle_cap_minutes: 15,
    user_seconds: 3900,
    agent_seconds: 7200,
    workspaces: [
      {
        workspace_id: 'ws-1',
        name: 'Fix login | redirect',
        branch: 'vk/fix-login',
        archived: false,
        repos: ['web', 'api'],
        first_activity_at: '2026-10-20T09:00:00Z',
        last_activity_at: '2026-10-20T11:00:00Z',
        user_seconds: 3900,
        agent_seconds: 7200,
        prompt_count: 4,
        merged_at: null,
        summary: {
          text: 'Fixed the "remember me" redirect,\nadded tests.',
          generated_at: '2026-10-21T00:00:00Z',
          is_stale: false,
        },
      },
    ],
  };
}

describe('presetRange', () => {
  it('starts weeks on Monday', () => {
    expect(presetRange('this-week', NOW)).toEqual({
      start: '2026-10-19',
      end: '2026-10-23',
    });
    expect(presetRange('last-week', NOW)).toEqual({
      start: '2026-10-12',
      end: '2026-10-18',
    });
  });

  it('covers whole previous month', () => {
    expect(presetRange('last-month', NOW)).toEqual({
      start: '2026-09-01',
      end: '2026-09-30',
    });
  });
});

describe('rangeToQuery', () => {
  it('covers from the first local midnight to the one after the last day', () => {
    const q = rangeToQuery({ start: '2026-10-20', end: '2026-10-22' });
    expect(q.from).toBe(new Date(2026, 9, 20).toISOString());
    expect(q.to).toBe(new Date(2026, 9, 23).toISOString());
  });
});

describe('isValidRange', () => {
  it('rejects cleared, reversed and oversized ranges', () => {
    expect(isValidRange({ start: '2026-10-20', end: '2026-10-20' })).toBe(true);
    expect(isValidRange({ start: '', end: '2026-10-20' })).toBe(false);
    expect(isValidRange({ start: '2026-10-21', end: '2026-10-20' })).toBe(
      false
    );
    expect(isValidRange({ start: '2020-01-01', end: '2026-10-20' })).toBe(
      false
    );
  });
});

describe('formatDuration', () => {
  it('formats hours and minutes', () => {
    expect(formatDuration(0)).toBe('0m');
    expect(formatDuration(30)).toBe('<1m');
    expect(formatDuration(25 * 60)).toBe('25m');
    expect(formatDuration(3600)).toBe('1h');
    expect(formatDuration(3900)).toBe('1h 5m');
  });
});

describe('exports', () => {
  it('escapes markdown table cells', () => {
    const md = reportToMarkdown(reportFixture(), {
      start: '2026-10-20',
      end: '2026-10-20',
    });
    expect(md).toContain('Fix login \\| redirect');
    expect(md).toContain('| 1h 5m | 2h | 4 | no |');
    expect(md).not.toContain('redirect,\nadded');
  });
});
