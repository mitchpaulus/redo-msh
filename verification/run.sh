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
# The overwrite prompt holds its token across the question (the release-
# and-spin alternative deadlocks; see TokenPool.tla's PromptMode).
run TokenPool.tla TokenPool_PromptHold.cfg ok
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
# Documented gap in the single-claim design: a reversed dependency edge
# makes BWait hard-fail through a speculative cedge. Must keep failing
# until nobody implements this design (SpeculationMP is the replacement).
run Speculation.tla Speculation_Reversed.cfg violation

# The corrected multi-process machine (per-process registries, kernel
# locks, drain — with typed edges, creation edges, and soft eviction).
# This is the design the implementation must be rebuilt to match.
run SpeculationMP.tla SpeculationMP_Reversed.cfg ok
run SpeculationMP.tla SpeculationMP_CrossStale.cfg ok
run SpeculationMP.tla SpeculationMP_TrueCycle.cfg ok
run SpeculationMP.tla SpeculationMP_Stale.cfg ok
run SpeculationMP.tla SpeculationMP_SpecAbort.cfg ok
# R6: hand-edited targets prompt only on demanded lineages; elsewhere the
# lineage aborts (sfail) so an upgrade can never report an unasked refusal.
run SpeculationMP.tla SpeculationMP_HandEditSpec.cfg ok
run SpeculationMP.tla SpeculationMP_HandEditYes.cfg ok
run SpeculationMP.tla SpeculationMP_HandEditNo.cfg ok
run SpeculationMP.tla SpeculationMP_HandEditLinDrop.cfg ok
run SpeculationMP.tla SpeculationMP_HandEditLinKeep.cfg ok

exit "$FAILED"
