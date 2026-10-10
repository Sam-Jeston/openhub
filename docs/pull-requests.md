# Pull requests

A pull request proposes merging one branch (the **head**) into another (the **base**) of the same repository. Openhub has no forks, so both branches always live in one bare repository. This page covers how pull requests are created, tracked as branches move, displayed, merged and closed.

**Source files:**
- `src/web/pulls.rs`: handlers.
- `src/sync.rs`: tracking.
- `src/git.rs`: `mergeability`, `merge`, `commits_between`, `range_diff`, `set_pull_ref`.
- `src/db.rs`: pull request and event queries.
- `templates/repo/{compare,pulls,pull}.html`: pages.

## Lifecycle

```
             create (compare page)
                     │
                     ▼
   ┌──────────────▶ open ───────────────┐
   │                 │                  │ merged in the web interface, or the
   │ reopen          │ close            │ head reached the base through a push
   │                 ▼                  ▼
   └────────────── closed            merged  (final)
```

| State | Range shown | Can change to |
| --- | --- | --- |
| `open` | Live: merge base of the current tips → head tip | `closed` (author or writer), `merged` (writer merges, or detected) |
| `closed` | Frozen at close: `base_sha..head_sha` | `open` (author or writer, if both branches still exist) |
| `merged` | Frozen at merge: `base_sha..head_sha` | Nothing; this state is final |

### Key columns

| Column | Meaning |
| --- | --- |
| `head_sha` | Last known tip of the head branch, mirrored in `refs/pull/<n>/head` |
| `base_sha` | The merge base of base and head. Despite the name, it is not the base branch's tip |
| `merge_commit_sha` | For web merges, the new tip of the base branch: the merge commit, the squash commit or the last rebased commit |

## Comparing and creating

**The compare page** is `GET /{owner}/{repo}/compare/{base}...{head}` (`pulls::compare`). `/compare?base=…&head=…` redirects there, and `/compare/{head}` uses the default branch as the base. It shows, in order:

1. A notice instead of results when no head is chosen, a branch doesn't exist, base and head are the same, the histories are unrelated, or the head has nothing new.
2. **Commits:** `git::commits_between(base_tip, head_tip)`, the commits on the head that aren't on the base, oldest first.
3. **Mergeability**, from `git::mergeability`.
4. **Diff:** `git::range_diff(merge_base, head_tip)`. This is a three-dot diff: only the head's changes since it diverged, not changes made on the base since then.
5. **A form** whose title defaults to the single commit's summary when there is exactly one commit, or else the branch name made readable (`feature/add-login` becomes `Feature/add login`).
6. **A link** to the existing open pull request for this base and head, instead of the form, when there is one.

**Creating** (`POST /{owner}/{repo}/pulls`, `pulls::create`) requires:
- a signed-in user with read access;
- a non-empty title, cut to 256 characters;
- both branches to exist;
- base and head to differ;
- the head not to be already contained in the base;
- the two histories to share a merge base.

If an open pull request already exists for the same base and head, the request redirects to it instead.

On success, Openhub:
1. Assigns the next number for this repository: `MAX(number) + 1`, inside a transaction.
2. Stores `head_sha` (the head tip) and `base_sha` (the merge base).
3. Writes `refs/pull/<n>/head`.

## Tracking open pull requests

Pushes can move either branch at any time, so `sync::refresh_pulls(state, repo_id, actor)` reconciles every open pull request in a repository. It runs:

| Trigger | `actor` recorded for detected merges |
| --- | --- |
| After every successful SSH push (`sync::after_push`) | The pusher |
| After a web merge (`pulls::merge`), to update the repository's other pull requests | The user who merged |
| On every view of an open pull request's conversation, commits or files tab (`pulls::load_pull`) | None, so the timeline reads "This pull request was merged" |

For each open pull request:

1. **The head branch no longer exists:** nothing changes. The page says the head branch no longer exists and still shows the commits from the stored head.
2. **The head tip moved:** `refs/pull/<n>/head` is moved to the new tip.
3. **The head tip is now an ancestor of, or equal to, the base tip:** the pull request is marked `merged`. This catches merges done on the command line and fast-forward pushes. In the database:
   - `merged_by` is set to `actor`.
   - `merge_commit_sha` is set to NULL.
   - `base_sha` keeps the stored merge base (or is computed if missing).
   - `head_sha` is set to the head tip.
   - A `merged` event is added.
4. **Otherwise:** if the head tip or the merge base changed, `head_sha` and `base_sha` are updated.

The git work runs in one `spawn_blocking` call, and the database writes follow it.

## Which commits a pull request shows

`pulls::pull_range` decides the commit range for every pull request tab:

- **Open:** `(merge_base(base tip, head tip), head tip)`, computed live. If the head branch is gone, it falls back to the stored `head_sha`, or `refs/pull/<n>/head`. If the base branch is gone, it falls back to the stored `base_sha`.
- **Closed or merged:** `(base_sha, head_sha)`, exactly as frozen. This is why `base_sha` stores the merge base rather than the base tip: after a merge, the base tip contains the head, so a live range would be empty.

From that range, `pulls::pull_data` derives:
- the commits (`commits_between`);
- the file, addition and deletion counts (`diff_stats`);
- the rendered diff (`range_diff`, on the files tab only);
- for open pull requests with both branches present, the mergeability.

## Mergeability

`git::mergeability(repo, base_tip, head_tip)` is computed on every view of an open pull request. It isn't cached.

| Result | Meaning | Page shows |
| --- | --- | --- |
| `UpToDate` | The head is already contained in the base | (Normally detected as merged first) |
| `Conflicts` | An in-memory three-way merge (`merge_commits`) has conflicts | Conflict notice with command-line instructions |
| `Clean` | The merge succeeds | Merge controls for users with write access |

