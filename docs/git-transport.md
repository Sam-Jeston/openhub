# Git transport

This page explains how git clients talk to Openhub. Pushes are accepted only over SSH, by a built-in SSH server. Clones and fetches work over SSH and over read-only smart HTTP. In every case the git protocol itself is handled by the installed `git` program, and Openhub decides who may run it against which repository.

**Source files:**
- `src/ssh.rs`: the SSH server.
- `src/web/git_http.rs`: smart HTTP.
- `src/sync.rs`: bookkeeping after a push.
- `src/web/code.rs` (`archive`): downloads.
- `src/config.rs`: clone URLs.

## Channels at a glance

| Channel | Operations | Authentication | Runs |
| --- | --- | --- | --- |
| SSH (`--ssh-addr`, default port 2222) | clone, fetch, push | SSH public key | `git upload-pack` or `git receive-pack` |
| Smart HTTP (`--http-addr`, default port 3000) | clone, fetch | None for public repositories; Basic auth for private ones | `git upload-pack --stateless-rpc` |
| HTTP archive download | ZIP or tar.gz of a branch, tag or commit | Browser session, or anonymous for public repositories | `git archive` |

## Clone URLs

`Config` builds the URLs shown in the **Clone** dropdown, in push hints and in the documentation pages:

| URL | Built by | Example |
| --- | --- | --- |
| SSH, port 22 | `ssh_clone_url` | `git@git.example.com:alice/project.git` |
| SSH, any other port | `ssh_clone_url` | `ssh://git@git.example.com:2222/alice/project.git` |
| HTTP | `http_clone_url` | `https://git.example.com/alice/project.git` |
| SSH test command | `ssh_test_command` | `ssh -T -p 2222 git@git.example.com` |

The SSH host is `--ssh-host`, or failing that the host of `--public-url`. The SSH port is `--ssh-port`, or failing that the port of `--ssh-addr`. Set both when Openhub runs behind port forwarding, for example container port 2222 published as host port 22.

## SSH server

The server is built on russh (`ssh::serve`) and listens on `--ssh-addr`.

### Host key

On first start, Openhub generates an Ed25519 host key at `<data>/ssh_host_ed25519_key`, in OpenSSH format with mode `0600`, and reuses it on every later start. Its SHA-256 fingerprint is logged at startup so users can check it. If the key is lost, every client sees a "host key changed" warning, so keep it in backups.

### Server settings

| Setting | Value |
| --- | --- |
| Version string | `SSH-2.0-Openhub` |
| Authentication methods | `publickey` only. No passwords or keyboard-interactive |
| Delay after a rejected authentication | 300 ms (none for the first "none" probe) |
| Inactivity timeout | 1 hour |
| Keepalive interval | 60 s |

### Authentication

1. **`auth_publickey_offered`.** The client offers a public key, and Openhub accepts the offer only if its fingerprint is in `ssh_keys`. Clients therefore only sign with keys that will work, and move on to the next key otherwise.
2. **`auth_publickey`.** russh has verified the client's signature. Openhub loads the owning user, updates `last_used_at` and logs the sign-in with the peer address.

The SSH username is ignored. Anything works, but clone URLs use `git`.

### Channel requests

| Request | Response |
| --- | --- |
| `env GIT_PROTOCOL=…` | Stored and passed to git, which enables protocol v2. Other variables are refused |
| `pty-req` | Refused |
| `shell` (for example `ssh -T`) | Prints `Hi <user>! You've successfully authenticated, but Openhub does not provide shell access.` and exits with status 1 |
| `subsystem` (for example `sftp`) | Refused |
| `exec` | Parsed as a git command. See below |

### Command parsing

`parse_command` accepts exactly these forms, which are what git clients send:

```
git-upload-pack '<path>'
git-receive-pack '<path>'
git upload-pack '<path>'
git receive-pack '<path>'
```

It then processes the path:

