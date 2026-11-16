#!/usr/bin/env bash
# End-to-end smoke test: starts a throwaway Openhub instance and exercises
# sign up, SSH keys, pushing, browsing, pull requests, merging and permissions.
#
# Usage: scripts/smoke-test.sh [path/to/openhub-binary]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${1:-$ROOT/target/debug/openhub}"
HTTP_PORT="${HTTP_PORT:-38080}"
SSH_PORT="${SSH_PORT:-38022}"
BASE="http://127.0.0.1:$HTTP_PORT"

if [[ ! -x "$BIN" ]]; then
  (cd "$ROOT" && cargo build --quiet)
fi

WORK="$(mktemp -d)"
SERVER_PID=""
cleanup() {
  [[ -n "$SERVER_PID" ]] && kill "$SERVER_PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

pass=0
ok() { pass=$((pass + 1)); printf '  \033[32m✓\033[0m %s\n' "$1"; }
fail() {
  printf '  \033[31m✗ %s\033[0m\n' "$1"
  echo "--- server log ---"
  tail -n 30 "$WORK/server.log" || true
  exit 1
}
expect_status() { # expect_status <code> <description> <curl args...>
  local want="$1" desc="$2"
  shift 2
  local got
  got="$(curl -s -o /dev/null -w '%{http_code}' "$@")"
  [[ "$got" == "$want" ]] && ok "$desc" || fail "$desc (expected $want, got $got)"
}
expect_body() { # expect_body <pattern> <description> <curl args...>
  local pattern="$1" desc="$2"
  shift 2
  curl -s "$@" | grep -q -- "$pattern" && ok "$desc" || fail "$desc (missing '$pattern')"
}

echo "Starting Openhub in $WORK"
"$BIN" --data-dir "$WORK/data" --http-addr "127.0.0.1:$HTTP_PORT" --ssh-addr "127.0.0.1:$SSH_PORT" \
  --public-url "$BASE" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 50); do
  curl -s -o /dev/null "$BASE/" && break
  sleep 0.2
done
curl -s -o /dev/null "$BASE/" || fail "server did not start"

ssh-keygen -q -t ed25519 -N "" -C alice@test -f "$WORK/alice_key"
ssh-keygen -q -t ed25519 -N "" -C bob@test -f "$WORK/bob_key"
ssh_cmd() { echo "ssh -i $WORK/$1_key -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"; }
A_COOKIES="$WORK/alice.cookies"
B_COOKIES="$WORK/bob.cookies"
REMOTE="ssh://git@127.0.0.1:$SSH_PORT"

echo "Accounts and keys"
expect_status 303 "register alice (first user)" -c "$A_COOKIES" -X POST \
  -d "username=alice&email=alice@example.com&password=correct-horse" "$BASE/register"
expect_status 303 "register bob" -c "$B_COOKIES" -X POST \
  -d "username=bob&email=bob@example.com&password=correct-horse" "$BASE/register"
expect_status 303 "alice adds SSH key" -b "$A_COOKIES" -X POST \
  --data-urlencode "key=$(cat "$WORK/alice_key.pub")" "$BASE/settings/keys"
expect_status 303 "bob adds SSH key" -b "$B_COOKIES" -X POST \
  --data-urlencode "key=$(cat "$WORK/bob_key.pub")" "$BASE/settings/keys"
expect_body "already in use" "the same key cannot be added twice" -b "$B_COOKIES" -X POST \
  --data-urlencode "key=$(cat "$WORK/alice_key.pub")" "$BASE/settings/keys"
expect_body "Incorrect username or password" "wrong password is rejected" -X POST \
  -d "login=alice&password=nope" "$BASE/login"
greeting="$($(ssh_cmd alice) -T -p "$SSH_PORT" git@127.0.0.1 2>&1 || true)"
[[ "$greeting" == *"Hi alice!"* ]] && ok "ssh -T greets alice" || fail "ssh -T greeting"

echo "Repositories and pushing"
expect_status 303 "create public repo" -b "$A_COOKIES" -X POST -d "name=demo&visibility=public" "$BASE/new"
expect_status 303 "create private repo with README" -b "$A_COOKIES" -X POST \
  -d "name=secret&visibility=private&readme=1" "$BASE/new"
