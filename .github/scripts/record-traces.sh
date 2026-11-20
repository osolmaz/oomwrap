#!/usr/bin/env bash
# Records traces with --record-readings on a throwaway CI runner: a build, a
# renderer, a memory ramp inside a scope and a memory ramp of the whole
# machine. The last one brings the runner itself under memory pressure, so
# this runs only on CI. $1 is the folder of build-suite.sh, $2 the output.
set -euo pipefail

if [[ "${CI:-}" != "true" ]]; then
  echo "record-traces.sh runs only on throwaway CI runners" >&2
  exit 2
fi

suite="$1"
out="$2"
mkdir -p "$out"
work="${RUNNER_TEMP:?}/traces-work"
mkdir -p "$work"

# record <name> <oomwrap flags...> -- <command...>
record() {
  local name="$1"
  shift
  local status=0
  "$suite/oomwrap" run --profile generic --allow-no-machine-guard \
    --event-log "$work/events.jsonl" --record-readings "$out/$name.jsonl" "$@" || status=$?
  echo "$name: exit $status" | tee -a "$out/exits.txt"
}

{
  uname -m
  uname -r
  systemctl --version | head -n 1
  free -m
  nproc
} >"$out/machine.txt"

record build --backend cgroup -- \
  cargo build --release --locked --target-dir "$work/build"

if command -v google-chrome >/dev/null; then
  cat >"$work/page.html" <<'HTML'
<!doctype html>
<html><body><script>
for (let i = 0; i < 48; i++) {
  const canvas = document.createElement("canvas");
  canvas.width = 2000;
  canvas.height = 2000;
  document.body.appendChild(canvas);
  const g = canvas.getContext("2d");
  for (let j = 0; j < 4000; j++) {
    g.fillStyle = `hsl(${(i * 37 + j) % 360}, 70%, 50%)`;
    g.beginPath();
    g.arc(Math.random() * 2000, Math.random() * 2000, 5 + Math.random() * 60, 0, 7);
    g.fill();
  }
}
</script></body></html>
HTML
  record renderer --backend cgroup -- \
    google-chrome --headless=new --no-sandbox --disable-gpu --virtual-time-budget=20000 \
    --window-size=2000,30000 --screenshot="$work/page.png" "file://$work/page.html"
fi

# The two ramps observe: their stop limits never hold, so the trace covers the
# whole run, and the replay tests apply the defaults to it.
record scope-ramp --backend cgroup --memory-max 3G --memory-high 1G --job-pressure 100% -- \
  "$suite/real_system" helper ramp 2048 60
record machine-ramp --backend poll --min-mem 0 --domain-pressure 100% -- \
  "$suite/real_system" helper ramp machine 60