## Merging in the web interface

`POST /{owner}/{repo}/pull/{n}/merge` with `method=merge|squash|rebase` (`pulls::merge` → `git::merge`). It requires `write`.

1. Re-reads both branch tips and re-checks mergeability. If there are conflicts, or nothing to merge, it shows an error message and changes nothing.
2. Builds the result in memory. The base branch isn't touched yet.

   | Method | Result | Author | Committer | Message |
   | --- | --- | --- | --- | --- |
   | `merge` | One commit with parents `[base tip, head tip]`, tree from the three-way merge | Merging user | Merging user | `Merge pull request #N from <head>`, a blank line, then the title |
   | `squash` | One commit with parent `[base tip]`, tree from the three-way merge | Author of the head tip commit | Merging user | `<title> (#N)`, plus a `* <summary>` line per commit when there's more than one |
   | `rebase` | Each non-merge commit in `base..head` cherry-picked in order onto the base tip | Original author | Merging user | Original message |

   About rebasing:
   - Merge commits in the range are skipped, as `git rebase` does.
   - A commit that becomes empty is dropped.
   - A conflict in any commit fails the whole merge with `Commit abc1234 cannot be rebased cleanly`.

   The merging user's identity is their display name (or username) and account email, with the current time. Commits aren't signed. No hooks run.
3. Moves the base branch with `repo.reference_matching(refs/heads/<base>, new_tip, force, expected = base tip from step 1)`. This is a compare-and-swap: if anyone pushed to the base since step 1, the update fails with "The base branch changed while merging; please try again". The objects already written stay unreferenced and are harmless.
4. Points `refs/pull/<n>/head` at the head tip that was merged.
5. Records the merge in the database:
   - `mark_merged` sets the state, `merged_by`, `merge_commit_sha = new_tip`, `head_sha` and `base_sha = merge base`.
   - A `merged` event is added.
   - `pushed_at` is updated.
   - `refresh_pulls` runs for the rest of the repository.

The head branch is kept. After a merge or close, the page offers **Delete branch** when all of these hold:
- the user has write access;
- the branch still exists;
- it isn't the default branch;
- no other open pull request uses it as its head.

Deleting it records a `branch_deleted` event.

## Closing, reopening and editing

- **Close:** use the **Close pull request** button beside the comment box. Any text in the box is posted as a comment first. Alternatively, `POST …/state` with `action=close`.
  - Allowed for the author, or anyone with write access.
  - Freezes `head_sha` and `base_sha` to the current `pull_range` and records a `closed` event.
- **Reopen** (`POST …/state` with `action=reopen`):
  - Allowed for the author, or anyone with write access.
  - Requires both branches to exist, and no other open pull request for the same base and head.
  - Recomputes `head_sha` and the merge base, moves `refs/pull/<n>/head` and records a `reopened` event.
  - Merged pull requests can't be reopened.
- **Edit** (`POST …/edit`): the author, or anyone with write access, can change the title and description.

## Conversation

The description (`pull_requests.body`) and comments (`pr_events` rows with `kind = 'comment'`) are Markdown, rendered with `markdown::render` without a link base. Relative links there aren't rewritten.

Any signed-in user with read access can comment. Comments can't be edited or deleted.

| Event `kind` | Created by | Shown as |
| --- | --- | --- |
| `comment` | `pulls::comment` | Comment box |
| `closed` | `pulls::close_pull` | "*user* closed this" |
| `reopened` | `pulls::set_state` | "*user* reopened this" |
| `merged` | `pulls::merge`, `sync::refresh_pulls` | "*user* merged commit abc1234 into main", or "This pull request was merged" when there's no author |
| `branch_deleted` | `repo::delete_branch` (when called with `pr`) | "*user* deleted the *branch* branch" |

## Where pull requests appear

- **Repository → Pull requests:** a list with open and closed tabs. The closed tab includes merged pull requests. The repository tab bar shows the open count.
- **Dashboard:** up to 20 open pull requests that the user authored, or that are in repositories they own or collaborate on.
- **Branches page:** a link from each branch to its open pull request.
- **Repository home:** a banner offering **Compare & pull request** when a non-default branch got a commit within the last hour, has no open pull request, and the viewer has write access.
- **Push output:** the "Create a pull request" or "View pull request" hints (see [Git transport](git-transport.md#after-a-push)).

## Checking out a pull request locally

```sh
git fetch origin pull/<n>/head:pr-<n>
git checkout pr-<n>
```

The sidebar of every pull request shows these commands.

## Edge cases

| Situation | Behaviour |
| --- | --- |
| Head branch force-pushed | The next refresh records the new head. `refs/pull/<n>/head` moves, so the old commits become unreferenced unless another ref holds them |
| Head branch deleted while open | The pull request stays open and shows its last known commits. It can't be merged until the branch is pushed again |
| Head branch renamed | Seen as a deletion. The pull request doesn't follow the new name |
| Base branch deleted | Can't be merged. The page says the base branch no longer exists |
| Head merged into base by a plain `git push` | Detected after that push and marked merged, with the pusher recorded |
| Base and head merged locally, but the merge result never pushed | Nothing changes until the base branch on the server contains the head |
| Two pull requests share a head branch with different bases | Allowed. Each is tracked independently |

## When you change this

- **New pull request state or event kind:** update the state table and event table above, the `-- …` comments in the migration (as a new migration if the schema changes), `templates/repo/pull.html` and the smoke test.
- **New merge method:** add it to `git::MergeMethod`, implement it in `git::merge` (keeping the compare-and-swap ref update) and add an option to the merge form.
- **Anything that writes `head_sha`** must also call `git::set_pull_ref`.
