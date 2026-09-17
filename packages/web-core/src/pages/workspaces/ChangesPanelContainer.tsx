import { memo, useEffect, useCallback, useRef, useState, useMemo } from 'react';
import { useTranslation } from 'react-i18next';
import {
  CaretDownIcon,
  CopyIcon,
  GithubLogoIcon,
  PlusIcon,
} from '@phosphor-icons/react';
import {
  CodeView,
  WorkerPoolContextProvider,
  type CodeViewHandle,
  type CodeViewReactOptions,
} from '@pierre/diffs/react';
import type {
  CodeViewDiffItem,
  CodeViewItem,
  DiffLineAnnotation,
  LineAnnotation,
  AnnotationSide,
  VirtualFileMetrics,
} from '@pierre/diffs';
const WorkerUrl = new URL(
  '@pierre/diffs/worker/worker-portable.js',
  import.meta.url
).href;
import { sortDiffs } from '@/shared/lib/fileTreeUtils';
import { useChangesView } from '@/shared/hooks/useChangesView';
import { useFileInViewStore } from '@/shared/stores/useFileInViewStore';
import {
  useDiffs,
  useShowGitHubComments,
  useGetGitHubCommentsForFile,
} from '@/shared/stores/useWorkspaceDiffStore';
import { useUiPreferencesStore } from '@/shared/stores/useUiPreferencesStore';
import {
  useDiffViewMode,
  useWrapTextDiff,
  useIgnoreWhitespaceDiff,
} from '@/shared/stores/useDiffViewStore';
import { useTheme } from '@/shared/hooks/useTheme';
import { getActualTheme } from '@/shared/lib/theme';
import { useReview, type ReviewDraft } from '@/shared/hooks/useReview';
import {
  transformDiffToFileDiffMetadata,
  transformCommentsToAnnotations,
  type CommentAnnotation,
} from '@/shared/lib/diffDataAdapter';
import { DiffSide } from '@/shared/types/diff';
import { isRealMobileDevice } from '@/shared/hooks/useIsMobile';
import { useOpenInEditor } from '@/shared/hooks/useOpenInEditor';
import { OpenInIdeButton } from '@/shared/components/OpenInIdeButton';
import { CopyButton } from '@/shared/components/CopyButton';
import { writeClipboardViaBridge } from '@/shared/lib/clipboard';
import { getFileIcon } from '@/shared/lib/fileTypeIcon';
import { stripLineEnding, splitLines } from '@/shared/lib/string';
import { ReviewCommentRenderer } from './ReviewCommentRenderer';
import { GitHubCommentRenderer } from './GitHubCommentRenderer';
import { CommentWidgetLine } from './CommentWidgetLine';
import type { Diff, DiffChangeKind } from 'shared/types';

function workerFactory() {
  return new Worker(WorkerUrl, { type: 'module' });
}

const POOL_OPTIONS = { workerFactory, poolSize: 3 };
const HIGHLIGHTER_OPTIONS = {
  theme: { dark: 'github-dark', light: 'github-light' } as const,
  langs: [] as string[],
};

const COLLAPSE_BY_CHANGE_TYPE: Record<DiffChangeKind, boolean> = {
  added: false,
  deleted: true,
  modified: false,
  renamed: true,
  copied: true,
  permissionChange: true,
};

const COLLAPSE_MAX_LINES = 800;

function shouldAutoCollapse(diff: Diff): boolean {
  const totalLines = (diff.additions ?? 0) + (diff.deletions ?? 0);
  if (diff.change === 'renamed') {
    return totalLines === 0 || totalLines > COLLAPSE_MAX_LINES;
  }
  if (COLLAPSE_BY_CHANGE_TYPE[diff.change]) return true;
  if (totalLines > COLLAPSE_MAX_LINES) return true;
  return false;
}

const IS_MOBILE = isRealMobileDevice();
const NOOP = () => {};

// Vertical rhythm of the file list. Replaces the flex gap/padding the old
// Virtualizer layout used, so CodeView can account for it while estimating.
const CODE_VIEW_LAYOUT = { paddingTop: 4, paddingBottom: 8, gap: 4 };

// How long a scroll-to-file request waits for its file to show up in the list.
const PENDING_SCROLL_TTL_MS = 10_000;

