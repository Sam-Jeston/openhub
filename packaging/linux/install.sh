#!/usr/bin/env bash
# Installs or updates openhub as a systemd service. Run it as root on the
# target machine (scripts/deploy-server.sh builds openhub there and runs this
# for you):
#
#   sudo ./install.sh ./openhub [./openhub.service]
#
# Each run stops the service, snapshots the database into backups/ in the data
# directory (keeping the last 5), swaps the binary (the old one stays as
# openhub.prev) and starts the service again. If the new binary fails its
# health check, the previous one is put back. Repositories aren't snapshotted:
# upgrades only migrate the database.
set -euo pipefail

bin=${1:?usage: install.sh BINARY [UNIT_FILE]}
unit=${2:-}
dest=/usr/local/bin/openhub
unit_dest=/etc/systemd/system/openhub.service
defaults=/etc/default/openhub
keep_backups=5

[ "$(id -u)" = 0 ] || { echo "install.sh must run as root, e.g. with sudo." >&2; exit 1; }

# setting NAME DEFAULT: the value the service sees, honouring /etc/default/openhub.
setting() {
  (
    [ -f "$defaults" ] && . "$defaults"
    printf '%s\n' "${!1:-$2}"
  )
}

# put SRC: replace the installed binary without writing into a running executable.
put() {
  install -m 0755 "$1" "$dest.new"
  mv -f "$dest.new" "$dest"
}

# get URL: succeeds when URL answers without an HTTP error.
get() {
  if command -v curl >/dev/null; then
    curl -fs --max-time 2 -o /dev/null "$1"
  else
    wget -q -T 2 -O /dev/null "$1"
  fi
}

# Refuse a binary built for another architecture before touching the service.
new_version=$("$bin" --version) || { echo "$bin doesn't run on this machine. Build it for $(uname -m)." >&2; exit 1; }
git_bin=$(setting OPENHUB_GIT_BIN git)
command -v "$git_bin" >/dev/null || { echo "openhub needs git at runtime and can't find $git_bin. Install git first." >&2; exit 1; }

data=$(setting OPENHUB_DATA_DIR /var/lib/openhub)
db=$data/openhub.db
addr=$(setting OPENHUB_HTTP_ADDR 127.0.0.1:3000)
case $addr in
  0.0.0.0:*) addr=127.0.0.1:${addr#*:} ;;
  "[::]":*) addr=[::1]:${addr##*:} ;;
esac
url=http://$addr/

id openhub >/dev/null 2>&1 || useradd --system --user-group --home-dir /var/lib/openhub --no-create-home --shell /usr/sbin/nologin openhub

if [ ! -f "$defaults" ]; then
  cat >"$defaults" <<'EOF'
# Settings for the openhub service; see "Configuration" in the openhub README.
# Restart after editing: sudo systemctl restart openhub
#OPENHUB_PUBLIC_URL=https://git.example.com
#OPENHUB_SSH_HOST=git.example.com
#OPENHUB_SSH_PORT=2222
#OPENHUB_DISABLE_REGISTRATION=true
EOF
  echo "Wrote $defaults. Set OPENHUB_PUBLIC_URL there, then run: sudo systemctl restart openhub"
fi

if [ -n "$unit" ] && ! cmp -s "$unit" "$unit_dest"; then
  install -m 0644 "$unit" "$unit_dest"
  systemctl daemon-reload
fi
[ -f "$unit_dest" ] || { echo "No openhub unit is installed yet. Pass the unit file as the second argument." >&2; exit 1; }

systemctl stop openhub 2>/dev/null || true

snap=
if [ -f "$db" ]; then
  backups=$data/backups
  snap=$backups/$(date +%Y%m%d-%H%M%S)
  mkdir -p "$snap"
  # The service is stopped, so the files are consistent; -wal/-shm only exist after an unclean stop.
  for f in "$db" "$db-wal" "$db-shm"; do
    if [ -f "$f" ]; then cp -p "$f" "$snap/"; fi
  done
  chown -R openhub:openhub "$backups"
  ls -1d "$backups"/*/ | head -n -"$keep_backups" | xargs -r rm -rf --
  echo "Database snapshot: $snap"
fi

if [ -f "$dest" ]; then cp -p "$dest" "$dest.prev"; fi
put "$bin"
systemctl enable --quiet openhub
started=$(date +%s)
systemctl start openhub

for _ in $(seq 20); do
  sleep 1
  if get "$url"; then
    echo "$new_version is running ($url)."
    fingerprint=$(journalctl -u openhub --since "@$started" -o cat --no-pager 2>/dev/null | grep -o 'SHA256:[A-Za-z0-9+/]*' | head -n 1 || true)
    if [ -n "$fingerprint" ]; then echo "SSH host key fingerprint: $fingerprint"; fi
    echo "Settings: $defaults"
    echo "Logs: journalctl -u openhub -f"
    exit 0
  fi
done

echo "$new_version failed its health check at $url." >&2
journalctl -u openhub -n 20 --no-pager >&2 || true
if [ -f "$dest.prev" ]; then
  put "$dest.prev"
  systemctl restart openhub
  echo "Restored the previous binary ($("$dest" --version))." >&2
  if [ -n "$snap" ]; then echo "If it can't open the database, stop the service and copy $snap back to $data." >&2; fi
fi
exit 1
