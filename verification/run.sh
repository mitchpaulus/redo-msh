#!/bin/sh
# Run every TLC model check for redo-msh's formal specs.
#
# Requires: Java 11+ and tla2tools.jar (default: ~/.local/share/java/,
# override with TLA_TOOLS_JAR). TLC scratch state goes under /tmp, NOT the
# repo: on WSL the repo lives on a drvfs mount, which is slow for the many
# small files TLC writes.
#
# Each line of output is PASS or FAIL per (spec, config). Configurations
# that document a KNOWN GAP in today's code (or a coverage check) PASS when
# TLC finds exactly the predicted failure: a deadlock, or a violation of
# the one named invariant -- any other outcome, including a violation of a
# different invariant, is a FAIL. See README.md.
set -u
cd "$(dirname "$0")"

JAR="${TLA_TOOLS_JAR:-$HOME/.local/share/java/tla2tools.jar}"
META="${TMPDIR:-/tmp}/redo-msh-tlc"
mkdir -p "$META"
FAILED=0

run() { # run <Module.tla> <Config.cfg> <expect: ok | deadlock | violation:<Invariant>>
    name="$(basename "$2" .cfg)"
    out="$META/$name.out"
    # -cleanup: TLC deletes its own state directory when the run ends (the
    # larger models write gigabytes of state there).
    java -XX:+UseParallelGC -cp "$JAR" tlc2.TLC -cleanup \
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
    violation:*)
        inv="${3#violation:}"
        if grep -q "Invariant $inv is violated" "$out"; then
            echo "PASS  $name ($inv violated, as predicted)"
        else
            echo "FAIL  $name (expected $inv to be violated; see $out)"
            FAILED=1
        fi
        ;;
    esac
}

run TokenPool.tla TokenPool_D2J2.cfg ok
run TokenPool.tla TokenPool_D2J3.cfg ok
run TokenPool.tla TokenPool_D3J2.cfg ok
run TokenPool.tla TokenPool_D3J3.cfg ok
run TokenPool.tla TokenPool_DepthCoverage.cfg violation:NeverTwoDeepExecuting
run TokenPool.tla TokenPool_Eager.cfg ok
run TokenPool.tla TokenPool_UnderUtilization.cfg violation:WorkConserving
run TokenPool.tla TokenPool_PromptDeadlock.cfg deadlock
run TokenPool.tla TokenPool_PromptHold.cfg ok
run TokenPool.tla TokenPool_PromptSource.cfg ok
run TokenPool.tla TokenPool_PromptSourceJ3.cfg ok
run LockSession.tla LockSession_OneSession.cfg ok
run LockSession.tla LockSession_Always.cfg ok
run LockSession.tla LockSession_CommitWindow.cfg violation:NoFalseRefusal
run LockSession.tla LockSession_TwoSessions.cfg violation:NoRedundantBuild
run LockSession.tla LockSession_StaticAdopt.cfg violation:NoOwnOutputAsSource
run LockSession.tla LockSession_RacyStamp.cfg violation:UpToDateIsTrue
run LockSession.tla LockSession_Fixed.cfg ok
run LockSession.tla LockSession_FixedRacy.cfg ok
run LockSession.tla LockSession_FixedDefaultDo.cfg ok
run LockSession.tla LockSession_FixedPhony.cfg ok
run LockSession.tla LockSession_Coverage.cfg violation:NeverCommitWindowLeftover
run SharedPaths.tla SharedPaths_TempGc.cfg violation:NoLostOutput
run SharedPaths.tla SharedPaths_TmpGc.cfg violation:NoGcFailedBuild
run SharedPaths.tla SharedPaths_LogRace.cfg violation:NoFalseCrash
run SharedPaths.tla SharedPaths_LogGc.cfg violation:NoLostLog
run SharedPaths.tla SharedPaths_Foreign.cfg violation:FollowsOwnBuild
run SharedPaths.tla SharedPaths_RunLockRace.cfg violation:NoLostEvent
run SharedPaths.tla SharedPaths_RunLockRaceTwoStepGc.cfg violation:NoLostEvent
run SharedPaths.tla SharedPaths_RunLockRaceNoInodeCheck.cfg violation:NoLostEvent
run SharedPaths.tla SharedPaths_Fixed.cfg ok
run SharedPaths.tla SharedPaths_Fixed3.cfg ok
run SharedPaths.tla SharedPaths_Coverage.cfg violation:JanitorCleans
run CycleLock.tla CycleLock_Diamond.cfg ok
run CycleLock.tla CycleLock_DiamondTwoSessions.cfg violation:BuildOnce
run CycleLock.tla CycleLock_DiamondTwoSessionsFix.cfg ok
run CycleLock.tla CycleLock_CycleSerial.cfg ok
run CycleLock.tla CycleLock_CycleParallel.cfg deadlock
run ParallelEnsure.tla ParallelEnsure_Diamond.cfg ok
run ParallelEnsure.tla ParallelEnsure_DiamondJ1.cfg ok
run ParallelEnsure.tla ParallelEnsure_Wide.cfg ok
run ParallelEnsure.tla ParallelEnsure_Cycle.cfg ok
run ParallelEnsure.tla ParallelEnsure_Coverage.cfg violation:NeverTwoBuilding
run Speculation.tla Speculation_Diamond.cfg ok
run Speculation.tla Speculation_Stale.cfg ok
run Speculation.tla Speculation_StaleCycle.cfg ok
run Speculation.tla Speculation_TrueCycle.cfg ok
run Speculation.tla Speculation_Coverage.cfg violation:NeverVerifiedOverRebuild
run WaitGraph.tla WaitGraph_StaleEdge.cfg violation:NoFalseCycle
run WaitGraph.tla WaitGraph_StaleEdgeGc.cfg ok
run WaitGraph.tla WaitGraph_LiveCross.cfg ok
run WaitGraph.tla WaitGraph_TrueCycle.cfg ok
run WaitGraph.tla WaitGraph_Coverage.cfg violation:NeverFails

exit "$FAILED"