// Pierre estimates unrendered items from these (line 20px, header 44px, …).
// They rarely match our themed CSS, so we measure the real geometry once and
// hand the measurements back through `itemMetrics`.
const FALLBACK_METRICS: VirtualFileMetrics = {
  hunkLineCount: 1,
  lineHeight: 20,
  diffHeaderHeight: 44,
  hunkSeparatorHeight: 32,
  spacing: 8,
};

// Cached for the session so later mounts estimate correctly from the first
// frame. Geometry only depends on font/theme CSS, which is stable per session.
let cachedMetrics: VirtualFileMetrics | null = null;

// Read the real rendered line/header/separator heights from a live diff so the
// estimates match the DOM. Returns null until at least one expanded diff has
// rendered a measurable line.
function measureDiffMetrics(
  scrollRoot: HTMLElement
): VirtualFileMetrics | null {
  let diffHeaderHeight = 0;
  let lineHeight = 0;
  let hunkSeparatorHeight = 0;

  for (const container of Array.from(
    scrollRoot.querySelectorAll('diffs-container')
  )) {
    const shadow = container.shadowRoot;
    if (!shadow) continue;

    if (diffHeaderHeight <= 0) {
      const header = shadow.querySelector('[data-diffs-header]');
      if (header instanceof HTMLElement) {
        diffHeaderHeight = header.getBoundingClientRect().height;
      }
    }
    if (lineHeight <= 0) {
      const line = shadow.querySelector('[data-line][data-line-index]');
      if (line instanceof HTMLElement) {
        lineHeight = line.getBoundingClientRect().height;
      }
    }
    if (hunkSeparatorHeight <= 0) {
      const separator = shadow.querySelector('[data-separator]');
      if (separator instanceof HTMLElement) {
        hunkSeparatorHeight = separator.getBoundingClientRect().height;
      }
    }
    if (diffHeaderHeight > 0 && lineHeight > 0) break;
  }

  // A code line is the one geometry we can't fall back on — without it the
  // estimate is meaningless, so wait for a rendered line before committing.
  if (diffHeaderHeight <= 0 || lineHeight <= 0) return null;

  return {
    hunkLineCount: FALLBACK_METRICS.hunkLineCount,
    lineHeight,
    diffHeaderHeight,
    hunkSeparatorHeight:
      hunkSeparatorHeight > 0
        ? hunkSeparatorHeight
        : FALLBACK_METRICS.hunkSeparatorHeight,
    spacing: FALLBACK_METRICS.spacing,
  };
}

const PIERRE_DIFFS_THEME_CSS = `
  [data-diffs-header] {
    background-color: hsl(var(--bg-primary));
    min-height: 40px;
    cursor: pointer;
    padding-inline: 12px;
    border-radius: 4px 4px 0 0;
    font-family: 'IBM Plex Mono', monospace;
    font-size: 0.875rem;
    line-height: 1.25rem;
  }

  [data-diffs-header] [data-additions-count],
  [data-diffs-header] [data-deletions-count] {
    display: none;
  }

  [data-diffs-header] [data-change-icon] {
    display: none;
  }

  [data-diffs-header] [data-metadata] {
    font-family: inherit;
    font-size: 0.75rem;
    gap: 8px;
  }

  [data-separator="line-info"][data-separator-first] {
    margin-top: 4px;
  }
  [data-separator="line-info"][data-separator-last] {
    margin-bottom: 4px;
  }

  [data-code] {
    border-radius: 0 0 4px 4px;
    padding-bottom: 0;
  }
  [data-code]::-webkit-scrollbar {
    height: 8px;
    background: transparent;
  }
  [data-code]::-webkit-scrollbar-track {
    background: transparent;
  }
  [data-code]::-webkit-scrollbar-thumb {
    background-color: transparent;
    border-radius: 4px;
  }
  [data-code]:hover::-webkit-scrollbar-thumb {
    background-color: hsl(var(--text-low) / 0.3);
  }
`;

type ExtendedCommentAnnotation =
  | CommentAnnotation
  | { type: 'draft'; draft: ReviewDraft; widgetKey: string };

type ChangesAnnotation = DiffLineAnnotation<ExtendedCommentAnnotation>;
type ChangesItem = CodeViewDiffItem<ExtendedCommentAnnotation>;

function mapSideToAnnotationSide(side: DiffSide): AnnotationSide {
  return side === DiffSide.Old ? 'deletions' : 'additions';
}

function mapAnnotationSideToSplitSide(side: AnnotationSide): DiffSide {
  return side === 'deletions' ? DiffSide.Old : DiffSide.New;
}

