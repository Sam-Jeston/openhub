# Issues

Issues are the repository's lightweight tracker: a title, a Markdown description, labels, a comment thread, and an open/closed state. They work like GitHub's tracker and have no link to git — anyone who can read a repository can open an issue and comment.

Like GitHub, a closed issue records *why* in `close_reason`: `completed` renders as a green **Resolved** pill, and `not_planned` as a grey **Not planned** pill. The page offers "Close as resolved", "Close as not planned" and "Reopen".

**Source files:**
- `src/web/issues.rs`: handlers and label parsing.
- `src/db.rs`: issue, event and label queries.
- `migrations/0002_issues.sql`: schema.
- `templates/repo/{issues,issue,issue_new}.html`: pages.

## Lifecycle

```
                 create (New issue form or POST /issues)
                              │
                     ┌────────▼────────┐
        reopen       │                 │ close (author or writer)
   ┌────────────────▶│      open       ├────────────────┐
   │                 │                 │                ▼
   │                 └─────────────────┘             closed
   └──────────────────────────────────────────────────┘
```

| State | Set by | Fields |
| --- | --- | --- |
| `open` | `db::create_issue`, `db::reopen_issue` | `closed_at`, `closed_by`, `close_reason` are NULL |
| `closed` | `db::close_issue` | `closed_at` and `closed_by` set; `close_reason` is `completed` (resolved) or `not_planned` |

Closing appends a `closed` event (whose `body` holds the reason) and reopening appends a `reopened` event. Renaming through the edit form appends a `renamed` event. Comments are `comment` events.

## Numbering

Issues and pull requests share one number space per repository. `db::create_issue` takes `MAX(number) + 1` over both `issues` and `pull_requests` inside a transaction, so `#12` is either an issue or a pull request, never both. **Any future code that creates either kind of row must use the same union.**

## Routes

| Method | Path | Handler | Access |
| --- | --- | --- | --- |
| GET | `/{owner}/{repo}/issues` | `issues::list` | Read. `?state=closed` lists closed issues, `?label=…` filters by label |
| GET, POST | `/{owner}/{repo}/issues/new` and `/{owner}/{repo}/issues` | `issues::new_form`, `issues::create` | Signed-in user plus read |
| GET | `/{owner}/{repo}/issue/{n}` | `issues::show` | Read |
| POST | `/{owner}/{repo}/issue/{n}/comment` | `issues::comment` | Signed-in user. `close=1` closes with a comment |
| POST | `/{owner}/{repo}/issue/{n}/state` | `issues::set_state` | Author or write. `action=close|reopen`, `reason=completed|not_planned` |
| POST | `/{owner}/{repo}/issue/{n}/edit` | `issues::edit` | Author or write |

"Author or write" means the issue's author (`issues::can_manage`) or anyone with write access (`RepoCtx::access.write`, i.e. the owner, an admin, or a collaborator). Any reader, signed in, can comment. Private repositories return 404 to everyone without read access, exactly as every other page does.

## Labels

Labels are per repository and created on demand: the create and edit forms take a comma-separated list, `issues::parse_labels` trims, collapses whitespace, drops case-insensitive duplicates, caps names at 50 characters and keeps at most 20, and `db::ensure_label` inserts any missing label with a deterministic colour from `db::label_color`. The list page's sidebar offers one filter link per label; `db::list_issues` filters with an `EXISTS` subquery.

## Rendering and security

The description and every comment go through `markdown::render`, which sanitises with ammonia, and are emitted with `|safe` like pull request bodies. Label names are escaped by askama and their colours come from the server, so they are safe to place in a `style` custom property.

When you change this:

- Adding a field to `issues` means updating `ISSUE_SELECT`, the `Issue` struct and `db::IssueView`.
- Keep numbering shared with pull requests.
- Keep the "author or writer" checks on every state-changing handler.
- Extend `scripts/smoke-test.sh` for new flows.
