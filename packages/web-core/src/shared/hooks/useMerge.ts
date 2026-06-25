import { useMutation, useQueryClient } from '@tanstack/react-query';
import { workspacesApi } from '@/shared/lib/api';
import { repoBranchKeys } from '@/shared/hooks/useRepoBranches';
import type { MergeWorkspaceResponse } from 'shared/types';

type MergeParams = {
  repoId: string;
};

export function useMerge(
  workspaceId?: string,
  onSuccess?: (result: MergeWorkspaceResponse) => void,
  onError?: (err: unknown) => void
) {
  const queryClient = useQueryClient();

  return useMutation<MergeWorkspaceResponse, unknown, MergeParams>({
    mutationFn: (params: MergeParams) => {
      if (!workspaceId) return Promise.resolve({ generating: false });
      return workspacesApi.merge(workspaceId, {
        repo_id: params.repoId,
      });
    },
    onSuccess: (result) => {
      // Refresh attempt-specific branch information
      queryClient.invalidateQueries({
        queryKey: ['branchStatus', workspaceId],
      });

      // Invalidate all repo branches queries
      queryClient.invalidateQueries({ queryKey: repoBranchKeys.all });

      onSuccess?.(result);
    },
    onError: (err) => {
      console.error('Failed to merge:', err);
      onError?.(err);
    },
  });
}
