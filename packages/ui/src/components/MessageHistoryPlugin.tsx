import { useEffect, useRef } from 'react';
import { useLexicalComposerContext } from '@lexical/react/LexicalComposerContext';
import {
  $createParagraphNode,
  $getRoot,
  $getSelection,
  $isRangeSelection,
  COMMAND_PRIORITY_CRITICAL,
  COMMAND_PRIORITY_LOW,
  KEY_ARROW_DOWN_COMMAND,
  KEY_ARROW_UP_COMMAND,
  type LexicalNode,
  type RangeSelection,
} from 'lexical';
import {
  $convertFromMarkdownString,
  $convertToMarkdownString,
  type Transformer,
} from '@lexical/markdown';
import { useTypeaheadOpen } from './TypeaheadOpenContext';

type Props = {
  /** Previously sent messages in chronological order (oldest first). */
  history: string[];
  /** Markdown transformers used to serialize/deserialize editor content. */
  transformers: Transformer[];
};

/**
 * Terminal-style command history recall.
 *
 * Pressing ArrowUp while the caret is on the first line walks backwards through
 * previously sent messages, dropping the recalled message into the editor.
 * ArrowDown walks forward again, eventually restoring the in-progress draft.
 *
 * Once the user edits a recalled message it becomes the live draft, so the next
 * ArrowUp starts a fresh walk from the most recent message (matching readline).
 */
