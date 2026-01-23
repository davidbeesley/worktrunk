//! Sync command implementation.
//!
//! Synchronizes a worktree branch with a target branch (like merge but keeps the worktree).

use worktrunk::HookType;
use worktrunk::git::Repository;
use worktrunk::styling::{eprintln, info_message, success_message};

use super::command_approval::approve_command_batch;
use super::command_executor::CommandContext;
use super::commit::CommitOptions;
use super::context::CommandEnv;
use super::hooks::{HookFailureStrategy, run_hook_with_filter};
use super::merge::run_pre_merge_commands;
use super::project_config::{HookCommand, collect_commands_for_hooks};
use super::repository_ext::RepositoryCliExt;
use super::worktree::{MergeOperations, handle_push};

/// Options for the sync command
pub struct SyncOptions<'a> {
    /// Branch to sync (required from main worktree, optional from linked worktree)
    pub branch: Option<&'a str>,
    /// Target branch to sync with (defaults to default branch)
    pub target: Option<&'a str>,
    /// CLI override for squash
    pub squash: Option<bool>,
    /// CLI override for commit
    pub commit: Option<bool>,
    /// CLI override for rebase
    pub rebase: Option<bool>,
    /// CLI override for verify (hooks)
    pub verify: Option<bool>,
    pub yes: bool,
    /// CLI override for stage mode
    pub stage: Option<super::commit::StageMode>,
}

/// Collect all commands that will be executed during sync.
fn collect_sync_commands(
    repo: &Repository,
    commit: bool,
    verify: bool,
) -> anyhow::Result<(Vec<HookCommand>, String)> {
    let mut all_commands = Vec::new();
    let project_config = match repo.load_project_config()? {
        Some(cfg) => cfg,
        None => return Ok((all_commands, repo.project_identifier()?.to_string())),
    };

    let mut hooks = Vec::new();

    if commit && verify && repo.current_worktree().is_dirty()? {
        hooks.push(HookType::PreCommit);
    }

    if verify {
        hooks.push(HookType::PreMerge);
        hooks.push(HookType::PostMerge);
    }

    all_commands.extend(collect_commands_for_hooks(&project_config, &hooks));

    let project_id = repo.project_identifier()?.to_string();
    Ok((all_commands, project_id))
}

