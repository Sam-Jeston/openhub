//! Keeps database state in step with repository refs after they change,
//! whether through a push or a merge in the web interface.

use anyhow::Result;
use git2::Oid;

use crate::db::{self, Repo};
use crate::git;
use crate::state::AppState;

enum Change {
    /// The head branch moved or the merge base changed.
    Updated {
        id: i64,
        head: Oid,
        merge_base: Option<Oid>,
    },
    /// The head is now contained in the base, e.g. merged on the command line.
    Merged {
        id: i64,
        head: Oid,
        merge_base: String,
    },
}

/// Called after a successful push.
pub async fn after_push(state: &AppState, repo: &Repo, pusher: i64) -> Result<()> {
    db::touch_pushed(&state.db, repo.id).await?;
    let path = state.config.repo_path(repo.id);
    let preferred = repo.default_branch.clone();
    let head =
        tokio::task::spawn_blocking(move || git::ensure_head(&git::open(&path)?, &preferred))
            .await??;
    if let Some(head) = head.filter(|h| *h != repo.default_branch) {
        db::set_default_branch(&state.db, repo.id, &head).await?;
    }
    refresh_pulls(state, repo.id, Some(pusher)).await
}

/// Re-evaluates every open pull request of a repository.
pub async fn refresh_pulls(state: &AppState, repo_id: i64, actor: Option<i64>) -> Result<()> {
    let pulls = db::list_pulls(&state.db, repo_id, true).await?;
    if pulls.is_empty() {
        return Ok(());
    }
    let path = state.config.repo_path(repo_id);
    let changes = tokio::task::spawn_blocking(move || -> Result<Vec<Change>> {
        let repo = git::open(&path)?;
        let mut changes = Vec::new();
        for p in pulls {
            // A deleted head branch leaves the pull request as it was.
            let Some(head) = git::branch_tip(&repo, &p.head_branch) else {
                continue;
            };
            let head_changed = p.head_sha.as_deref() != Some(head.to_string().as_str());
            if head_changed {
                git::set_pull_ref(&repo, p.number, head)?;
            }
            let base = git::branch_tip(&repo, &p.base_branch);
            match base {
                Some(base) if git::is_ancestor(&repo, head, base) => {
                    let merge_base = p
                        .base_sha
                        .clone()
                        .or_else(|| git::merge_base(&repo, head, base).map(|o| o.to_string()))
                        .unwrap_or_else(|| head.to_string());
                    changes.push(Change::Merged {
                        id: p.id,
                        head,
                        merge_base,
                    });
                }
                _ => {
                    let merge_base = base.and_then(|b| git::merge_base(&repo, b, head));
                    let base_changed =
                        merge_base.map(|m| m.to_string()) != p.base_sha && merge_base.is_some();
                    if head_changed || base_changed {
                        changes.push(Change::Updated {
                            id: p.id,
                            head,
                            merge_base,
                        });
                    }
                }
            }
        }
        Ok(changes)
    })
    .await??;

    for change in changes {
        match change {
            Change::Updated {
                id,
                head,
                merge_base,
            } => {
                let mb = merge_base.map(|m| m.to_string());
                db::update_pull_head(&state.db, id, &head.to_string(), mb.as_deref()).await?;
            }
            Change::Merged {
                id,
                head,
                merge_base,
            } => {
                db::mark_merged(&state.db, id, actor, None, &head.to_string(), &merge_base).await?;
                db::add_event(&state.db, id, actor, "merged", "").await?;
            }
        }
    }
    Ok(())
}
