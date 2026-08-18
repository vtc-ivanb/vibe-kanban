import { useMemo } from 'react';
import { useUiPreferencesStore } from '@/shared/stores/useUiPreferencesStore';

/**
 * Time an entry was written, shown above the entry when the preference is on.
 *
 * Entries recorded before timestamps were captured have no time, so nothing is
 * rendered rather than showing a misleading placeholder.
 */
export function ConversationEntryTimestamp({
  timestamp,
}: {
  timestamp: string | null;
}) {
  const showMessageTimestamps = useUiPreferencesStore(
    (state) => state.showMessageTimestamps
  );

  const parsed = useMemo(() => {
    if (!timestamp) return null;
    const date = new Date(timestamp);
    return Number.isNaN(date.getTime()) ? null : date;
  }, [timestamp]);

  if (!showMessageTimestamps || !parsed) {
    return null;
  }

  return (
    <div
      className="flex justify-end text-xs text-low font-ibm-plex-mono leading-none mb-half"
      title={parsed.toLocaleString()}
    >
      <time dateTime={parsed.toISOString()}>
        {parsed.toLocaleTimeString(undefined, {
          hour: '2-digit',
          minute: '2-digit',
        })}
      </time>
    </div>
  );
}
