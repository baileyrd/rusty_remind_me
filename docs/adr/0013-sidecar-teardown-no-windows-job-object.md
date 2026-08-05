# ADR-0013: Sidecar teardown — kill on drop, no Windows Job object

Status: Accepted
Date: 2026-08-05

## Context

`#169` ports the reference's `remind_me_mcp/sidecars.py`: child processes —
the hub SSH tunnel, optionally the dashboard UI — kept alive alongside the
server, started idempotently from the sync loop so one lost to a sibling
server's exit is respawned within an interval.

The module's stated purpose is that the children **live and die with the
server**. Starting them is easy; the interesting half is guaranteeing they
stop.

The reference spawns each child into a Windows **Job object** created with
`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (`CreateJobObjectW` +
`SetInformationJobObject` + `AssignProcessToJobObject`, all via `ctypes`).
When the parent's handle closes — for any reason, including `TerminateProcess`
or a hard crash — the OS kills every process in the job. No exit hook is
involved, which is precisely what makes it robust.

Two facts shaped this decision:

1. **The reference's guarantee is Windows-only.** Its `_job()` opens with
   `if sys.platform != "win32" or _job_handle is not None: return _job_handle`
   — on Linux and macOS it returns `None` immediately and no job is ever
   created. There is no `prctl(PR_SET_PDEATHSIG)` fallback, no process group
   teardown, nothing. On Unix the reference orphans its sidecars on abnormal
   exit exactly as a naive implementation would.

2. **Reaching the Win32 job APIs from Rust needs a direct `windows-sys`
   dependency.** This workspace has no FFI dependency of any kind today —
   `docs/adr/0012` took the same decision against `libc` for a `kill(0)`
   probe, and every hand-rolled client here (`sync/http.rs`, `embedder.rs`,
   the webhook and API servers) uses only `std`. `windows-sys` is present in
   `Cargo.lock` transitively, but declaring it is still a deliberate act.

## Decision

**Kill children on `Drop`, on every platform. Do not create a Windows Job
object, and do not add `windows-sys`.**

`Sidecars::shutdown` kills and reaps every child it started, and `Drop` calls
it. Unwinding runs `Drop`, so both a normal return and a panic tear the
children down.

What that covers, against the reference:

| Platform | Exit | Reference | Here |
| --- | --- | --- | --- |
| Windows | graceful | killed | killed |
| Windows | abnormal (`TerminateProcess`, crash) | killed by the OS | **orphaned** |
| Unix | graceful | killed | killed |
| Unix | abnormal (`SIGKILL`, crash) | orphaned | orphaned |

**One cell differs.** Three of four match, and the Unix rows match because the
reference has no guarantee there either — this is a narrower gap than
"the reference tears down and we do not" suggests.

## Consequences

- No new dependency; the workspace stays FFI-free, consistent with ADR-0012.
- **On Windows, a `SIGKILL`-equivalent leaves the tunnel running.** The
  practical symptom is mild: the orphan keeps holding its local port, so the
  next server's `ensure` sees the port answering and declines to start its
  own. The surviving tunnel still works. It is a leaked process, not an
  outage — but it is a leaked process, and nothing here reaps it.
- The gap is stated in `sidecars`' own module docs, including the table
  above, rather than left for someone to find when a stale `ssh.exe` turns up
  in Task Manager.
- **Revisiting is small and self-contained**, exactly as ADR-0012 said of
  `libc`: add `windows-sys` with the `Win32_System_JobObjects` feature,
  create the job once, and assign in `Sidecars::spawn`. Nothing else in the
  module changes shape. If Windows becomes a first-class deployment target —
  or if an orphaned tunnel is actually observed in practice — that is the
  moment to do it, and this ADR is the record of why it was not done at
  `#169`'s scope rather than a claim that it never should be.

## Alternatives considered

- **`prctl(PR_SET_PDEATHSIG)` on Linux.** Would make the Unix abnormal-exit
  row *better* than the reference's. Rejected for this change: it needs
  `libc` (ADR-0012 again), it is Linux-only so it does not generalise to
  macOS, and it closes a hole the reference does not consider worth closing.
  Worth revisiting only alongside the Windows work, so the platform story is
  decided once rather than drifting a row at a time.
- **A supervising wrapper process.** Portable, and genuinely robust, but it
  doubles the process count and puts a bespoke supervisor on the critical
  path for a feature whose entire job is keeping one `ssh` alive. Far more
  machinery than the gap justifies.