function getLineContent(
  content: string | null,
  lineNumber: number
): string | undefined {
  if (!content) return undefined;
  const lines = splitLines(content);
  const index = lineNumber - 1;
  if (index < 0 || index >= lines.length) return undefined;
  return stripLineEnding(lines[index]);
}

function getCodeLineForComment(
  diff: Diff,
  lineNumber: number,
  side: DiffSide
): string | undefined {
  const content = side === DiffSide.Old ? diff.oldContent : diff.newContent;
  return getLineContent(content, lineNumber);
}

// Parsed diffs are cached per path for as long as the file is in the changes
// list — a fixed-size LRU would thrash on large diffs (every recompute evicts
// the entry the next file is about to need) and re-parse every file.
const fileDiffCache = new Map<
  string,
  {
    diff: Diff;
    ignoreWhitespace: boolean;
    result: ReturnType<typeof transformDiffToFileDiffMetadata>;
  }
>();

function getCachedFileDiffMetadata(diff: Diff, ignoreWhitespace: boolean) {
  const path = diff.newPath || diff.oldPath || '';
  const cached = fileDiffCache.get(path);
  if (
    cached &&
    cached.diff === diff &&
    cached.ignoreWhitespace === ignoreWhitespace
  ) {
    return cached.result;
  }
  const result = transformDiffToFileDiffMetadata(diff, { ignoreWhitespace });
  fileDiffCache.set(path, { diff, ignoreWhitespace, result });
  return result;
}

function getDiffPath(diff: Diff): string {
  return diff.newPath || diff.oldPath || '';
}

function expandKeyFor(path: string): string {
  return `diff:${path}`;
}

// CodeView reconciles a controlled item only when its `version` changes, so a
// version has to be published whenever the item's rendered inputs change. The
// signature therefore covers comment bodies too: editing a comment keeps its
// id, and without the text here the edit would never reach the rendered card.
// Drafts are keyed by widget only — the widget owns its text as local state,
// so including it would re-version the file on every keystroke.
function annotationSignature(annotations: ChangesAnnotation[]): string {
  return JSON.stringify(
    annotations.map((annotation) => {
      const { metadata } = annotation;
      switch (metadata.type) {
        case 'draft':
          return [
            annotation.side,
            annotation.lineNumber,
            'draft',
            metadata.widgetKey,
          ];
        case 'github':
          return [
            annotation.side,
            annotation.lineNumber,
            'github',
            metadata.comment.id,
            metadata.comment.body,
          ];
        default:
          return [
            annotation.side,
            annotation.lineNumber,
            'review',
            metadata.comment.id,
            metadata.comment.text,
          ];
      }
    })
  );
}

interface ChangesPanelContainerProps {
  className: string;
  workspaceId: string;
}

