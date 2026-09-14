# Killer App #2: Windows Automations You Can Trust

## Product definition

ion-win turns an ordinary command or Ion script into a named, scheduled,
observable Windows automation without requiring PowerShell, Task Scheduler XML,
or a separate workflow service.

The promise is:

> Write it once at the prompt. Save it as a task. Schedule it. Know what
> happened.

This is not a general workflow orchestrator. It is the shortest path from a
working local command to a dependable automation on one Windows machine.

## The user and the job

The primary user is a Windows developer, power user, or small-team operator who
has recurring local work: backups, exports, media processing, repository
maintenance, report generation, downloads, and cleanup.

Today that user has to combine a script, Task Scheduler, fragile quoting,
redirected log files, and manual process cleanup. The job is complete only when
they can answer four questions from one place:

1. What will run?
2. When will it run?
3. Is it running now?
4. What happened last time?

## Signature workflow

```ion
# Prove the operation interactively.
find C:\Photos --recurse | stat --hash sha256 | compress D:\Backups\photos.zip

# Promote the same operation to a governed task.
task create --timeout 30m --memory 1GiB photo-backup -- \
    ion-win.exe C:\Automations\photo-backup.ion

# Put it on the Windows calendar without leaving the shell.
schedule create nightly-photos photo-backup daily 02:00 Australia/Sydney

# Inspect and control actual runs.
exec list
exec show 42
exec cancel 42
```

The key product moment is **promotion, not translation**: the command that was
tested interactively becomes automation without being rewritten in another
language or configured in a GUI.

## Why ion-win can own this

The required foundation already exists in one coherent runtime:

- structured file/data pipelines make useful automations concise;
- immutable tasks preserve launch intent and working directory;
- Windows Task Scheduler integration supplies reboot-safe triggers;
- every launch enters the same execution lifecycle;
- Job Objects provide process-tree cancellation and enforce memory/process
  limits;
- timeouts, inspection, and cancellation behave the same for manual and
  scheduled runs.

Most shells stop after launching a process. Most schedulers start at a form or
configuration file. ion-win connects the interactive experiment to the
scheduled, governed execution.

## MVP product surface

The existing task, schedule, and execution commands provide the control plane.
The next milestone should complete the feedback loop with durable run history:

- persist a sanitized record for every task execution across ion-win processes
  and reboots;
- add `task history NAME` with start time, duration, terminal state, exit code,
  and policy failure;
- capture bounded stdout and stderr per run, with secrets and effective
  environment excluded;
- add `exec logs ID [--stderr] [--follow]`;
- add `task run NAME --wait` as the script- and CI-friendly success boundary;
- give scheduled runs the same records and logs as interactive task runs;
- define retention by count and total bytes, with explicit pruning behavior.

One-command task creation from arbitrary shell text is deliberately not in the
MVP. The current external-command task definition is safe and unambiguous;
multi-step work belongs in a version-controlled `.ion` script until a real
workflow format is justified.

## Non-goals

- distributed or cloud orchestration;
- DAGs, approvals, queues, and team tenancy;
- a Task Scheduler replacement UI;
- storing credentials in task definitions or execution history;
- automatic retries before idempotency and retry policy can be expressed
  safely;
- notifications before durable results and logs exist;
- ConPTY unless a specific automated program requires a terminal.

These constraints keep the product about reliable local automation rather than
turning ion-win into a smaller, incompatible CI system.

## Acceptance demonstration

Killer App #2 is real when a clean Windows machine can demonstrate all of the
following with the compiled binary:

1. Create a version-controlled `.ion` backup script that uses a structured
   manifest.
2. Run it manually and observe a successful execution record.
3. Register a daily schedule in an IANA timezone.
4. Trigger that schedule and inspect the same task identity, captured output,
   duration, and result from a new ion-win process.
5. Trigger a deliberately long run, observe timeout cancellation of its process
   tree, and inspect a retained `TimedOut` result.
6. Reboot or restart ion-win and still retrieve both run records and logs.

The demo must not open Task Scheduler, construct XML, use PowerShell, or search
the filesystem for ad hoc log files.

## Success signal

The leading product signal is the number of distinct tasks that run successfully
on a schedule more than once. Creation counts are weak; repeated unattended
success proves that ion-win has become infrastructure the user trusts.

A useful supporting metric is **time from first successful interactive run to
first scheduled run**. The product should make that promotion take less than two
minutes.

## Positioning

**ion-win is the Windows shell where a command can grow up into an automation.**

Killer App #1 makes Windows file and data work composable. Killer App #2 makes
that work repeatable, observable, and safe to leave unattended.