1. Removes surrounding single or double quotes.
2. Rejects the path if it still contains `'`, `"`, `\` or a NUL byte.
3. Trims `/` from both ends, then removes a trailing `.git`.
4. Requires exactly `owner/name`.

Anything else, including `git-upload-archive` (that is, `git archive --remote`), gets `ERROR: Openhub only supports git fetch and push over SSH.` and exit status 1.

### Authorisation and execution

`SshSession::exec_request` does the following:

1. Looks up the repository with `db::repo_by_name` and calls `AppState::access`.
2. Checks permissions:
   - No read access, or no such repository: `ERROR: Repository not found.`
   - `receive-pack` without write access: `ERROR: You do not have permission to push to owner/name.`
3. For `receive-pack`, records every branch tip (`branch_tips`), so push hints can tell what changed.
4. Spawns `<git-bin> upload-pack|receive-pack <data>/repos/<id>.git`:
   - The path comes from the database ID, never from the client's string.
   - No shell is involved.
   - stdin, stdout and stderr are piped.
   - `kill_on_drop` is set.
   - `GIT_PROTOCOL` is passed through when the client sent it.
5. Relays data:

   ```
   client ──SSH data──▶ SshSession::data ──unbounded mpsc──▶ task ──▶ git stdin
   client ◀─SSH data─── handle.data      ◀── forward() ◀───────────── git stdout
   client ◀─ext. data── handle.extended_data(1) ◀── forward() ◀────── git stderr
   client EOF ─────────▶ channel_eof drops the sender ──────────────▶ git stdin closed
   ```

6. After git exits, sends git's exit status, then EOF, then closes the channel.

**Why the `Channel` object is dropped.** `channel_open_session` drops russh's `Channel` handle on purpose and receives data through the `data` callback instead. Holding the handle without reading from it fills russh's per-channel buffer and deadlocks the session. The trade-off is that the queue to git's stdin is unbounded (see [Security](security.md#known-gaps)).

### Protecting `refs/pull`

Every repository is created with `receive.hideRefs=refs/pull`. `git receive-pack` then neither advertises `refs/pull/*` nor accepts updates to it: a push there fails with `deny updating a hidden ref`. `upload-pack` still advertises these refs, so clients can fetch them.

## After a push

When `receive-pack` exits with status 0, two things happen before the exit status is sent:

1. **`sync::after_push`** runs:
   - `db::touch_pushed` sets `pushed_at`.
   - `git::ensure_head` makes sure `HEAD` names an existing branch. It prefers the current default branch, then `main`, then `master`, then the first branch alphabetically. If it had to pick a different branch, `repositories.default_branch` is updated. That's how a repository's first push of a non-`main` branch becomes its default.
   - `sync::refresh_pulls` re-evaluates every open pull request in the repository. It records new heads and marks pull requests as merged when their head is now in the base (see [Pull requests](pull-requests.md#tracking-open-pull-requests)).
2. **Push hints** (`push_hints`): for each non-default branch whose tip changed in this push, Openhub prints one of the following on stderr. The client shows them prefixed with `remote:`.

   ```
   Create a pull request for 'feature' on Openhub by visiting:
        https://git.example.com/alice/project/compare/main...feature
   ```

   ```
   View pull request for 'feature':
        https://git.example.com/alice/project/pull/3
   ```

   Deleted branches and the default branch get no hint.

Errors in `after_push` are logged and don't fail the push, which by then has already been accepted.

## Smart HTTP (read-only)

Routes, with `{repo}` accepting an optional `.git` suffix:

| Request | Result |
| --- | --- |
| `GET /{owner}/{repo}/info/refs?service=git-upload-pack` | Ref advertisement from `git upload-pack --stateless-rpc --advertise-refs`. Unless protocol v2 was requested, it's prefixed with the `# service=git-upload-pack` pkt-line and a flush packet |
| `POST /{owner}/{repo}/git-upload-pack` | Request body (gzip decoded if `Content-Encoding: gzip`) piped to `git upload-pack --stateless-rpc`; the output is streamed back |
| `GET …/info/refs?service=git-receive-pack` | 403: "Pushing over HTTP is not supported. Add an SSH key in your Openhub settings and push over SSH." |
| `POST …/git-receive-pack` | 403 |
| `GET …/info/refs` with no `service` (dumb HTTP) | 403: only the smart protocol is supported |

Details:

- **Authorisation** (`git_http::authorize`):
  - A missing repository gets 404.
  - A public repository needs no credentials.
  - A private repository needs Basic auth. Missing or wrong credentials get 401 with `WWW-Authenticate: Basic realm="Openhub"`. Valid credentials without read access get 404.
- **Protocol v2.** The `Git-Protocol` header is passed through as `GIT_PROTOCOL` when it contains only `[A-Za-z0-9=:.]`.
- **Body limit.** The `git-upload-pack` route accepts request bodies up to 64 MB (`GIT_BODY_LIMIT` in `src/web/mod.rs`). Negotiation for large repositories lists many commits. Other routes keep axum's 2 MB default.
- **Middleware.** These paths skip the session lookup and the cross-origin check, because git clients send neither cookies nor `Origin` (see `ctx::is_git_request`).
- **Errors.** git's stderr is discarded. Failures are logged and returned as 500 with a short message.

## Archive downloads

`GET /{owner}/{repo}/archive/{ref}.zip` or `.tar.gz` (`code::archive`):

1. Checks read access with `load_repo`.
2. Resolves `{ref}` with `git::resolve`, which accepts a branch, tag or commit SHA. The ref must not include a path.
3. Runs `git --git-dir <path> archive --format=zip|tar.gz --prefix=<repo>-<ref>/ <commit-sha>` and streams stdout to the client as `attachment; filename="<repo>-<ref>.<ext>"`. Any `/` in the ref becomes `-`.

`git archive` honours the repository's own `.gitattributes`, including `export-ignore` and `export-subst`. See [Security](security.md) for why that matters.

## Protocol features

The installed git provides the protocol support:
- Protocol v0, v1 and v2.
- Shallow clones (`--depth`).
- Side-band progress.
- Atomic pushes.

Openhub doesn't set `uploadpack.allowFilter`, so partial clone filters (`--filter=blob:none`) are ignored by the server. It doesn't install hooks, and nothing in the protocol can write a repository's config or hooks. Git LFS isn't supported.

## When you change this

- **New SSH command:** add it to `Service` and `parse_command`, together with its unit test. Decide which `Access` level it needs. Extend the smoke test.
- **New post-push behaviour:** put it in `sync::after_push`, so web merges and pushes share it where it applies.
- **New git configuration for hosted repositories:** add it to `git::configure`. Existing repositories don't pick it up automatically, so add a startup step if they need it.
- **Changes to HTTP authentication:** keep private repositories indistinguishable from missing ones, and keep pushes on SSH unless that is decided otherwise.