pub fn handle_sync(opts: SyncOptions<'_>) -> anyhow::Result<()> {
    let SyncOptions {
        branch,
        target,
        squash: squash_opt,
        commit: commit_opt,
        rebase: rebase_opt,
        verify: verify_opt,
        yes,
        stage,
    } = opts;

    let env = CommandEnv::for_action("sync")?;
    let repo = &env.repo;
    let config = &env.config;

    // Cache current worktree
    let current_wt = repo.current_worktree();
    let in_main = !current_wt.is_linked().unwrap_or(false);

    // Determine which branch to sync
    let sync_branch = match branch {
        Some(b) => b.to_string(),
        None => {
            if in_main {
                return Err(worktrunk::git::GitError::Other {
                    message: "Branch argument required when running from main worktree. Use: wt sync <branch>".into(),
                }.into());
            }
            // Use current branch
            env.require_branch("sync")?.to_string()
        }
    };

    // If we're in main and syncing a different branch, we need to operate on that worktree
    let (worktree_path, working_branch) = if in_main && branch.is_some() {
        // Find the worktree for the specified branch
        let wt_path = repo.worktree_for_branch(&sync_branch)?.ok_or_else(|| {
            worktrunk::git::GitError::WorktreeNotFound {
                branch: sync_branch.clone(),
            }
        })?;
        (wt_path, sync_branch.clone())
    } else {
        // Operating on current worktree
        (current_wt.root()?.to_path_buf(), sync_branch.clone())
    };

    // Get effective merge config (reuse merge config for consistency)
    let merge_config = env.merge();

    // Determine final values: CLI > project config > global config > default (true)
    let squash = squash_opt
        .or_else(|| merge_config.as_ref().and_then(|m| m.squash))
        .unwrap_or(true);
    let commit = commit_opt
        .or_else(|| merge_config.as_ref().and_then(|m| m.commit))
        .unwrap_or(true);
    let rebase = rebase_opt
        .or_else(|| merge_config.as_ref().and_then(|m| m.rebase))
        .unwrap_or(true);
    let verify = verify_opt
        .or_else(|| merge_config.as_ref().and_then(|m| m.verify))
        .unwrap_or(true);

    // Stage mode
    let stage_mode = stage
        .or_else(|| env.commit().and_then(|c| c.stage))
        .unwrap_or_default();

    // Get target branch (defaults to default branch)
    let target_branch = repo.require_target_branch(target)?;

    // Validate --no-commit requires clean working tree
    let target_wt = repo.worktree_at(&worktree_path);
    if !commit && target_wt.is_dirty()? {
        return Err(worktrunk::git::GitError::UncommittedChanges {
            action: Some("sync with --no-commit".into()),
            branch: Some(working_branch.clone()),
            force_hint: false,
        }
        .into());
    }

    // --no-commit implies --no-squash
    let squash_enabled = squash && commit;

    // Collect and approve all commands upfront
    let (all_commands, project_id) = collect_sync_commands(repo, commit, verify)?;

    let approved = approve_command_batch(&all_commands, &project_id, config, yes, false)?;

    let verify = if !approved {
        eprintln!("{}", info_message("Commands declined, continuing sync"));
        false
    } else {
        verify
    };

    // Handle uncommitted changes
    let committed = if commit && target_wt.is_dirty()? {
        if squash_enabled {
            false // Squash path handles staging and committing
        } else {
            let ctx = CommandContext::new(repo, config, Some(&working_branch), &worktree_path, yes);
            let mut options = CommitOptions::new(&ctx);
            options.target_branch = Some(&target_branch);
            options.no_verify = !verify;
            options.stage_mode = stage_mode;
            options.warn_about_untracked = stage_mode == super::commit::StageMode::All;
            options.show_no_squash_note = true;

            options.commit()?;
            true
        }
    } else {
        false
    };

    // Squash commits if enabled
    let squashed = if squash_enabled {
        matches!(
            super::step_commands::handle_squash(
                Some(&target_branch),
                yes,
                !verify,
                Some(stage_mode)
            )?,
            super::step_commands::SquashResult::Squashed
        )
    } else {
        false
    };

    // Rebase onto target
    let rebased = if rebase {
        matches!(
            super::step_commands::handle_rebase(Some(&target_branch))?,
            super::step_commands::RebaseResult::Rebased
        )
    } else {
        if !repo.is_rebased_onto(&target_branch)? {
            return Err(worktrunk::git::GitError::NotRebased {
                target_branch: target_branch.clone(),
            }
            .into());
        }
        false
    };

    // Run pre-merge hooks
    if verify {
        let ctx = CommandContext::new(repo, config, Some(&working_branch), &worktree_path, yes);
        let project_config = repo.load_project_config()?.unwrap_or_default();
        run_pre_merge_commands(&project_config, &ctx, &target_branch, None, &[])?;
    }

    // Fast-forward push to target branch
    handle_push(
        Some(&target_branch),
        "Synced to",
        Some(MergeOperations {
            committed,
            squashed,
            rebased,
        }),
    )?;

    // Show success - worktree is preserved
    eprintln!(
        "{}",
        success_message(format!("Worktree preserved at {}", worktree_path.display()))
    );

    // Run post-merge hooks in the target worktree if it exists, otherwise current
    if verify {
        let destination_path = repo
            .worktree_for_branch(&target_branch)?
            .unwrap_or_else(|| repo.home_path().unwrap_or_default());

        let ctx = CommandContext::new(repo, config, Some(&working_branch), &destination_path, yes);
        execute_post_sync_commands(&ctx, &target_branch)?;
    }

    Ok(())
}

/// Execute post-merge commands after sync (reuses post-merge hooks)
fn execute_post_sync_commands(ctx: &CommandContext, target_branch: &str) -> anyhow::Result<()> {
    let project_config = ctx.repo.load_project_config()?;

    let vars = vec![("target", target_branch)];
    run_hook_with_filter(
        ctx,
        ctx.config.hooks.post_merge.as_ref(),
        project_config
            .as_ref()
            .and_then(|c| c.hooks.post_merge.as_ref()),
        HookType::PostMerge,
        &vars,
        HookFailureStrategy::Warn,
        None,
        crate::output::pre_hook_display_path(ctx.worktree_path),
    )
    .map_err(worktrunk::git::add_hook_skip_hint)
}