git init -q -b main "$WORK/demo"
cd "$WORK/demo"
git config user.name "Alice" && git config user.email alice@example.com
printf '# Demo\n\nHello from **Openhub**.\n' >README.md
mkdir -p src && printf 'fn main() {\n    println!("hi");\n}\n' >src/main.rs
git add -A && git commit -qm "Initial commit"
git remote add origin "$REMOTE/alice/demo.git"
GIT_SSH_COMMAND="$(ssh_cmd alice)" git push -q -u origin main 2>/dev/null && ok "push over SSH" || fail "push over SSH"

expect_body "Hello from <strong>Openhub</strong>" "README renders on repo page" "$BASE/alice/demo"
expect_body 'tok-k' "source files are syntax highlighted" "$BASE/alice/demo/blob/main/src/main.rs"
expect_body "Initial commit" "commit history lists the commit" "$BASE/alice/demo/commits/main"
expect_status 200 "raw file download" "$BASE/alice/demo/raw/main/README.md"
expect_status 200 "zip archive download" "$BASE/alice/demo/archive/main.zip"

echo "Pull requests"
git checkout -q -b feature
printf 'pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n' >src/lib.rs
git add -A && git commit -qm "Add library"
GIT_SSH_COMMAND="$(ssh_cmd alice)" git push -u origin feature 2>"$WORK/push.txt" >/dev/null
grep -q "compare/main...feature" "$WORK/push.txt" && ok "push suggests opening a pull request" || fail "push hint"
expect_status 303 "open pull request" -b "$A_COOKIES" -X POST \
  -d "base=main&head=feature&title=Add+library&body=Adds+add()" "$BASE/alice/demo/pulls"
expect_body "no conflicts" "pull request is mergeable" -b "$A_COOKIES" "$BASE/alice/demo/pull/1"
expect_body "src/lib.rs" "files changed tab shows the diff" "$BASE/alice/demo/pull/1/files"
expect_status 303 "bob comments" -b "$B_COOKIES" -X POST -d "body=Nice+work" "$BASE/alice/demo/pull/1/comment"
expect_status 403 "bob cannot merge without write access" -b "$B_COOKIES" -X POST -d "method=merge" \
  "$BASE/alice/demo/pull/1/merge"
expect_status 303 "alice merges" -b "$A_COOKIES" -X POST -d "method=merge" "$BASE/alice/demo/pull/1/merge"
expect_body "successfully merged" "pull request shows as merged" -b "$A_COOKIES" "$BASE/alice/demo/pull/1"
GIT_SSH_COMMAND="$(ssh_cmd alice)" git fetch -q origin
git log --format=%s origin/main | grep -q "Merge pull request #1" && ok "merge commit landed on main" || fail "merge commit"

git checkout -q -b cli origin/main
echo "x" >cli.txt && git add -A && git commit -qm "CLI change"
GIT_SSH_COMMAND="$(ssh_cmd alice)" git push -q -u origin cli 2>/dev/null
expect_status 303 "open second pull request" -b "$A_COOKIES" -X POST -d "base=main&head=cli&title=CLI" "$BASE/alice/demo/pulls"
git checkout -q -B main origin/main && git merge -q --no-ff cli -m "Merge cli locally"
GIT_SSH_COMMAND="$(ssh_cmd alice)" git push -q origin main 2>/dev/null
expect_body "state-merged" "pull request merged on the command line is detected" -b "$A_COOKIES" "$BASE/alice/demo/pull/2"

echo "Issues"
expect_status 303 "alice opens an issue" -b "$A_COOKIES" -X POST \
  -d "title=Bug+in+add&body=It+adds+wrong&labels=bug,+help+wanted" "$BASE/alice/demo/issues"
expect_body "Bug in add" "issue appears in the list" "$BASE/alice/demo/issues"
expect_body "It adds wrong" "issue page shows the body" "$BASE/alice/demo/issue/1"
expect_body "help wanted" "issue shows its labels" "$BASE/alice/demo/issue/1"
expect_body "help wanted" "labels appear in the list" "$BASE/alice/demo/issues"
expect_body "Bug in add" "labels filter the list" "$BASE/alice/demo/issues?label=bug"
expect_status 200 "issue numbers share the pull request space" "$BASE/alice/demo/issue/1"
expect_status 404 "missing issue is a 404" "$BASE/alice/demo/issue/99"
expect_status 303 "bob comments on the issue" -b "$B_COOKIES" -X POST -d "body=I+can+reproduce" "$BASE/alice/demo/issue/1/comment"
expect_body "I can reproduce" "comment appears on the timeline" "$BASE/alice/demo/issue/1"
expect_status 403 "bob cannot close an issue he did not open" -b "$B_COOKIES" -X POST \
  -d "action=close&reason=completed" "$BASE/alice/demo/issue/1/state"
