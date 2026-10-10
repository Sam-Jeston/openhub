# Operations

This page covers running an Openhub instance day to day: first start, reverse proxies, backups, upgrades, administration without a UI, and troubleshooting. The [project README](../README.md) has installation steps and the full [configuration table](../README.md#configuration).

## First start checklist

1. **Set `OPENHUB_PUBLIC_URL`** to the exact URL people will use, such as `https://git.example.com`. Links, HTTP clone URLs, push hints, the cross-origin check and the cookie `Secure` flag all depend on it.
2. **Set the SSH host and port** with `OPENHUB_SSH_HOST` and `OPENHUB_SSH_PORT` if clients reach SSH on a different host or port from the one Openhub listens on. Docker port mapping is the usual reason.
3. **Start the server and register the administrator account straight away.** The first account becomes site administrator (see [Security](security.md#known-gaps)).
4. **Add an SSH key** in **Settings → SSH keys**, then check it with the command shown there (`ssh -T -p 2222 git@host`).
5. **Set `OPENHUB_DISABLE_REGISTRATION=true`** once everyone who needs an account has one, unless you intend to run an open instance.
6. **Note the SSH host key fingerprint** from the startup log (`SSH host key fingerprint: SHA256:…`) so users can verify it on first connect.

## Logs

Openhub logs to stdout through `tracing`. Set the level with `RUST_LOG`:

| `RUST_LOG` | Shows |
| --- | --- |
| unset (`openhub=info`) | Startup, SSH sign-ins, git commands run over SSH, and errors |
| `openhub=debug` | Plus SSH session errors |
| `openhub=info,tower_http=debug` | Plus one line for every HTTP request and response |

Internal errors (HTTP 500) are logged with their full error chain. The user only sees a generic message.

## Reverse proxy

Openhub speaks plain HTTP. Put a TLS-terminating proxy in front of it for the web port only; SSH goes straight to the SSH port.

**Caddy:**

```
git.example.com {
    reverse_proxy 127.0.0.1:3000
}
```

**nginx:**

```nginx
server {
    listen 443 ssl;
    server_name git.example.com;
    # ssl_certificate …

    client_max_body_size 64m;          # git fetch negotiation for large repositories
    location / {
        proxy_pass http://127.0.0.1:3000;
        proxy_set_header Host $host;
        proxy_buffering off;           # stream clones and archive downloads
    }
}
```

Then set `OPENHUB_PUBLIC_URL=https://git.example.com`. The cross-origin check accepts form posts whose `Origin` matches either the public URL or the `Host` header, so proxies that rewrite `Host` still work.

If you rate-limit at the proxy (recommended, see [Security](security.md#hardening-advice)), cover `POST /login`, `POST /register` and `/*/*/info/refs`.

## Backups

Everything lives in the data directory (see [Architecture](architecture.md#data-directory)):

| Item | How to back it up |
| --- | --- |
| `openhub.db` | While running: `sqlite3 openhub.db ".backup backup.db"`, which takes a consistent snapshot. When stopped: copy the file. Don't copy a live database file without its `-wal` file |
| `repos/` | Copying bare repositories while a push is in progress can capture a half-written state. Stop the server, use a filesystem snapshot (ZFS, Btrfs or LVM), or accept that the latest push may need repeating |
| `ssh_host_ed25519_key` | Copy it. Without it, every client warns about a changed host key after a restore |

**To restore:** stop Openhub, put all three items back in one data directory, and start it again. The database and `repos/` must come from the same point in time. A repository row without its directory, or a directory without its row, causes errors (see [Data model → Invariants](data-model.md#invariants)).

## Upgrades

1. **Back up** the data directory.
2. **Build or pull the new version.** From source, run `git pull && cargo build --release`. With Docker, run `docker compose build && docker compose up -d`.
3. **Start it.** Pending database migrations run automatically at startup, before the servers listen.

Migrations only go forward. To roll back, restore the backup taken in step 1 and run the old version.

## Repository maintenance

Openhub never runs `git gc`. `git receive-pack` runs `git gc --auto` after pushes by default, which keeps most repositories tidy. To compact a repository by hand, or to clean up objects left by failed web merges:

```sh
git --git-dir data/repos/<id>.git gc
```

To map a repository to its ID:

```sh
sqlite3 -readonly data/openhub.db \
  "SELECT r.id FROM repositories r JOIN users u ON u.id = r.owner_id WHERE u.username = 'alice' AND r.name = 'project';"
```

Run maintenance as the same user that runs Openhub, so file ownership stays correct. In Docker:

```sh
docker compose exec openhub git --git-dir /data/repos/<id>.git gc
```

## Administration without a UI

Openhub has no admin pages. Do these tasks with `sqlite3` against `openhub.db`, after taking a backup. Changes take effect immediately, and the server doesn't need restarting.

```sql
-- Make someone a site administrator (or set to 0 to remove)
UPDATE users SET is_admin = 1 WHERE username = 'bob';

-- Sign a user out everywhere
DELETE FROM sessions WHERE user_id = (SELECT id FROM users WHERE username = 'bob');

-- Revoke all of a user's SSH keys (stops them pushing)
DELETE FROM ssh_keys WHERE user_id = (SELECT id FROM users WHERE username = 'bob');

-- Transfer a repository to another user (the new owner must not already have a repository with that name)
UPDATE repositories SET owner_id = (SELECT id FROM users WHERE username = 'carol')
 WHERE id = 42;
```

**Resetting a forgotten password.** There's no reset flow. You can set a new hash made by any Argon2id tool that writes the standard PHC string format. For example, with the `argon2` command-line tool (Debian package `argon2`):

```sh
printf '%s' 'new-password' | argon2 "$(openssl rand -hex 16)" -id -e
# → $argon2id$v=19$m=4096,t=3,p=1$…
```

```sql
UPDATE users SET password_hash = '$argon2id$v=19$…' WHERE username = 'bob';
```

**Deleting a user.** `DELETE FROM users …` removes their sessions, keys, repositories and collaborations, through cascades. It fails if they authored or merged pull requests, or wrote pull request events, in other users' repositories (see [Data model → Deleting rows](data-model.md#deleting-rows)). Deleting their rows also doesn't remove their repositories' directories under `repos/`. Delete each repository in the web interface first, while signed in as an administrator.

## Troubleshooting

| Symptom | Likely cause | Fix |
| --- | --- | --- |
| Forms fail with **"Cross-origin request rejected"** | The browser's origin matches neither `OPENHUB_PUBLIC_URL` nor the `Host` header the server receives | Set the public URL to exactly what's in the address bar, including scheme and port |
| Startup fails with **"could not run `git`"** | git isn't installed or isn't on `PATH` | Install git, or set `OPENHUB_GIT_BIN` |
| Startup fails with a **migration checksum or "previously applied but has been modified"** error | An already-applied file in `migrations/` was edited | Restore the original file and put the change in a new migration |
| **"database is locked"** errors | Another process held a write lock for more than 5 seconds, often a `sqlite3` shell with an open transaction | Close the other connection. Open shells with `-readonly` for inspection |
| `git push` says **"Permission denied (publickey)"** | The key isn't registered, or ssh offered a different key | Add the key in Settings. Test with `ssh -i ~/.ssh/key -o IdentitiesOnly=yes -T -p 2222 git@host` |
| `git push` says **"ERROR: Repository not found."** | The path is wrong, or the account can't read the repository | Check `owner/name` in the remote URL, and that the key belongs to the expected account (`ssh -T` greets you by username) |
| `git push` says **"You do not have permission to push to …"** | The account can read but isn't the owner or a collaborator | The owner adds the account under repository **Settings → Collaborators** |
| ssh warns **"REMOTE HOST IDENTIFICATION HAS CHANGED"** | `ssh_host_ed25519_key` was lost or replaced | Restore the key from backup. If it really is gone, users remove the old entry with `ssh-keygen -R '[host]:2222'` |
| Push to `refs/pull/…` fails with **"deny updating a hidden ref"** | Expected: Openhub manages those refs | Push to a branch instead |
| `git push` over HTTP fails with **403** | HTTP is read-only by design | Use the SSH remote URL |
| Clone URLs show **localhost** or the wrong port | `OPENHUB_PUBLIC_URL`, `OPENHUB_SSH_HOST` or `OPENHUB_SSH_PORT` isn't set | Set them and restart |
| Large HTTP clones fail with **413** | A proxy limits request bodies below the 64 MB Openhub allows for fetch negotiation | Raise the proxy limit (`client_max_body_size` in nginx) |
| A repository page shows **"Something went wrong"** | An internal error, such as a missing `repos/<id>.git` directory | Check the log for `internal error:`. Restore the directory from backup, or delete the repository |
| A new repository unexpectedly shows **old content** | A previous repository with the same ID left its directory behind (deletion logs `could not remove`) | Delete the new repository, remove the stale `repos/<id>.git`, then create it again |
