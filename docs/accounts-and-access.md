# Accounts and access

This page explains how people get accounts, how they prove who they are on each channel (browser, SSH and HTTP git), and what they can then do with each repository.

**Source files:**
- `src/web/account.rs`: sign-up, sign-in, sessions, profile, password and SSH keys.
- `src/web/ctx.rs`: session lookup and cookies.
- `src/state.rs`: the permission model.
- `src/validate.rs`: input rules.
- `src/ssh.rs`: SSH authentication.
- `src/web/git_http.rs`: HTTP Basic authentication.

## Accounts

### Registration

`GET`/`POST /register` (`account::register_form`, `account::register`).

- **When it's allowed.** Registration is open unless `--disable-registration` (`OPENHUB_DISABLE_REGISTRATION=true`) is set. Even then, the first account can always be created, so a new instance can't lock itself out. `account::can_register` holds the rule, and the landing and sign-in pages use it to show or hide the sign-up link.
- **The first account becomes the site administrator** (`users.is_admin = 1`). Register it before the server is reachable by anyone else.
- **Validation** (`src/validate.rs`):

  | Field | Rule |
  | --- | --- |
  | Username | 1–39 ASCII letters, digits or hyphens. Can't start or end with a hyphen or contain `--`. Not reserved. Unique, case-insensitively |
  | Email | Up to 254 characters, no whitespace, a non-empty local part, and a domain containing `.`. Unique, case-insensitively. Not verified by email |
  | Password | At least 8 characters and at most 256 bytes |

  Reserved usernames are names that would clash with top-level routes: `admin`, `api`, `assets`, `explore`, `favicon.svg`, `git`, `help`, `login`, `logout`, `new`, `register`, `root`, `settings`, `signin`, `signup`, `static`, `system`. Adding a top-level route means adding its first path segment to `RESERVED`.

- **Afterwards**, the new user is signed in and sent to **Settings → SSH keys** with a welcome message.

### Signing in

`POST /login` (`account::login`):

- The login field accepts a username or an email (`db::user_by_login`).
- A password is verified with Argon2id even when the account doesn't exist, against `DUMMY_HASH`. That way, response timing doesn't reveal which accounts exist. The error message is always "Incorrect username or password."
- `next` takes the user back to the page that required sign-in. `web::safe_next` only accepts same-site paths: it must start with `/`, but not `//` or `/\`.
- There is no rate limiting, lockout, two-factor authentication or password reset (see [Security](security.md#known-gaps)).

### Passwords

- Hashed with `argon2` 0.5 defaults (Argon2id, 19 MiB of memory, 2 iterations) and a random salt, inside `spawn_blocking`.
- Changing the password (`POST /settings/password`) requires the current password. It signs out every other session of that user and keeps the current one.

### Sessions

| Property | Value |
| --- | --- |
| Cookie | `openhub_session`, `HttpOnly`, `SameSite=Lax`, `Path=/`, 30-day `Max-Age`. `Secure` when `--public-url` starts with `https://` |
| Token | 32 random bytes from the OS RNG, base64url-encoded |
| Storage | Only `sha256(token)` is stored, in `sessions.token_hash`, so a database leak doesn't expose usable cookies |
| Lifetime | Fixed 30 days from sign-in. There's no sliding renewal |
| Validation | `ctx::middleware` hashes the cookie and calls `db::user_by_session`, which ignores expired rows. It skips this for git HTTP paths |
| Sign-out | `POST /logout` deletes the session row and clears the cookie |

### Flash messages

One-off messages shown after a redirect, such as "Profile updated.", are set by `web::redirect_flash`. They are stored in the `openhub_flash` cookie as base64url `kind\nmessage`, where kind is `success`, `error` or `info`. The middleware decodes the cookie into `Ctx.flash` and clears it on the next response, unless that response sets a new one.

### Profile

`/settings/profile` edits the display name (up to 100 characters), email and bio (up to 500 characters). Avatars are generated from the username, as a coloured initial (`fmt::avatar`). No images are uploaded or fetched.

## SSH keys

SSH keys are the only credential that can push.

**Adding a key** (`POST /settings/keys`, `account::add_key` and `validate::ssh_public_key`):
1. The input must be a single line. Input containing `PRIVATE KEY` is rejected, with a hint to paste the `.pub` file instead.
2. The line is parsed as an OpenSSH public key by the `ssh-key` crate.
3. The stored copy is normalised: re-encoded with the comment removed. The comment becomes the default title.
4. The SHA-256 fingerprint must not already be registered, by this user or anyone else. Because a key belongs to at most one account, the key alone identifies the user during SSH authentication.

