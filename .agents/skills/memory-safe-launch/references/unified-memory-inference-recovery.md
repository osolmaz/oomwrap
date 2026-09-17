# Unified-memory inference recovery

Use this runbook after a large local inference launch fails on a machine where
the GPU and CPU share memory, such as NVIDIA GB10. Collect evidence before a GPU
reset or reboot. Driver allocation warnings and process watchdog events can
occur during the same launch.

## Collect evidence

Do not stop a process that belongs to another agent or user. First identify the
failed launch and confirm that you are authorized to stop any remaining process
from that exact launch.

Record:

```bash
free -h
swapon --show
nvidia-smi
ps -eo pid,ppid,stat,etime,rss,cmd --sort=-rss | head -30
sudo fuser -v /dev/nvidia* 2>&1 || true
systemctl is-active earlyoom.service || true
pgrep -a earlyoom || true
```

Match failure timestamps across the server and system logs:

```bash
journalctl -k --since '10 minutes ago' --no-pager \
  | grep -E 'NVRM|Out of memory|oom-kill|Killed process'
journalctl -u earlyoom --since '10 minutes ago' --no-pager
```

`nvidia-smi` can report `Not Supported` for memory totals on a unified-memory
system. Use `MemAvailable`, free swap, process RSS, server logs, oomwrap events,
and watchdog logs together.

An `NVRM: ... NV_ERR_NO_MEMORY` line does not prove that the driver terminated
the launch. Some loaders continue after a failed allocation. An oomwrap pressure
event proves that oomwrap stopped its process group. A matching `earlyoom` line
proves a separate machine-wide watchdog action.

## Loading peaks and earlyoom

A quantized model load can use much more memory than the ready server. Weight
repacking is one example. Do not disable protection to pass this peak.

1. Choose oomwrap floors from the measured machine budget.
2. Confirm that machine-wide `earlyoom` is active.
3. Start the server with oomwrap and the same recorded floors.
4. Watch available RAM, free swap, the server log, and oomwrap events.
5. Stop on pressure or a driver allocation failure.
6. After readiness, confirm that steady-state headroom remains above the normal
   workstation safety threshold.

If the operator explicitly approves a bounded temporary `earlyoom` policy for a
loading peak, keep a watchdog active throughout the change and restore the
normal policy on success, failure, interruption, and timeout. Do not pass
`--allow-no-earlyoom` for a large model load.

## GPU reset approval

A GPU reset can close the graphical session because the display server may use
the GPU. Explain the impact and obtain explicit approval before stopping the
display stack or resetting the GPU.

Before an approved reset, stop only the failed launch processes that the user
authorized you to stop. Inspect device users with:

```bash
sudo fuser -v /dev/nvidia* 2>&1 || true
```

If the display stack and NVIDIA persistence are the only remaining users, this
pattern restores both services when the reset succeeds or fails:

```bash
sudo bash -c '
set -Eeuo pipefail
restore() {
  systemctl start nvidia-persistenced.service || true
  systemctl start display-manager.service || true
}
trap restore EXIT

systemctl stop display-manager.service
systemctl stop nvidia-persistenced.service
sleep 5
fuser -v /dev/nvidia* 2>&1 || true
nvidia-smi --gpu-reset -i 0

restore
trap - EXIT
'
```

Change the GPU index when the target is not GPU 0. Do not reset while an
unapproved process uses the device. Some systems do not support a reset while a
primary display is attached.

## Verify recovery

Run:

```bash
nvidia-smi
systemctl is-active nvidia-persistenced.service
systemctl is-active display-manager.service
systemctl is-active earlyoom.service
sudo fuser -v /dev/nvidia* 2>&1 || true
free -h
```

Wait for the graphical stack and memory accounting to settle. Start the next
attempt through oomwrap and repeat the staged launch checks. A successful reset
only proves that the reset completed.

Request reboot approval only when reset is unsupported, the driver remains
unhealthy, or a clean guarded launch still fails at the driver level without a
process-stop event. Preserve useful outputs and logs before recovery.
