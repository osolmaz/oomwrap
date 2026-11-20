#!/usr/bin/env bash
# Starts a user manager for the runner user on a throwaway CI runner, with the
# memory controller delegated to it, so that `systemd-run --user --scope` can
# hold memory limits. It changes the system's configuration, so it runs only
# on CI.
set -euo pipefail

if [[ "${CI:-}" != "true" ]]; then
  echo "start-user-manager.sh runs only on throwaway CI runners" >&2
  exit 2
fi

uid="$(id -u)"
sudo mkdir -p /etc/systemd/system/user@.service.d
printf '[Service]\nDelegate=cpu cpuset io memory pids\n' |
  sudo tee /etc/systemd/system/user@.service.d/delegate.conf >/dev/null
sudo systemctl daemon-reload
sudo loginctl enable-linger "$(id -un)"
for _ in $(seq 100); do
  [[ -S "/run/user/$uid/bus" ]] && break
  sleep 0.1
done
test -S "/run/user/$uid/bus"
{
  echo "XDG_RUNTIME_DIR=/run/user/$uid"
  echo "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$uid/bus"
} >>"$GITHUB_ENV"

manager="/sys/fs/cgroup/user.slice/user-$uid.slice/user@$uid.service"
echo "$(systemctl --version | head -n 1), kernel $(uname -r), $(uname -m)"
echo "controllers of the user manager: $(cat "$manager/cgroup.controllers")"
