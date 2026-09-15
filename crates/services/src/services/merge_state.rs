use chrono::{DateTime, Utc};
use db::models::merge::{Merge, MergeStatus};

/// Whether a repo still holds workspace work that has not landed on its target
/// branch.
///
/// `commits_ahead` is how far the workspace branch is ahead of the repo's
/// target branch and `branch_head_time` the committer time of that branch's
/// head — both `None` when they could not be determined (a missing branch, an
/// unreadable repo). An undetermined repo never blocks on its own.
///
/// A direct merge rewinds the workspace branch onto the squash commit, so a
/// repo merged that way reports zero commits ahead and settles here; direct
/// merge records themselves say nothing. A PR is merged on the remote and
/// leaves the local branch ahead of the target (squash merges especially), so
/// for a merged PR the commit times decide: commits that predate the merge are
/// leftovers from it, commits made after it are unlanded work.
pub fn repo_has_unmerged_work(
    merges: &[Merge],
    commits_ahead: Option<usize>,
    branch_head_time: Option<DateTime<Utc>>,
) -> bool {
    let mut has_merged_pr = false;
    let mut latest_pr_merge: Option<DateTime<Utc>> = None;

    for merge in merges {
        if let Merge::Pr(pr) = merge {
            match pr.pr_info.status {
                // Work is still in flight, whatever the commit counts say.
                MergeStatus::Open => return true,
                MergeStatus::Merged => {
                    has_merged_pr = true;
                    if let Some(merged_at) = pr.pr_info.merged_at {
                        latest_pr_merge =
                            Some(latest_pr_merge.map_or(merged_at, |latest| latest.max(merged_at)));
                    }
                }
                MergeStatus::Closed | MergeStatus::Unknown => {}
            }
        }
    }

    if commits_ahead.unwrap_or(0) == 0 {
        return false;
    }

    if !has_merged_pr {
        return true;
    }

    match (latest_pr_merge, branch_head_time) {
        (Some(merged_at), Some(head_time)) => head_time > merged_at,
        // Without both timestamps, leftovers and follow-up work are
        // indistinguishable; treat the merged PR as settling the repo.
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use db::models::merge::{DirectMerge, PrMerge, PullRequestInfo};
    use uuid::Uuid;

    use super::*;

    fn at(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 15, hour, 0, 0).unwrap()
    }

    fn direct_merge() -> Merge {
        Merge::Direct(DirectMerge {
            id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            repo_id: Uuid::new_v4(),
            merge_commit: "abc123".to_string(),
            target_branch_name: "main".to_string(),
            created_at: at(12),
        })
    }

    fn pr_merge(status: MergeStatus, merged_at: Option<DateTime<Utc>>) -> Merge {
        Merge::Pr(PrMerge {
            id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            repo_id: Uuid::new_v4(),
            created_at: at(9),
            target_branch_name: "main".to_string(),
            pr_info: PullRequestInfo {
                number: 7,
                url: "https://example.com/pr/7".to_string(),
                status,
                merged_at,
                merge_commit_sha: None,
            },
        })
    }

    #[test]
    fn untouched_repo_with_commits_is_unmerged() {
        assert!(repo_has_unmerged_work(&[], Some(3), Some(at(12))));
    }

    #[test]
    fn repo_without_commits_is_settled() {
        assert!(!repo_has_unmerged_work(&[], Some(0), Some(at(12))));
    }

    #[test]
    fn directly_merged_repo_is_settled_by_its_rewound_branch() {
        // A direct merge rewinds the branch onto the squash commit; the merge
        // record itself carries no weight, so zero commits ahead is what
        // settles the repo.
        assert!(!repo_has_unmerged_work(&[direct_merge()], Some(0), None));
        assert!(!repo_has_unmerged_work(&[], Some(0), None));
    }

    #[test]
    fn commits_after_a_direct_merge_are_unmerged() {
        assert!(repo_has_unmerged_work(&[direct_merge()], Some(1), None));
    }

    #[test]
    fn merged_pr_settles_squash_leftovers() {
        // Branch commits predate the merge: they are what the PR squashed.
        assert!(!repo_has_unmerged_work(
            &[pr_merge(MergeStatus::Merged, Some(at(12)))],
            Some(4),
            Some(at(11)),
        ));
    }

    #[test]
    fn commits_after_a_merged_pr_are_unmerged() {
        assert!(repo_has_unmerged_work(
            &[pr_merge(MergeStatus::Merged, Some(at(12)))],
            Some(1),
            Some(at(13)),
        ));
    }

    #[test]
    fn merged_pr_settles_repo_when_timestamps_are_missing() {
        // No merged_at, or no readable branch head: leftovers and follow-up
        // work can't be told apart, so keep the merged PR authoritative.
        assert!(!repo_has_unmerged_work(
            &[pr_merge(MergeStatus::Merged, None)],
            Some(4),
            Some(at(13)),
        ));
        assert!(!repo_has_unmerged_work(
            &[pr_merge(MergeStatus::Merged, Some(at(12)))],
            Some(4),
            None,
        ));
    }

    #[test]
    fn latest_merged_pr_wins_over_an_earlier_one() {
        let merges = [
            pr_merge(MergeStatus::Merged, Some(at(10))),
            pr_merge(MergeStatus::Merged, Some(at(14))),
        ];
        assert!(!repo_has_unmerged_work(&merges, Some(2), Some(at(13))));
        assert!(repo_has_unmerged_work(&merges, Some(2), Some(at(15))));
    }

    #[test]
    fn open_pr_is_unmerged_even_without_commits_ahead() {
        assert!(repo_has_unmerged_work(
            &[pr_merge(MergeStatus::Open, None)],
            Some(0),
            Some(at(12)),
        ));
    }

    #[test]
    fn open_pr_is_unmerged_even_with_an_unreadable_branch() {
        assert!(repo_has_unmerged_work(
            &[pr_merge(MergeStatus::Open, None)],
            None,
            None
        ));
    }

    #[test]
    fn open_pr_wins_over_an_earlier_merged_pr() {
        assert!(repo_has_unmerged_work(
            &[
                pr_merge(MergeStatus::Merged, Some(at(12))),
                pr_merge(MergeStatus::Open, None),
            ],
            Some(0),
            Some(at(11)),
        ));
    }

    #[test]
    fn closed_and_unknown_prs_leave_commits_unmerged() {
        for status in [MergeStatus::Closed, MergeStatus::Unknown] {
            assert!(repo_has_unmerged_work(
                &[pr_merge(status.clone(), None)],
                Some(2),
                Some(at(12)),
            ));
            assert!(!repo_has_unmerged_work(
                &[pr_merge(status, None)],
                Some(0),
                Some(at(12)),
            ));
        }
    }

    #[test]
    fn undetermined_commit_count_does_not_block() {
        assert!(!repo_has_unmerged_work(&[], None, None));
        assert!(!repo_has_unmerged_work(
            &[direct_merge()],
            None,
            Some(at(12))
        ));
    }
}
