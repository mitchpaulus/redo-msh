---------------------------- MODULE SharedPaths ----------------------------
(***************************************************************************)
(* Files that concurrent top-level redo runs (SESSIONS) share under        *)
(* .redom/, and the rules that create and delete them (src/build.rs redo / *)
(* gc_temps / build_inner, src/logs.rs create_log / gc_logs / follower).   *)
(*                                                                         *)
(* Every other spec models one session at a time; this one exists because *)
(* the cross-session behavior lives entirely in these files. Each session *)
(* in Builders force-builds the same target t once, with its own log       *)
(* follower; any other session is a run with nothing to build (it still    *)
(* takes its run lock, sweeps, starts its follower and exits) -- which is  *)
(* all a run needs to do to take part in the startup-GC races:             *)
(*                                                                         *)
(*   mkrunlock - create run.<s>.lock (File::create) ...                    *)
(*   lockrun   - ... and take its kernel lock: the run's liveness proof.   *)
(*               InodeCheck (fix): then verify the path still names the    *)
(*               locked file, else start over (the standard lock-file      *)
(*               pattern; a GC may have unlinked it in between).           *)
(*   mkrunlog  - create run.<s>.log, the run trace.                        *)
(*   gc        - the startup sweep (gc_temps + gc_logs). Today it PROBES   *)
(*               each file's liveness and DELETES by path later (two       *)
(*               steps); GcAtomic (fix) probes and deletes each file while *)
(*               holding its liveness lock, so nothing can become live in  *)
(*               between.                                                  *)
(*   (follower starts: opens the run trace; if it is gone, it gives up.)   *)
(*   lock      - the per-target kernel lock (blocking).                    *)
(*   mktmp     - create_log, part 1: create the log's temp file.           *)
(*   rename    - create_log, part 2: rename the temp over the log path;    *)
(*               append the `do` event to the run trace (by path); create  *)
(*               the stdout capture.                                       *)
(*   finish    - read + remove the capture; append `done` to the log path; *)
(*               unlock the target.                                        *)
(*   rootdone  - append the run's own `done` to the run trace (by path).   *)
(*   exit      - after the follower drains: unlock, remove run.<s>.lock    *)
(*               and run.<s>.log.                                          *)
(*   follower  - descend on `do` (open the target log path), delete the    *)
(*               log on our own `done`, report "(crashed)" on EOF + target *)
(*               lock free + no `done`, finish on the run's own `done`.    *)
(*                                                                         *)
(* A file is an INSTANCE (an id) at a path: replacing a path by rename     *)
(* mints a new instance, a reader keeps the instance it opened, and an     *)
(* append by path reaches whatever instance is at the path now -- or fails *)
(* if there is none (event_append never creates). A session may crash at   *)
(* any step (at most MaxCrashes times); the OS releases its kernel locks.   *)
(* After every session has                                                 *)
(* ended, a JANITOR (the next redo run's startup GC, with no one alive)    *)
(* sweeps once, so GC completeness can be checked.                         *)
(*                                                                         *)
(* Fix switches: GcByLiveness (captures and log temps swept only when the  *)
(* owning build no longer holds the target lock; today: unconditionally),  *)
(* SessionScopedLogs (t.<key>.<session>.log swept when its session is      *)
(* dead; today: one t.<key>.log per target, swept when the target lock is  *)
(* free), GcAtomic, InodeCheck.                                            *)
(*                                                                         *)
(* Properties (history flags record that a bad step happened; the model    *)
(* then continues as if it had succeeded -- flags are read only by         *)
(* invariants):                                                            *)
(*   NoLostOutput    - a build always finds its own stdout capture (today  *)
(*                     a missing one makes it commit "no output" and       *)
(*                     DELETE the target -- reproduced).                   *)
(*   NoGcFailedBuild - create_log's rename always finds its temp.          *)
(*   NoLostEvent     - every append to a live run's trace lands in it.     *)
(*   NoFalseCrash    - "(crashed)" only for a builder that crashed.        *)
(*   NoLostLog       - a live run's follower always finds its own log.     *)
(*   FollowsOwnBuild - a follower only ever tails its own run's build.     *)
(*   CleanExit       - a run that exits leaves none of its files behind.   *)
(*   JanitorCleans   - once every run has ended, one sweep removes every   *)
(*                     file (GC completeness: nothing leaks forever).      *)
(*   Termination     - every run that does not crash exits.               *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Sessions, Builders, GcByLiveness, SessionScopedLogs, GcAtomic, InodeCheck,
          JanitorDeletes, MaxCrashes

ASSUME \A b \in {GcByLiveness, SessionScopedLogs, GcAtomic, InodeCheck,
                 JanitorDeletes} : b \in BOOLEAN
ASSUME MaxCrashes \in Nat /\ Builders \subseteq Sessions

TwoSessions == {"s1", "s2"}
ThreeSessions == {"s1", "s2", "s3"}
\* For configurations that declare sessions as model values: the builders
\* are interchangeable, so TLC may explore one representative per
\* permutation (sound for invariants; NOT for liveness, which those
\* configs omit).
Perms == Permutations(Builders)

FREE == "FREE"
NOBODY == "NOBODY"

\* Per session: at most one run-lock file per attempt (an attempt restarts
\* only after another session's GC unlinked it: at most |Sessions| - 1
\* times), the run log, the target log (born as the temp), the capture.
MaxId == Cardinality(Sessions) * (Cardinality(Sessions) + 3)
Ids == 1..MaxId

RunLockPath(s) == <<"runlock", s>>   \* logs/run.<session>.lock
RunLogPath(s) == <<"runlog", s>>     \* logs/run.<session>.log
TmpPath(s) == <<"tmp", s>>           \* logs/t.<key>.log.<pid>.tmp
CapPath(s) == <<"cap", s>>           \* tmp/stdout.<pid>.<target>
LogPath(s) == IF SessionScopedLogs THEN <<"log", s>> ELSE <<"log", "t">>
Paths == UNION {{RunLockPath(s), RunLogPath(s), TmpPath(s), CapPath(s),
                 LogPath(s)} : s \in Sessions}

VARIABLES
    phase,           \* [Sessions -> lifecycle]
    lock,            \* the target's kernel lock: holder or FREE
    held,            \* [Sessions -> run-lock instance whose lock s holds, or 0]
    fs,              \* [Paths -> instance id, or 0 = no file]
    owner,           \* [Ids -> session that created it, or NOBODY]
    done,            \* [Ids -> BOOLEAN]: a target `done` is in that instance
    nextId,
    myRunLock, myRunLog, myTmp, myCap,   \* [Sessions -> Ids \cup {0}]
    toDel,           \* [Sessions -> SUBSET Paths]: probed, not yet deleted
    evDo, evRoot,    \* [Sessions -> BOOLEAN]: event landed in our trace
    fpc,             \* [Sessions -> follower state]
    frun,            \* [Sessions -> run-trace instance the follower opened]
    freader,         \* [Sessions -> target-log instance the follower opened]
    janitor,         \* the post-run sweep has happened
    crashes,         \* crashes so far (bounded by MaxCrashes)
    lostOutput, gcFailedBuild, lostEvent, falseCrash, lostLog, followedForeign

vars == <<phase, lock, held, fs, owner, done, nextId, myRunLock, myRunLog,
          myTmp, myCap, toDel, evDo, evRoot, fpc, frun, freader, janitor,
          crashes, lostOutput, gcFailedBuild, lostEvent, falseCrash, lostLog,
          followedForeign>>
mine == <<myRunLock, myRunLog, myTmp, myCap>>
fol == <<fpc, frun, freader>>
flags == <<crashes, lostOutput, gcFailedBuild, lostEvent, falseCrash, lostLog,
           followedForeign>>

Phases == {"init", "lockfile", "runlog", "gc", "gcdel", "ready", "tlocked",
           "tmp", "running", "built", "finished", "exited", "crashed"}
FStates == {"none", "running", "tail", "afterchild", "finished", "dead"}

TypeOK ==
    /\ phase \in [Sessions -> Phases]
    /\ lock \in Sessions \cup {FREE}
    /\ held \in [Sessions -> {0} \cup Ids]
    /\ fs \in [Paths -> {0} \cup Ids]
    /\ owner \in [Ids -> Sessions \cup {NOBODY}]
    /\ done \in [Ids -> BOOLEAN]
    /\ nextId \in 1..(MaxId + 1)
    /\ \A m \in {myRunLock, myRunLog, myTmp, myCap} : m \in [Sessions -> {0} \cup Ids]
    /\ toDel \in [Sessions -> SUBSET Paths]
    /\ evDo \in [Sessions -> BOOLEAN] /\ evRoot \in [Sessions -> BOOLEAN]
    /\ fpc \in [Sessions -> FStates]
    /\ frun \in [Sessions -> {0} \cup Ids] /\ freader \in [Sessions -> {0} \cup Ids]
    /\ janitor \in BOOLEAN
    /\ crashes \in 0..MaxCrashes

Init ==
    /\ phase = [s \in Sessions |-> "init"]
    /\ lock = FREE
    /\ held = [s \in Sessions |-> 0]
    /\ fs = [p \in Paths |-> 0]
    /\ owner = [i \in Ids |-> NOBODY]
    /\ done = [i \in Ids |-> FALSE]
    /\ nextId = 1
    /\ myRunLock = [s \in Sessions |-> 0] /\ myRunLog = [s \in Sessions |-> 0]
    /\ myTmp = [s \in Sessions |-> 0] /\ myCap = [s \in Sessions |-> 0]
    /\ toDel = [s \in Sessions |-> {}]
    /\ evDo = [s \in Sessions |-> FALSE] /\ evRoot = [s \in Sessions |-> FALSE]
    /\ fpc = [s \in Sessions |-> "none"]
    /\ frun = [s \in Sessions |-> 0] /\ freader = [s \in Sessions |-> 0]
    /\ janitor = FALSE
    /\ crashes = 0
    /\ lostOutput = FALSE /\ gcFailedBuild = FALSE /\ lostEvent = FALSE
    /\ falseCrash = FALSE /\ lostLog = FALSE /\ followedForeign = FALSE

Live(s) == phase[s] \notin {"exited", "crashed"}

(* lock_file_is_free: no file at the path, or nobody holds its lock. *)
RunLockFree(o) ==
    fs[RunLockPath(o)] = 0 \/ ~\E x \in Sessions : held[x] = fs[RunLockPath(o)]

(* The startup GC's verdict on the file at path p (instance i). The run
   files of session o are judged by o's run lock; a session never sweeps
   its own run files. *)
Stale(s, p, i) ==
    CASE p[1] \in {"runlock", "runlog"} -> p[2] # s /\ RunLockFree(p[2])
      [] p[1] \in {"cap", "tmp"} ->
             IF GcByLiveness THEN lock # owner[i] ELSE TRUE
      [] p[1] = "log" ->
             IF SessionScopedLogs THEN RunLockFree(owner[i]) ELSE lock = FREE

StaleSet(s) == {p \in Paths : fs[p] # 0 /\ Stale(s, p, fs[p])}

Mint(p, s) ==
    /\ fs' = [fs EXCEPT ![p] = nextId]
    /\ owner' = [owner EXCEPT ![nextId] = s]
    /\ nextId' = nextId + 1

(* Append an event by path: lands iff our trace instance is still there. *)
Lands(s) == fs[RunLogPath(s)] = myRunLog[s]

----------------------------------------------------------------------------
MkRunLock(s) ==
    /\ phase[s] = "init"
    /\ Mint(RunLockPath(s), s)
    /\ myRunLock' = [myRunLock EXCEPT ![s] = nextId]
    /\ phase' = [phase EXCEPT ![s] = "lockfile"]
    /\ UNCHANGED <<lock, held, done, myRunLog, myTmp, myCap, toDel, evDo,
                   evRoot, fol, janitor, flags>>

(* Lock the instance we created (it may already be unlinked). *)
LockRun(s) ==
    /\ phase[s] = "lockfile"
    /\ IF InodeCheck /\ fs[RunLockPath(s)] # myRunLock[s]
         THEN /\ phase' = [phase EXCEPT ![s] = "init"]     \* start over
              /\ UNCHANGED held
         ELSE /\ held' = [held EXCEPT ![s] = myRunLock[s]]
              /\ phase' = [phase EXCEPT ![s] = "runlog"]
    /\ UNCHANGED <<lock, fs, owner, done, nextId, mine, toDel, evDo, evRoot,
                   fol, janitor, flags>>

MkRunLog(s) ==
    /\ phase[s] = "runlog"
    /\ Mint(RunLogPath(s), s)
    /\ myRunLog' = [myRunLog EXCEPT ![s] = nextId]
    /\ phase' = [phase EXCEPT ![s] = "gc"]
    /\ UNCHANGED <<lock, held, done, myRunLock, myTmp, myCap, toDel, evDo,
                   evRoot, fol, janitor, flags>>

(* The startup GC: atomic (fix) or probe-then-delete-by-path (today). *)
Gc(s) ==
    /\ phase[s] = "gc"
    /\ IF GcAtomic
         THEN /\ fs' = [p \in Paths |-> IF p \in StaleSet(s) THEN 0 ELSE fs[p]]
              /\ phase' = [phase EXCEPT ![s] = "ready"]
              /\ UNCHANGED toDel
         ELSE /\ toDel' = [toDel EXCEPT ![s] = StaleSet(s)]
              /\ phase' = [phase EXCEPT ![s] = "gcdel"]
              /\ UNCHANGED fs
    /\ UNCHANGED <<lock, held, owner, done, nextId, mine, evDo, evRoot, fol,
                   janitor, flags>>

GcDelete(s) ==
    /\ phase[s] = "gcdel"
    /\ fs' = [p \in Paths |-> IF p \in toDel[s] THEN 0 ELSE fs[p]]
    /\ toDel' = [toDel EXCEPT ![s] = {}]
    /\ phase' = [phase EXCEPT ![s] = "ready"]
    /\ UNCHANGED <<lock, held, owner, done, nextId, mine, evDo, evRoot, fol,
                   janitor, flags>>

(* A run with nothing to build goes straight to its own `done`. *)
NoBuild(s) ==
    /\ phase[s] = "ready"
    /\ s \notin Builders
    /\ fpc[s] # "none"
    /\ phase' = [phase EXCEPT ![s] = "built"]
    /\ UNCHANGED <<lock, held, fs, owner, done, nextId, mine, toDel, evDo, evRoot,
                   fol, janitor, flags>>

Lock(s) ==
    /\ phase[s] = "ready"
    /\ s \in Builders
    /\ fpc[s] # "none"                 \* the follower is started first
    /\ lock = FREE
    /\ lock' = s
    /\ phase' = [phase EXCEPT ![s] = "tlocked"]
    /\ UNCHANGED <<held, fs, owner, done, nextId, mine, toDel, evDo, evRoot,
                   fol, janitor, flags>>

MkTmp(s) ==
    /\ phase[s] = "tlocked"
    /\ Mint(TmpPath(s), s)
    /\ myTmp' = [myTmp EXCEPT ![s] = nextId]
    /\ phase' = [phase EXCEPT ![s] = "tmp"]
    /\ UNCHANGED <<lock, held, done, myRunLock, myRunLog, myCap, toDel, evDo,
                   evRoot, fol, janitor, flags>>

(* Rename the temp over the log path, append `do` by path, create the
   stdout capture. *)
RenameLog(s) ==
    /\ phase[s] = "tmp"
    /\ gcFailedBuild' = (gcFailedBuild \/ fs[TmpPath(s)] # myTmp[s])
    /\ evDo' = [evDo EXCEPT ![s] = Lands(s)]
    /\ lostEvent' = (lostEvent \/ ~Lands(s))
    /\ fs' = [fs EXCEPT ![LogPath(s)] = myTmp[s], ![TmpPath(s)] = 0,
                        ![CapPath(s)] = nextId]
    /\ owner' = [owner EXCEPT ![nextId] = s]
    /\ myCap' = [myCap EXCEPT ![s] = nextId]
    /\ nextId' = nextId + 1
    /\ phase' = [phase EXCEPT ![s] = "running"]
    /\ UNCHANGED <<lock, held, done, myRunLock, myRunLog, myTmp, toDel, evRoot,
                   fol, janitor, crashes, lostOutput, falseCrash, lostLog,
                   followedForeign>>

(* Read + remove the capture (by path), append `done` to whatever instance
   is at the log path, unlock the target (done before unlock: I4). *)
Finish(s) ==
    /\ phase[s] = "running"
    /\ lostOutput' = (lostOutput \/ fs[CapPath(s)] # myCap[s])
    /\ fs' = [fs EXCEPT ![CapPath(s)] = 0]
    /\ done' = IF fs[LogPath(s)] # 0 THEN [done EXCEPT ![fs[LogPath(s)]] = TRUE]
                                     ELSE done
    /\ lock' = FREE
    /\ phase' = [phase EXCEPT ![s] = "built"]
    /\ UNCHANGED <<held, owner, nextId, mine, toDel, evDo, evRoot, fol, janitor,
                   crashes, gcFailedBuild, lostEvent, falseCrash, lostLog, followedForeign>>

(* The run's own `done` into the run trace (by path). *)
RootDone(s) ==
    /\ phase[s] = "built"
    /\ evRoot' = [evRoot EXCEPT ![s] = Lands(s)]
    /\ lostEvent' = (lostEvent \/ ~Lands(s))
    /\ phase' = [phase EXCEPT ![s] = "finished"]
    /\ UNCHANGED <<lock, held, fs, owner, done, nextId, mine, toDel, evDo, fol,
                   janitor, crashes, lostOutput, gcFailedBuild, falseCrash, lostLog,
                   followedForeign>>

(* After the follower drains: unlock, then remove our run files by path. *)
Exit(s) ==
    /\ phase[s] = "finished"
    /\ fpc[s] = "finished"
    /\ held' = [held EXCEPT ![s] = 0]
    /\ fs' = [fs EXCEPT ![RunLockPath(s)] = 0, ![RunLogPath(s)] = 0]
    /\ phase' = [phase EXCEPT ![s] = "exited"]
    /\ UNCHANGED <<lock, owner, done, nextId, mine, toDel, evDo, evRoot, fol,
                   janitor, flags>>

Crash(s) ==
    /\ Live(s)
    /\ crashes < MaxCrashes
    /\ crashes' = crashes + 1
    /\ phase' = [phase EXCEPT ![s] = "crashed"]
    /\ fpc' = [fpc EXCEPT ![s] = "dead"]
    /\ lock' = IF lock = s THEN FREE ELSE lock
    /\ held' = [held EXCEPT ![s] = 0]
    /\ UNCHANGED <<fs, owner, done, nextId, mine, toDel, evDo, evRoot, frun,
                   freader, janitor, lostOutput, gcFailedBuild, lostEvent,
                   falseCrash, lostLog, followedForeign>>

----------------------------------------------------------------------------
(* The follower thread starts after the GC: open the run trace by path. *)
FStart(s) ==
    /\ fpc[s] = "none"
    /\ phase[s] = "ready"
    /\ IF fs[RunLogPath(s)] = 0
         THEN /\ fpc' = [fpc EXCEPT ![s] = "finished"]   \* gives up silently
              /\ lostLog' = TRUE
              /\ UNCHANGED frun
         ELSE /\ fpc' = [fpc EXCEPT ![s] = "running"]
              /\ frun' = [frun EXCEPT ![s] = fs[RunLogPath(s)]]
              /\ UNCHANGED lostLog
    /\ UNCHANGED <<phase, lock, held, fs, owner, done, nextId, mine, toDel,
                   evDo, evRoot, freader, janitor, crashes, lostOutput, gcFailedBuild,
                   lostEvent, falseCrash, followedForeign>>

(* Our `do` is in the trace instance we read: open the target log path. *)
FOpen(s) ==
    /\ fpc[s] = "running"
    /\ evDo[s] /\ frun[s] = myRunLog[s]
    /\ IF fs[LogPath(s)] = 0
         THEN /\ lostLog' = TRUE
              /\ fpc' = [fpc EXCEPT ![s] = "afterchild"]
              /\ UNCHANGED <<freader, followedForeign>>
         ELSE /\ freader' = [freader EXCEPT ![s] = fs[LogPath(s)]]
              /\ followedForeign' = (followedForeign \/ owner[fs[LogPath(s)]] # s)
              /\ fpc' = [fpc EXCEPT ![s] = "tail"]
              /\ UNCHANGED lostLog
    /\ UNCHANGED <<phase, lock, held, fs, owner, done, nextId, mine, toDel,
                   evDo, evRoot, frun, janitor, crashes, lostOutput, gcFailedBuild,
                   lostEvent, falseCrash>>

(* `done` read: our own run's done -> delete the log BY PATH. *)
FDone(s) ==
    /\ fpc[s] = "tail"
    /\ done[freader[s]]
    /\ fs' = IF owner[freader[s]] = s THEN [fs EXCEPT ![LogPath(s)] = 0] ELSE fs
    /\ fpc' = [fpc EXCEPT ![s] = "afterchild"]
    /\ UNCHANGED <<phase, lock, held, owner, done, nextId, mine, toDel, evDo,
                   evRoot, frun, freader, janitor, flags>>

(* EOF, no `done`, target lock free: reported as a crashed builder. *)
FCrash(s) ==
    /\ fpc[s] = "tail"
    /\ ~done[freader[s]]
    /\ lock = FREE
    /\ falseCrash' = (falseCrash \/ phase[owner[freader[s]]] # "crashed")
    /\ fpc' = [fpc EXCEPT ![s] = "afterchild"]
    /\ UNCHANGED <<phase, lock, held, fs, owner, done, nextId, mine, toDel,
                   evDo, evRoot, frun, freader, janitor, crashes, lostOutput,
                   gcFailedBuild, lostEvent, lostLog, followedForeign>>

(* The run's own `done` is in the trace we read: the follower finishes.
   (If our `do` never landed, the next line it reads is this one.) *)
FRoot(s) ==
    /\ \/ fpc[s] = "afterchild"
       \/ fpc[s] = "running" /\ ~(evDo[s] /\ frun[s] = myRunLog[s])
    /\ evRoot[s] /\ frun[s] = myRunLog[s]
    /\ fpc' = [fpc EXCEPT ![s] = "finished"]
    /\ UNCHANGED <<phase, lock, held, fs, owner, done, nextId, mine, toDel,
                   evDo, evRoot, frun, freader, janitor, flags>>

----------------------------------------------------------------------------
Ended == \A s \in Sessions : ~Live(s)

(* The next redo run's startup sweep, with every earlier run ended.
   JanitorDeletes = FALSE is a mutation (a GC that never deletes). *)
Janitor ==
    /\ Ended /\ ~janitor
    /\ fs' = IF JanitorDeletes
               THEN [p \in Paths |-> IF fs[p] # 0 /\ Stale("janitor", p, fs[p])
                                       THEN 0 ELSE fs[p]]
               ELSE fs
    /\ janitor' = TRUE
    /\ UNCHANGED <<phase, lock, held, owner, done, nextId, mine, toDel, evDo,
                   evRoot, fol, flags>>

SStep(s) ==
    \/ MkRunLock(s) \/ LockRun(s) \/ MkRunLog(s) \/ Gc(s) \/ GcDelete(s)
    \/ NoBuild(s) \/ Lock(s) \/ MkTmp(s) \/ RenameLog(s) \/ Finish(s) \/ RootDone(s)
    \/ Exit(s)
    \/ FStart(s) \/ FOpen(s) \/ FDone(s) \/ FCrash(s) \/ FRoot(s)

Terminating == Ended /\ janitor /\ UNCHANGED vars

Next == (\E s \in Sessions : SStep(s) \/ Crash(s)) \/ Janitor \/ Terminating

Spec == Init /\ [][Next]_vars /\ (\A s \in Sessions : WF_vars(SStep(s)))
             /\ WF_vars(Janitor)

----------------------------------------------------------------------------
NoLostOutput == ~lostOutput
NoGcFailedBuild == ~gcFailedBuild
NoLostEvent == ~lostEvent
NoFalseCrash == ~falseCrash
NoLostLog == ~lostLog
FollowsOwnBuild == ~followedForeign

CleanExit ==
    \A s \in Sessions :
        phase[s] = "exited" => \A p \in Paths : fs[p] = 0 \/ owner[fs[p]] # s

JanitorCleans == janitor => \A p \in Paths : fs[p] = 0

Termination == <>Ended

============================================================================
