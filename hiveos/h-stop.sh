#!/usr/bin/env bash
# HiveOS custom-miner STOP hook for csdpool.
#
# HiveOS stops a custom miner with `screen -S <pid>.miner -X quit`, which kills
# only the FOREGROUND process the screen exec'd — our csd-gpu-miner. But h-run.sh
# launches two helpers in the BACKGROUND with `&` *before* the exec, so they get
# reparented to init and SURVIVE the screen quit:
#   - csd-relay-node            the SP2 canonical-anchor relay (holds :18644/:18645)
#   - the auto-update sidecar   (bash -c hive_update_sidecar csd-hive-update-sidecar)
#
# HiveOS delegates cleanup of such children to THIS script (it runs h-stop.sh on
# stop if the miner ships one). Without it the relay orphans — ports held, disk
# I/O on a rig HiveOS believes is idle — and the sidecar keeps polling GitHub and
# can `pkill` a later miner. Reap both here.
#
# Match by the UNIQUE sidecar marker and the relay binary name. NEVER
# `pkill -f h-run.sh` (it would match unrelated processes / this very script's
# call graph). The miner itself is already gone via `screen -X quit`; the final
# `pkill -x csd-gpu-miner` is belt-and-suspenders for a stuck instance.
pkill -f csd-hive-update-sidecar 2>/dev/null || true   # the auto-update sidecar (unique marker)
pkill -f csd-relay-node          2>/dev/null || true   # the SP2 relay
pkill -x csd-gpu-miner           2>/dev/null || true   # the miner (usually already stopped)
exit 0