export const ChangesPanelContainer = memo(function ChangesPanelContainer({
  className,
  workspaceId,
}: ChangesPanelContainerProps) {
  const { t } = useTranslation('common');
  const diffs = useDiffs();
  const { registerScrollToFile } = useChangesView();
  const [metrics, setMetrics] = useState<VirtualFileMetrics | null>(
    cachedMetrics
  );
  const scrollRootRef = useRef<HTMLDivElement | null>(null);

  const { theme } = useTheme();
  const actualTheme = getActualTheme(theme);
  const globalMode = useDiffViewMode();
  const wrapText = useWrapTextDiff();
  const ignoreWhitespace = useIgnoreWhitespaceDiff();

  const { comments, drafts, setDraft, addComment } = useReview();
  const draftsRef = useRef(drafts);
  draftsRef.current = drafts;

  const showGitHubComments = useShowGitHubComments();
  const getGitHubCommentsForFile = useGetGitHubCommentsForFile();
  const expandedPrefs = useUiPreferencesStore((s) => s.expanded);

  const openInEditor = useOpenInEditor(workspaceId);

  const codeViewRef = useRef<CodeViewHandle<
    ExtendedCommentAnnotation,
    undefined
  > | null>(null);

  const sortedDiffs = useMemo(() => sortDiffs(diffs), [diffs]);

  // Files auto-collapse the first time we see them; after that the user's
  // expand/collapse choice (persisted in ui preferences) wins. The decision is
  // kept per path for the lifetime of the panel so a later diff update (more
  // lines streamed in) can't silently re-collapse a file.
  const defaultCollapsedRef = useRef(new Map<string, boolean>());
  const defaultCollapsed = useMemo(() => {
    const map = defaultCollapsedRef.current;
    for (const diff of sortedDiffs) {
      const path = getDiffPath(diff);
      if (!map.has(path)) map.set(path, shouldAutoCollapse(diff));
    }
    return map;
  }, [sortedDiffs]);

  const diffByPath = useMemo(() => {
    const map = new Map<string, Diff>();
    for (const diff of sortedDiffs) map.set(getDiffPath(diff), diff);
    return map;
  }, [sortedDiffs]);
  const diffByPathRef = useRef(diffByPath);
  diffByPathRef.current = diffByPath;

  const commentsByPath = useMemo(() => {
    const map = new Map<string, typeof comments>();
    for (const comment of comments) {
      const list = map.get(comment.filePath);
      if (list) list.push(comment);
      else map.set(comment.filePath, [comment]);
    }
    return map;
  }, [comments]);

  // Keep annotation arrays referentially stable while their contents are
  // unchanged, so unrelated review activity doesn't re-version every item.
  const annotationCacheRef = useRef(
    new Map<string, { signature: string; value: ChangesAnnotation[] }>()
  );

  const annotationsByPath = useMemo(() => {
    const cache = annotationCacheRef.current;
    const next = new Map<string, ChangesAnnotation[]>();

    const draftsByPath = new Map<string, ChangesAnnotation[]>();
    Object.entries(drafts).forEach(([widgetKey, draft]) => {
      if (!draft) return;
      const list = draftsByPath.get(draft.filePath) ?? [];
      list.push({
        side: mapSideToAnnotationSide(draft.side),
        lineNumber: draft.lineNumber,
        metadata: { type: 'draft', draft, widgetKey },
      });
      draftsByPath.set(draft.filePath, list);
    });

    const paths = new Set<string>([
      ...commentsByPath.keys(),
      ...draftsByPath.keys(),
    ]);
    if (showGitHubComments) {
      for (const path of diffByPath.keys()) paths.add(path);
    }

    for (const path of paths) {
      const base = transformCommentsToAnnotations(
        commentsByPath.get(path) ?? [],
        showGitHubComments ? getGitHubCommentsForFile(path) : [],
        path
      ) as ChangesAnnotation[];
      const value = [...base, ...(draftsByPath.get(path) ?? [])];
      if (value.length === 0) continue;

      const signature = annotationSignature(value);
      const cached = cache.get(path);
      if (cached && cached.signature === signature) {
        next.set(path, cached.value);
        continue;
      }
      cache.set(path, { signature, value });
      next.set(path, value);
    }

    for (const path of [...cache.keys()]) {
      if (!next.has(path)) cache.delete(path);
    }

    return next;
  }, [
    commentsByPath,
    drafts,
    showGitHubComments,
    getGitHubCommentsForFile,
    diffByPath,
  ]);

  const itemVersionsRef = useRef(
    new Map<
      string,
      {
        fileDiff: unknown;
        annotations: unknown;
        collapsed: boolean;
        version: number;
      }
    >()
  );

  const items = useMemo(() => {
    const versions = itemVersionsRef.current;
    const next: ChangesItem[] = sortedDiffs.map((diff) => {
      const path = getDiffPath(diff);
      const fileDiff = getCachedFileDiffMetadata(diff, ignoreWhitespace);
      const annotations = annotationsByPath.get(path);
      const collapsed =
        expandedPrefs[expandKeyFor(path)] === undefined
          ? (defaultCollapsed.get(path) ?? false)
          : !expandedPrefs[expandKeyFor(path)];

      const previous = versions.get(path);
      const changed =
        previous === undefined ||
        previous.fileDiff !== fileDiff ||
        previous.annotations !== annotations ||
        previous.collapsed !== collapsed;
      const version = changed ? (previous?.version ?? 0) + 1 : previous.version;
      if (changed) {
        versions.set(path, { fileDiff, annotations, collapsed, version });
      }

      return {
        id: path,
        type: 'diff' as const,
        fileDiff,
        annotations,
        collapsed,
        version,
      };
    });

    return next;
  }, [
    sortedDiffs,
    ignoreWhitespace,
    annotationsByPath,
    expandedPrefs,
    defaultCollapsed,
  ]);

  const orderedPathsRef = useRef<string[]>([]);

  // Pruning and ref publishing happen on commit, never during render: an
  // abandoned render must not drop a version entry, or the path could come
  // back at a version CodeView has already seen and the update would be
  // rejected as a no-op.
  useEffect(() => {
    orderedPathsRef.current = items.map((item) => item.id);
    const live = new Set(orderedPathsRef.current);
    for (const path of [...itemVersionsRef.current.keys()]) {
      if (!live.has(path)) itemVersionsRef.current.delete(path);
    }
    for (const path of [...fileDiffCache.keys()]) {
      if (!live.has(path)) fileDiffCache.delete(path);
    }
  }, [items]);

  const handleLineClick = useCallback(
    (filePath: string, lineNumber: number, annotationSide: AnnotationSide) => {
      const diff = diffByPathRef.current.get(filePath);
      if (!diff) return;
      const splitSide = mapAnnotationSideToSplitSide(annotationSide);
      const widgetKey = `${filePath}-${splitSide}-${lineNumber}`;
      if (draftsRef.current[widgetKey]) return;

      const codeLine = getCodeLineForComment(diff, lineNumber, splitSide);
      setDraft(widgetKey, {
        filePath,
        side: splitSide,
        lineNumber,
        text: '',
        ...(codeLine !== undefined ? { codeLine } : {}),
      });
    },
    [setDraft]
  );

  const handleToggle = useCallback((path: string) => {
    const key = expandKeyFor(path);
    useUiPreferencesStore
      .getState()
      .toggleExpanded(key, !(defaultCollapsedRef.current.get(path) ?? false));
  }, []);

  const handleCopyFilePath = useCallback((path: string) => {
    void writeClipboardViaBridge(path);
  }, []);

  const handleOpenInIde = useCallback(
    (filePath: string) => {
      openInEditor({ filePath });
    },
    [openInEditor]
  );

  const renderHeaderPrefix = useCallback(
    (item: CodeViewItem<ExtendedCommentAnnotation>) => {
      const FileIcon = getFileIcon(item.id, actualTheme);
      return <FileIcon className="size-icon-base shrink-0" />;
    },
    [actualTheme]
  );

  const renderHeaderMetadata = useCallback(
    (item: CodeViewItem<ExtendedCommentAnnotation>) => {
      const path = item.id;
      const diff = diffByPathRef.current.get(path);
      const additions = diff?.additions ?? 0;
      const deletions = diff?.deletions ?? 0;
      const githubCommentCount = showGitHubComments
        ? getGitHubCommentsForFile(path).length
        : 0;
      const collapsed = item.collapsed ?? false;

      return (
        <div
          className="flex items-center gap-2 shrink-0 text-xs"
          onClick={(e) => e.stopPropagation()}
        >
          <CopyButton
            onCopy={() => handleCopyFilePath(path)}
            disabled={false}
            iconSize="size-icon-xs"
            icon={CopyIcon}
          />
          {(additions > 0 || deletions > 0) && (
            <span className="inline-flex items-center gap-1 font-mono">
              {additions > 0 && (
                <span className="text-success">+{additions}</span>
              )}
              {deletions > 0 && (
                <span className="text-error">-{deletions}</span>
              )}
            </span>
          )}
          {githubCommentCount > 0 && (
            <span className="inline-flex items-center gap-0.5 text-low">
              <GithubLogoIcon className="size-icon-xs" weight="fill" />
              {githubCommentCount}
            </span>
          )}
          {!IS_MOBILE && (
            <OpenInIdeButton
              onClick={() => handleOpenInIde(path)}
              className="size-icon-xs p-0"
            />
          )}
          <CaretDownIcon
            className={`size-icon-xs text-low transition-transform cursor-pointer${collapsed ? ' -rotate-90' : ''}`}
            onClick={() => handleToggle(path)}
          />
        </div>
      );
    },
    [
      showGitHubComments,
      getGitHubCommentsForFile,
      handleCopyFilePath,
      handleOpenInIde,
      handleToggle,
    ]
  );

  const renderAnnotation = useCallback(
    (
      annotation: ChangesAnnotation | LineAnnotation<ExtendedCommentAnnotation>,
      item: CodeViewItem<ExtendedCommentAnnotation>
    ) => {
      const { metadata } = annotation;

      if (metadata.type === 'draft') {
        return (
          <CommentWidgetLine
            draft={metadata.draft}
            widgetKey={metadata.widgetKey}
            onSave={NOOP}
            onCancel={NOOP}
          />
        );
      }

      if (metadata.type === 'github') {
        const githubComment = metadata.comment;
        return (
          <GitHubCommentRenderer
            comment={githubComment}
            theme={actualTheme}
            onCopyToUserComment={() => {
              const diff = diffByPathRef.current.get(item.id);
              const codeLine = diff
                ? getCodeLineForComment(
                    diff,
                    githubComment.lineNumber,
                    githubComment.side
                  )
                : undefined;
              addComment({
                filePath: item.id,
                lineNumber: githubComment.lineNumber,
                side: githubComment.side,
                text: githubComment.body,
                ...(codeLine !== undefined ? { codeLine } : {}),
              });
            }}
          />
        );
      }

      return <ReviewCommentRenderer comment={metadata.comment} />;
    },
    [actualTheme, addComment]
  );

  const renderGutterUtility = useCallback(
    (
      getHoveredLine: () =>
        | { lineNumber: number; side?: AnnotationSide }
        | undefined,
      item: CodeViewItem<ExtendedCommentAnnotation>
    ) => (
      <button
        className="flex items-center justify-center size-icon-base rounded text-brand bg-brand/20 transition-transform hover:scale-110"
        onClick={() => {
          const line = getHoveredLine();
          if (!line) return;
          handleLineClick(item.id, line.lineNumber, line.side ?? 'additions');
        }}
        title={t('comments.addReviewComment')}
      >
        <PlusIcon className="size-3.5" weight="bold" />
      </button>
    ),
    [handleLineClick, t]
  );

  const options = useMemo<
    CodeViewReactOptions<ExtendedCommentAnnotation, undefined>
  >(
    () => ({
      diffStyle:
        globalMode === 'split' ? ('split' as const) : ('unified' as const),
      diffIndicators: 'classic' as const,
      themeType: actualTheme,
      overflow: wrapText ? ('wrap' as const) : ('scroll' as const),
      hunkSeparators: 'line-info' as const,
      stickyHeaders: true,
      layout: CODE_VIEW_LAYOUT,
      enableGutterUtility: true,
      theme: { dark: 'github-dark', light: 'github-light' } as const,
      unsafeCSS: PIERRE_DIFFS_THEME_CSS,
      ...(metrics ? { itemMetrics: metrics } : {}),
      onLineClick: (
        props: { lineNumber: number; annotationSide?: AnnotationSide },
        context: { item: { id: string } }
      ) => {
        handleLineClick(
          context.item.id,
          props.lineNumber,
          props.annotationSide ?? 'additions'
        );
      },
    }),
    [globalMode, actualTheme, wrapText, metrics, handleLineClick]
  );

  const hasItems = items.length > 0;

  // Measure real line/header geometry once a diff has rendered, then feed it
  // back as `itemMetrics`. Pierre estimates unrendered items from built-in
  // sizes; when those disagree with our themed CSS the scrollbar jitters as
  // estimates get replaced by measurements.
  useEffect(() => {
    if (metrics !== null || !hasItems) return;
    let raf = 0;
    let attempts = 0;
    let settled = false;

    const tryMeasure = () => {
      if (settled) return;
      const scrollRoot = scrollRootRef.current;
      const measured =
        scrollRoot instanceof HTMLElement
          ? measureDiffMetrics(scrollRoot)
          : null;
      if (measured) {
        settled = true;
        cachedMetrics = measured;
        setMetrics(measured);
        return;
      }
      // Lines render asynchronously (worker highlighting); retry for a short
      // window before giving up and leaving the built-in estimates in place.
      if (++attempts < 30) {
        raf = requestAnimationFrame(tryMeasure);
      }
    };

    const startMeasuring = () => {
      if (settled) return;
      cancelAnimationFrame(raf);
      attempts = 0;
      raf = requestAnimationFrame(tryMeasure);
    };

    startMeasuring();

    // If every initially-rendered file is collapsed (deleted/renamed or large
    // diffs) there is no code line to measure and the retry window expires on
    // defaults. Expanding a file only updates the store, so re-measure then.
    const unsubscribe = useUiPreferencesStore.subscribe((state, prev) => {
      if (settled || state.expanded === prev.expanded) return;
      startMeasuring();
    });

    return () => {
      settled = true;
      cancelAnimationFrame(raf);
      unsubscribe();
    };
  }, [metrics, hasItems]);

  // Track the file at the top of the viewport for the file tree's selection.
  const handleScroll = useCallback(
    (
      scrollTop: number,
      viewer: { getTopForItem(id: string): number | undefined }
    ) => {
      const paths = orderedPathsRef.current;
      if (paths.length === 0) return;

      // Binary search for the last item starting at or above the viewport top.
      const probe = scrollTop + 1;
      let low = 0;
      let high = paths.length - 1;
      let found = paths[0];
      while (low <= high) {
        const mid = (low + high) >> 1;
        const top = viewer.getTopForItem(paths[mid]);
        if (top === undefined) break;
        if (top <= probe) {
          found = paths[mid];
          low = mid + 1;
        } else {
          high = mid - 1;
        }
      }
      useFileInViewStore.getState().setFileInView(found);
    },
    []
  );

  // CodeView only reports the file in view once the user scrolls, so seed it
  // with the first file when the list arrives — the file tree highlights it
  // from the moment the panel opens, as it did under the old observer.
  const seededFileInViewRef = useRef(false);
  useEffect(() => {
    if (seededFileInViewRef.current || items.length === 0) return;
    seededFileInViewRef.current = true;
    useFileInViewStore.getState().setFileInView(items[0].id);
  }, [items]);

  // Scroll-to-file has to run after the item list reflects an expand, so the
  // request is queued and flushed once the new items are committed.
  const pendingScrollRef = useRef<{
    path: string;
    lineNumber?: number;
    requestedAt: number;
  } | null>(null);
  const [scrollRequestId, setScrollRequestId] = useState(0);

  const handleScrollToFile = useCallback(
    (path: string, lineNumber?: number) => {
      const key = expandKeyFor(path);
      const prefs = useUiPreferencesStore.getState();
      const isCollapsed =
        prefs.expanded[key] === undefined
          ? (defaultCollapsedRef.current.get(path) ?? false)
          : !prefs.expanded[key];
      if (isCollapsed) prefs.setExpanded(key, true);
      useFileInViewStore.getState().setFileInView(path);
      pendingScrollRef.current = { path, lineNumber, requestedAt: Date.now() };
      setScrollRequestId((id) => id + 1);
    },
    []
  );

  useEffect(() => {
    const pending = pendingScrollRef.current;
    if (!pending) return;
    // A request outlives a render or two on purpose: the file may still be
    // streaming in, or an expand may not have committed yet. It does not
    // outlive the user's interest in it.
    if (Date.now() - pending.requestedAt > PENDING_SCROLL_TTL_MS) {
      pendingScrollRef.current = null;
      return;
    }
    const handle = codeViewRef.current;
    if (!handle) return;
    const item = handle.getItem(pending.path);
    if (!item) return;
    // A line inside a collapsed file resolves to the header, which would burn
    // the request on a half-done scroll; wait for the expand to land.
    if (pending.lineNumber != null && item.collapsed) return;

    pendingScrollRef.current = null;
    // Always land on the file first: CodeView ignores a line target it cannot
    // resolve (a deleted-file line requested on the additions side, a line
    // outside the rendered hunks), and without this the panel would not move
    // at all for those requests.
    handle.scrollTo({ type: 'item', id: pending.path, align: 'start' });
    if (pending.lineNumber != null) {
      const change = diffByPathRef.current.get(pending.path)?.change;
      handle.scrollTo({
        type: 'line',
        id: pending.path,
        lineNumber: pending.lineNumber,
        ...(change === 'deleted' ? { side: 'deletions' as const } : {}),
        align: 'start',
      });
    }
  }, [items, scrollRequestId]);

  useEffect(() => {
    registerScrollToFile(handleScrollToFile);
    return () => {
      registerScrollToFile(null);
    };
  }, [registerScrollToFile, handleScrollToFile]);

  return (
    <WorkerPoolContextProvider
      poolOptions={POOL_OPTIONS}
      highlighterOptions={HIGHLIGHTER_OPTIONS}
    >
      <CodeView<ExtendedCommentAnnotation>
        ref={codeViewRef}
        containerRef={scrollRootRef}
        items={items}
        options={options}
        onScroll={handleScroll}
        renderHeaderPrefix={renderHeaderPrefix}
        renderHeaderMetadata={renderHeaderMetadata}
        renderAnnotation={renderAnnotation}
        renderGutterUtility={renderGutterUtility}
        className={`w-full h-full overflow-auto bg-secondary px-base ${className}`}
      />
    </WorkerPoolContextProvider>
  );
});
