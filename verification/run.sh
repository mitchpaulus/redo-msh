#!/bin/sh
# Run every TLC model check for redo-msh's formal specs.
#
# Requires: Java 11+ and tla2tools.jar (default: ~/.local/share/java/,
# override with TLA_TOOLS_JAR). TLC scratch state goes under /tmp, NOT the
# repo: on WSL the repo lives on a drvfs mount, which is slow for the many
# small files TLC writes.
#
# Each line of output is PASS or FAIL per (spec, config). One config —
# CycleLock_CycleParallel — documents a known gap and PASSES when TLC finds
# the predicted deadlock (see README.md).
set -u
cd "$(dirname "$0")"

JAR="${TLA_TOOLS_JAR:-$HOME/.local/share/java/tla2tools.jar}"
META="${TMPDIR:-/tmp}/redo-msh-tlc"
mkdir -p "$META"
FAILED=0

run() { # run <Module.tla> <Config.cfg> <expect: ok|deadlock>
    name="$(basename "$2" .cfg)"
    out="$META/$name.out"
    java -XX:+UseParallelGC -cp "$JAR" tlc2.TLC \
        -workers auto -metadir "$META/$name" -config "$2" "$1" \
        >"$out" 2>&1
    rc=$?
    case "$3" in
    ok)
        if [ "$rc" -eq 0 ]; then
            echo "PASS  $name"
        else
            echo "FAIL  $name (see $out)"
            FAILED=1
        fi
        ;;
    deadlock)
        if grep -q "Deadlock reached" "$out"; then
            echo "PASS  $name (deadlock found, as predicted)"
        else
            echo "FAIL  $name (expected a deadlock; see $out)"
            FAILED=1
        fi
        ;;
    violation)
        if grep -q "Invariant .* is violated" "$out"; then
            echo "PASS  $name (invariant violated, as predicted)"
        else
            echo "FAIL  $name (expected an invariant violation; see $out)"
            FAILED=1
        fi
        ;;
    esac
}

run TokenPool.tla TokenPool_J2.cfg ok
run TokenPool.tla TokenPool_J3.cfg ok
run LockSession.tla LockSession.cfg ok
run CycleLock.tla CycleLock_Diamond.cfg ok
run CycleLock.tla CycleLock_CycleSerial.cfg ok
run CycleLock.tla CycleLock_CycleParallel.cfg deadlock
run ParallelEnsure.tla ParallelEnsure_Diamond.cfg ok
run ParallelEnsure.tla ParallelEnsure_DiamondJ1.cfg ok
run ParallelEnsure.tla ParallelEnsure_Wide.cfg ok
run ParallelEnsure.tla ParallelEnsure_Cycle.cfg ok
run ParallelEnsure.tla ParallelEnsure_Coverage.cfg violation
run Speculation.tla Speculation_Diamond.cfg ok
run Speculation.tla Speculation_Stale.cfg ok
run Speculation.tla Speculation_StaleCycle.cfg ok
run Speculation.tla Speculation_TrueCycle.cfg ok

exit "$FAILED"