expect_status 303 "alice closes the issue as completed" -b "$A_COOKIES" -X POST \
  -d "action=close&reason=completed" "$BASE/alice/demo/issue/1/state"
expect_body "closed this as" "closed issue shows its state" "$BASE/alice/demo/issue/1"
expect_body "state-resolved" "issue closed as completed shows the resolved pill" "$BASE/alice/demo/issue/1"
expect_status 303 "alice reopens the issue" -b "$A_COOKIES" -X POST -d "action=reopen" "$BASE/alice/demo/issue/1/state"
expect_body "reopened this" "reopened issue is open again" "$BASE/alice/demo/issue/1"
expect_status 303 "bob opens his own issue" -b "$B_COOKIES" -X POST \
  -d "title=Bobs+idea&body=Suggest+something" "$BASE/alice/demo/issues"
expect_status 303 "bob closes his own issue with a comment" -b "$B_COOKIES" -X POST \
  -d "body=Never+mind&close=1&reason=not_planned" "$BASE/alice/demo/issue/2/comment"
expect_body "not planned" "issue closed as not planned" "$BASE/alice/demo/issue/2"
expect_body "state-not-planned" "not planned issue shows the grey pill" "$BASE/alice/demo/issue/2"
expect_body "Bobs idea" "closed issues can be listed" "$BASE/alice/demo/issues?state=closed"
expect_status 303 "anonymous users are sent to sign in to open an issue" -X POST -d "title=x" "$BASE/alice/demo/issues"
expect_status 404 "anonymous users cannot see a private repo's issues" "$BASE/alice/secret/issues"

echo "Permissions"
expect_status 404 "private repo hidden from anonymous users" "$BASE/alice/secret"
expect_status 404 "private repo hidden from non-collaborators" -b "$B_COOKIES" "$BASE/alice/secret"
GIT_SSH_COMMAND="$(ssh_cmd bob)" git clone -q "$REMOTE/alice/secret.git" "$WORK/nope" 2>/dev/null &&
  fail "bob cloned a private repo" || ok "bob cannot clone private repo over SSH"
cd "$WORK/demo"
GIT_SSH_COMMAND="$(ssh_cmd bob)" git push -q origin HEAD:refs/heads/bob 2>/dev/null &&
  fail "bob pushed without access" || ok "bob cannot push to alice/demo"
expect_status 303 "alice adds bob as collaborator" -b "$A_COOKIES" -X POST -d "username=bob" \
  "$BASE/alice/secret/settings/collaborators"
GIT_SSH_COMMAND="$(ssh_cmd bob)" git clone -q "$REMOTE/alice/secret.git" "$WORK/secret" 2>/dev/null &&
  ok "collaborator can clone private repo" || fail "collaborator clone"
GIT_SSH_COMMAND="$(ssh_cmd alice)" git push -q origin HEAD:refs/pull/1/head 2>/dev/null &&
  fail "pushed to refs/pull" || ok "refs/pull/* cannot be pushed to"

echo "Git over HTTP"
git clone -q "$BASE/alice/demo.git" "$WORK/http-clone" && ok "anonymous HTTP clone of public repo" || fail "HTTP clone"
GIT_TERMINAL_PROMPT=0 git clone -q "$BASE/alice/secret.git" "$WORK/http-secret" 2>/dev/null &&
  fail "anonymous HTTP clone of private repo" || ok "private repo requires authentication over HTTP"
git clone -q "http://bob:correct-horse@127.0.0.1:$HTTP_PORT/alice/secret.git" "$WORK/http-secret2" &&
  ok "collaborator HTTP clone with password" || fail "authenticated HTTP clone"
cd "$WORK/http-clone"
git -c user.name=x -c user.email=x@x commit -q --allow-empty -m "nope"
git push -q origin main 2>/dev/null && fail "push over HTTP" || ok "push over HTTP is refused"

echo
printf '\033[32mAll %d checks passed.\033[0m\n' "$pass"