export function MessageHistoryPlugin({ history, transformers }: Props) {
  const [editor] = useLexicalComposerContext();
  const { isOpen: isTypeaheadOpen } = useTypeaheadOpen();

  // TEMP DIAGNOSTICS — remove once history recall is confirmed working.
  console.info('[MsgHistory] render, history.length =', history.length);

  // Mirror the latest props/state into refs so the registered commands, which
  // are only attached once, always read fresh values.
  const historyRef = useRef(history);
  historyRef.current = history;
  const transformersRef = useRef(transformers);
  transformersRef.current = transformers;
  const typeaheadOpenRef = useRef(isTypeaheadOpen);
  typeaheadOpenRef.current = isTypeaheadOpen;

  // Navigation state. position 0 = live draft, 1 = most recent message, etc.
  const positionRef = useRef(0);
  const draftRef = useRef('');
  // True while we programmatically replace the editor content, so the update
  // listener can tell our own edits apart from the user's typing.
  const applyingRef = useRef(false);

  // A new message was sent (history grew) — reset back to the live draft.
  const historyLength = history.length;
  useEffect(() => {
    positionRef.current = 0;
    draftRef.current = '';
  }, [historyLength]);

  useEffect(() => {
    // True when the caret sits on the first visual line of the editor.
    //
    // Block identity alone isn't enough: in `Enter` send mode newlines are soft
    // LineBreakNodes inside a single paragraph, so a multi-line draft is one
    // top-level block. We additionally require that no line break precedes the
    // caret within the first block, so ArrowUp still moves up a line first.
    const $isCaretOnFirstLine = (selection: RangeSelection): boolean => {
      const anchorNode = selection.anchor.getNode();
      const topLevel = anchorNode.getTopLevelElement();
      const firstChild = $getRoot().getFirstChild();
      if (!topLevel || !firstChild || topLevel.getKey() !== firstChild.getKey()) {
        return false;
      }
      // Walk every node before the caret within the first block; a line break
      // (its text content is '\n') means the caret is on a later line.
      let current: LexicalNode | null = anchorNode;
      while (current && current.getKey() !== topLevel.getKey()) {
        let prev = current.getPreviousSibling();
        while (prev) {
          if (prev.getTextContent().includes('\n')) return false;
          prev = prev.getPreviousSibling();
        }
        current = current.getParent();
      }
      return true;
    };

    // Whether ArrowUp should start walking history. An empty editor always
    // qualifies — note that an empty editor frequently has no RangeSelection at
    // all (the root is cleared to zero children), so we must check emptiness
    // before requiring a caret.
    const $canEnterHistory = (): boolean => {
      if ($getRoot().getTextContent().length === 0) return true;
      const selection = $getSelection();
      if (!$isRangeSelection(selection) || !selection.isCollapsed()) {
        return false;
      }
      return $isCaretOnFirstLine(selection);
    };

    // Replaces the editor content. Must run inside an editor update context
    // (command listeners already provide one).
    const $applyMarkdown = (markdown: string) => {
      applyingRef.current = true;
      const root = $getRoot();
      root.clear();
      if (markdown.trim() === '') {
        root.append($createParagraphNode());
      } else {
        $convertFromMarkdownString(markdown, transformersRef.current);
      }
      root.selectEnd();
    };

    const $navigate = (direction: 'older' | 'newer'): boolean => {
      // TEMP DIAGNOSTICS — remove once confirmed working.
      console.info('[MsgHistory] $navigate', direction, {
        typeaheadOpen: typeaheadOpenRef.current,
        historyLen: historyRef.current.length,
        pos: positionRef.current,
        canEnter: direction === 'older' ? $canEnterHistory() : null,
      });

      if (typeaheadOpenRef.current) return false;

      const hist = historyRef.current;
      if (hist.length === 0) return false;

      const pos = positionRef.current;

      if (direction === 'older') {
        // Entering history requires the caret to be on the first line, so that
        // ArrowUp still moves between lines of a multi-line draft. Once we're
        // already walking history (pos > 0) we own the buffer and keep going.
        if (pos === 0 && !$canEnterHistory()) return false;
        // Already at the oldest entry — consume the key so the caret stays put.
        if (pos >= hist.length) return pos > 0;

        if (pos === 0) {
          draftRef.current = $convertToMarkdownString(transformersRef.current);
        }

        const nextPos = pos + 1;
        positionRef.current = nextPos;
        $applyMarkdown(hist[hist.length - nextPos]);
        return true;
      }

      // direction === 'newer'
      if (pos === 0) return false; // not navigating; let ArrowDown move the caret

      const nextPos = pos - 1;
      positionRef.current = nextPos;
      $applyMarkdown(
        nextPos === 0 ? draftRef.current : hist[hist.length - nextPos]
      );
      return true;
    };

    // When the user actually edits the buffer, the recalled text becomes their
    // new draft. Ignore pure selection changes and our own programmatic edits.
    const unregisterUpdate = editor.registerUpdateListener(
      ({ editorState, prevEditorState }) => {
        if (applyingRef.current) {
          applyingRef.current = false;
          return;
        }
        if (positionRef.current === 0) return;
        const current = editorState.read(() => $getRoot().getTextContent());
        const previous = prevEditorState.read(() =>
          $getRoot().getTextContent()
        );
        if (current !== previous) {
          positionRef.current = 0;
        }
      }
    );

    // TEMP DIAGNOSTICS — a non-consuming probe at the highest priority tells us
    // whether ArrowUp reaches the editor's command layer at all (independent of
    // any lower-priority interception). Remove once confirmed working.
    const unregisterProbe = editor.registerCommand(
      KEY_ARROW_UP_COMMAND,
      () => {
        console.info('[MsgHistory] ArrowUp reached editor (CRITICAL probe)');
        return false;
      },
      COMMAND_PRIORITY_CRITICAL
    );

    const unregisterUp = editor.registerCommand(
      KEY_ARROW_UP_COMMAND,
      (event: KeyboardEvent | null) => {
        const handled = $navigate('older');
        console.info('[MsgHistory] ArrowUp handler result:', handled);
        if (handled) event?.preventDefault();
        return handled;
      },
      COMMAND_PRIORITY_LOW
    );

    const unregisterDown = editor.registerCommand(
      KEY_ARROW_DOWN_COMMAND,
      (event: KeyboardEvent | null) => {
        const handled = $navigate('newer');
        if (handled) event?.preventDefault();
        return handled;
      },
      COMMAND_PRIORITY_LOW
    );

    return () => {
      unregisterUpdate();
      unregisterProbe();
      unregisterUp();
      unregisterDown();
    };
  }, [editor]);

  return null;
}
