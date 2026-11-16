# Openhub documentation

These pages explain how Openhub works inside: its components, the data they keep and the rules they follow. They are written for two readers:

- **People** running, reviewing or extending Openhub.
- **Coding agents** that need accurate context before changing the system.

Related documents:

- The [project README](../README.md) covers installing, configuring and using Openhub.
- [`CLAUDE.md`](../CLAUDE.md) and [`templates/CLAUDE.md`](../templates/CLAUDE.md) cover coding conventions and pitfalls.

These pages fill the gap between those: they explain the mechanics, so you can change the system without first reading every source file.

## Contents

| Document | Read it to learn |
| --- | --- |
| [Architecture](architecture.md) | What runs in the process, how requests flow through it, and where files live on disk |
| [Data model](data-model.md) | The SQLite tables, the bare repositories, and the invariants that tie them together |
| [Accounts and access](accounts-and-access.md) | How sign-up, sessions and SSH keys work, and who may read, push to or administer a repository |
| [Git transport](git-transport.md) | How `git push`, `fetch` and `clone` are served over SSH and HTTP, and how archives are built |
| [Pull requests](pull-requests.md) | The pull request lifecycle, how heads and merge bases are tracked, and the three merge methods |
| [Issues](issues.md) | The issue tracker: numbering, labels, comments, and closing and reopening |
| [Web interface](web-interface.md) | Routes, the request pipeline, code browsing, diffs, syntax highlighting and Markdown |
| [Security](security.md) | The threat model, the protections in place, known gaps and hardening advice |
| [Operations](operations.md) | How to deploy, back up, upgrade, administer and troubleshoot an instance |

## Quick lookup

| Question | Where the answer is |
| --- | --- |
| Who may do what to a repository? | [Accounts and access → Permission model](accounts-and-access.md#permission-model) (`AppState::access` in `src/state.rs`) |
| What happens when someone pushes? | [Git transport → After a push](git-transport.md#after-a-push) (`sync::after_push` in `src/sync.rs`) |
| How does Openhub notice a pull request was merged on the command line? | [Pull requests → Tracking open pull requests](pull-requests.md#tracking-open-pull-requests) |
| How does a web merge avoid racing a concurrent push? | [Pull requests → Merging in the web interface](pull-requests.md#merging-in-the-web-interface) |
| Why is issue #5 missing when #4 and #6 are pull requests? | [Issues → Numbering](issues.md#numbering) |
| How does `/tree/feature/login/src` know where the branch name ends? | [Web interface → Refs and paths in URLs](web-interface.md#refs-and-paths-in-urls) |
| Where is a repository stored on disk? | [Data model → Repositories on disk](data-model.md#repositories-on-disk) |
| Which work uses libgit2, and which uses the `git` program? | [Architecture → libgit2 and the git executable](architecture.md#libgit2-and-the-git-executable) |
| How do I add a database column? | [Data model → Changing the schema](data-model.md#changing-the-schema) |
| Is it safe to leave sign-ups open? | [Security → Known gaps](security.md#known-gaps) |
| How do I make someone an administrator? | [Operations → Administration without a UI](operations.md#administration-without-a-ui) |
| Why does a form say "Cross-origin request rejected"? | [Operations → Troubleshooting](operations.md#troubleshooting) |

## Conventions in these pages

- Code is referenced by file and function name (for example `git::merge` in `src/git.rs`), not by line number, so references survive edits.
- "Repository" means a hosted repository. "Bare repository" means its directory on disk.
- Most pages end with a **When you change this** list naming the other places that must change with it.
- `docs/screenshots/` holds the images embedded in the project README. [`CLAUDE.md`](../CLAUDE.md) explains when and how to retake them.

## Keeping these docs accurate

These pages describe behaviour that the code implements. When a change alters something described here, update the page in the same change. If a page and the code disagree, the code is right and the page is a bug.