**Deleting a key** (`POST /settings/keys/{id}/delete`) only works for the user's own keys. Anything else returns 404.

**Usage tracking.** `last_used_at` is updated each time the key authenticates, and shown on the keys page.

**Key types.** Ed25519 is recommended and is what the smoke test uses. Other algorithms that both the `ssh-key` parser and russh support also work.

## Identity on each channel

| Channel | How the user is identified | Code |
| --- | --- | --- |
| Browser | Session cookie | `ctx::middleware` → `Ctx.user` |
| SSH (`git push`, `fetch`, `clone`) | Public key fingerprint → `ssh_keys` → `users`. The SSH username, conventionally `git`, is ignored | `SshSession::auth_publickey` |
| HTTP git, public repository | Anonymous. Credentials aren't requested | `git_http::authorize` |
| HTTP git, private repository | HTTP Basic auth: a username or email, plus the account password, checked with Argon2id on every request | `git_http::authorize` |
| HTTP push | Always refused (403) | `git_http::receive_pack`, `info_refs` |

## Permission model

All permission checks go through `AppState::access(repo, user)`, which returns `Access { read, write, admin }`:

| Who | `read` | `write` | `admin` |
| --- | --- | --- | --- |
| Site administrator (`users.is_admin`) | ✓ | ✓ | ✓ |
| Repository owner | ✓ | ✓ | ✓ |
| Collaborator | ✓ | ✓ | |
| Any other signed-in user | public repositories only | | |
| Anonymous visitor | public repositories only | | |

### What each level allows

| Action | Needs | Enforced in |
| --- | --- | --- |
| View code, history, branches, tags and pull requests; download archives and raw files | `read` | `repo::load_repo` (every repository page) |
| Clone and fetch over SSH or HTTP | `read` | `SshSession::exec_request`, `git_http::authorize` |
| Push over SSH (create, update or delete branches and tags) | `write` | `SshSession::exec_request` |
| Open a pull request, or comment on one | Signed in, plus `read` | `pulls::create`, `pulls::comment` |
| Edit a pull request's title or body, close it or reopen it | The pull request's author, or `write` | `pulls::edit`, `pulls::close_pull`, `pulls::set_state` |
| Merge a pull request | `write` | `pulls::merge` (`RepoCtx::require_write`) |
| Delete a branch in the web interface (not the default branch) | `write` | `repo::delete_branch` |
| Change the name, description, visibility or default branch | `admin` | `repo::update_settings` (`RepoCtx::require_admin`) |
| Add or remove collaborators | `admin` | `repo::add_collaborator`, `repo::remove_collaborator` |
| Delete the repository | `admin`, plus typing `owner/name` to confirm | `repo::delete` |
| Create a repository | Signed in | `repo::create` (always owned by the creator) |

Notes:

- **Opening a pull request needs only `read`,** but both branches must already exist in the repository, and only people with `write` can push branches. So on a public repository, any signed-in user can open a pull request between existing branches and comment on it. They can't change any code.
- **Collaborators can't be granted read-only access.** The only level is write.
- **There are no organisations or teams.** Every repository belongs to one user.
- **The site administrator has no special pages.** Their power is `admin` on every repository. Promoting or demoting users, or removing accounts, is done in SQL (see [Operations](operations.md#administration-without-a-ui)).

### Private repositories look like missing ones

A user without `read` should be unable to tell a private repository from one that doesn't exist:

| Channel | Response when the repository is private and the user lacks access |
| --- | --- |
| Web | 404 page (`load_repo` returns `AppError::NotFound` before any other check) |
| SSH | `ERROR: Repository not found.`, the same as for a missing repository |
| HTTP git, with credentials | 404 `Repository not found` |
| HTTP git, without credentials | **401** with a Basic challenge, while a missing repository gets 404. This is a known gap, listed under [Security → Known gaps](security.md#known-gaps) |
| Profile and dashboard lists | Private repositories are only listed for the owner, collaborators and site administrators (`db::list_owner_repos`) |
| Explore and landing pages | Public repositories only |

New handlers must load repositories through `repo::load_repo`, which applies this rule, then call `require_write()` or `require_admin()` as needed.

## When you change this

- **New top-level route** (`/something`): add `something` to `RESERVED` in `src/validate.rs`. An existing user with that name would shadow the route, or the route would shadow the user, so check the database.
- **New permission level**, such as read-only collaborators: change `Access`, `AppState::access`, the collaborators table (via a migration), the settings page, `db::list_owner_repos` and `db::list_member_repos`, then update both tables above.
- **New credential type**, such as tokens: add it to the identity table, keep pushes on SSH unless a decision is made otherwise, and extend `scripts/smoke-test.sh`.
