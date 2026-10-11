#!/usr/bin/env bash
# Builds openhub on a Linux machine and installs or updates it there as a
# systemd service (packaging/linux), over ssh. It builds on the target instead
# of cross-compiling, because libgit2 and SQLite are compiled from C. The
# target needs a C compiler, curl and git. If it has no cargo, the first run
# installs Rust there with rustup. The ssh user needs sudo.
#
#   scripts/deploy-server.sh user@host
#
# Uncommitted changes are deployed too. The source and build output are kept on
# the target in ~/.cache/openhub-build, so later builds only recompile openhub.
set -euo pipefail

host=${1:?usage: scripts/deploy-server.sh user@host}
cd "$(dirname "$0")/.."
version=$(git describe --tags --always --dirty 2>/dev/null || echo dev)
build=.cache/openhub-build # relative to the remote home directory

echo "Uploading openhub $version to $host:~/$build/src"
# Tracked files plus untracked ones that aren't ignored, skipping deleted files.
# tar -m stamps them with the extraction time, so cargo always rebuilds openhub
# itself (whatever the clock skew between the machines) and reuses dependencies.
git ls-files -z --cached --others --exclude-standard |
  while IFS= read -r -d '' f; do if [ -f "$f" ]; then printf '%s\0' "$f"; fi; done |
  COPYFILE_DISABLE=1 tar --null -T - -czf - |
  ssh "$host" "rm -rf $build/src && mkdir -p $build/src && tar -xzmf - -C $build/src"

echo "Building on $host (the first build takes several minutes)"
ssh "$host" bash -s -- "$build" <<'EOF'
set -euo pipefail
build=$1
export PATH=$HOME/.cargo/bin:$PATH
if ! command -v cargo >/dev/null; then
  echo "Installing Rust with rustup"
  curl -sSf --proto '=https' --tlsv1.2 https://sh.rustup.rs | sh -s -- -y --profile minimal --no-modify-path
fi
cd "$build/src"
CARGO_TARGET_DIR=$HOME/$build/target cargo build --release --locked
EOF

# A terminal lets sudo ask for a password.
tty=
[ -t 0 ] && tty=-t
ssh $tty "$host" sudo bash "$build/src/packaging/linux/install.sh" "$build/target/release/openhub" "$build/src/packaging/linux/openhub.service"
