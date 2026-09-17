import { useState, useCallback, useEffect, useRef, memo } from 'react';
import { useTranslation } from 'react-i18next';
import { PrimaryButton } from '@vibe/ui/components/PrimaryButton';
import { CommentCard } from '@vibe/ui/components/CommentCard';
import WYSIWYGEditor from '@/shared/components/WYSIWYGEditor';
import { useReview, type ReviewDraft } from '@/shared/hooks/useReview';

interface CommentWidgetLineProps {
  draft: ReviewDraft;
  widgetKey: string;
  onSave: () => void;
  onCancel: () => void;
}

export const CommentWidgetLine = memo(function CommentWidgetLine({
  draft,
  widgetKey,
  onSave,
  onCancel,
}: CommentWidgetLineProps) {
  const { t } = useTranslation('common');
  const { drafts, setDraft, addComment } = useReview();
  // Seed from the stored draft, not the prop: the changes panel virtualizes,
  // so this widget is unmounted and remounted as its file leaves and re-enters
  // the rendered window, and the prop's draft object can be a cached one.
  const [value, setValue] = useState(
    () => drafts[widgetKey]?.text ?? draft.text
  );

  const latestValueRef = useRef(value);
  latestValueRef.current = value;
  const latestDraftRef = useRef(draft);
  latestDraftRef.current = drafts[widgetKey] ?? draft;
  const setDraftRef = useRef(setDraft);
  setDraftRef.current = setDraft;
  const finishedRef = useRef(false);

  // Flush in-progress text back to the draft when the widget goes away for any
  // reason other than save/cancel — otherwise scrolling the file out of view
  // discards whatever has been typed.
  useEffect(
    () => () => {
      if (finishedRef.current) return;
      const current = latestDraftRef.current;
      if (current && latestValueRef.current !== current.text) {
        setDraftRef.current(widgetKey, {
          ...current,
          text: latestValueRef.current,
        });
      }
    },
    [widgetKey]
  );

  const handleCancel = useCallback(() => {
    finishedRef.current = true;
    setDraft(widgetKey, null);
    onCancel();
  }, [setDraft, widgetKey, onCancel]);

  const handleSave = useCallback(() => {
    finishedRef.current = true;
    if (value.trim()) {
      addComment({
        filePath: draft.filePath,
        side: draft.side,
        lineNumber: draft.lineNumber,
        text: value.trim(),
        codeLine: draft.codeLine,
      });
    }
    setDraft(widgetKey, null);
    onSave();
  }, [value, draft, setDraft, widgetKey, onSave, addComment]);

  return (
    <CommentCard
      variant="input"
      actions={
        <>
          <PrimaryButton
            variant="default"
            onClick={handleSave}
            disabled={!value.trim()}
          >
            {t('comments.addReviewComment')}
          </PrimaryButton>
          <PrimaryButton variant="secondary" onClick={handleCancel}>
            {t('actions.cancel')}
          </PrimaryButton>
        </>
      }
    >
      <WYSIWYGEditor
        value={value}
        onChange={setValue}
        placeholder={t('comments.addPlaceholder')}
        className="w-full text-normal min-h-[60px]"
        onCmdEnter={handleSave}
        autoFocus
      />
    </CommentCard>
  );
});
