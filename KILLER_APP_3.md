# Killer App #3: The Windows Operations Cockpit

## Product definition

ion-win turns live Windows system state into structured tables that can be
queried, correlated, and acted on from the same shell.

The promise is:

> See what Windows is doing, isolate the cause, and take a safe action without
> switching tools or parsing formatted text.

This is not a replacement for Task Manager, Event Viewer, or enterprise
observability. It is a keyboard-first diagnostic and remediation environment
for one Windows machine.

## The user and the job

The primary user is a developer, support engineer, power user, or local
administrator investigating questions such as:

- What owns this port?
- Which process is consuming memory, and which service launched it?
- What failed immediately before the application stopped responding?
- Which child processes will be affected if I stop this execution?
- Did the remediation work?

Today the evidence is split across Task Manager, Resource Monitor, Event
Viewer, Services, `netstat`, PowerShell objects, and application logs. Each tool
has a different filter language and loses context when the user moves to the
next one.

The job is complete when the user can move through one loop:

```text
observe -> filter -> correlate -> inspect -> act -> verify
```

## Signature workflow

```ion
# Find the process behind a busy development port.
let listener = ports | where local_port = 5173
listener | select pid process local_address state

# Inspect the owning process and its recent failures.
let owner = processes | where pid = $field($listener pid)
owner | select pid parent_pid name command memory started
events --since 15m --level error | where process_id = $field($owner pid)

# Take an explicit, process-tree-aware action and verify the result.
process stop $field($owner pid) --tree
ports | where local_port = 5173
```

The key product moment is **preserving structure all the way to the action**.
The PID is a typed field selected from live state, not text scraped from a
column aligned for human display.

## Why ion-win can own this

ion-win already has the unusual combination this product needs:

- table values can flow through `where`, `select`, variables, and loops;
- date/time values and timezone-aware operations make event correlation useful;
- the execution registry understands process identity and process trees;
- Job Objects provide a principled boundary for ion-managed processes;
- Windows-specific behavior is an explicit design goal, not an emulation leak;
- safe file deletion establishes the precedent that dangerous actions require
  stronger syntax than inspection.

PowerShell exposes rich objects but is a large automation platform with a high
syntax cost for occasional operators. Traditional shells compose easily but
flatten system state into text. ion-win can occupy the middle: shell-speed
interaction with structured Windows-native evidence.

## MVP product surface

The first milestone should answer the highest-frequency local diagnostic
questions with a small, consistent vocabulary:

- `processes [--all]` produces process rows including PID, parent PID, name,
  executable, command line when accessible, start time, CPU time, working-set
  memory, session, and elevation/access status;
- `ports [--listen] [--tcp|--udp]` produces endpoints with protocol, local and
  remote address/port, state, PID, and resolved process name;
- `services [--all]` produces service name, display name, state, start mode,
  PID, and account;
- `events --log LOG --since DURATION [--level LEVEL]` produces a bounded,
  forward-compatible event table with timestamp, provider, event ID, level,
  process/thread IDs, and message;
- `process show PID` provides one detailed, human-readable inspection view;
- `process stop PID` requests graceful termination where available;
- `process stop PID --tree --force` is the explicit escalation for an
  unmanaged process tree;
- every producer supports table pipelines and returns stable machine-readable
  column names;
- inaccessible fields become empty values plus a stable access-status column;
  a single protected process must not abort a machine-wide query.

Commands should return snapshots, not long-lived magic objects. A fresh command
performs a fresh observation, which keeps pipeline semantics understandable and
prevents stale handles from entering shell variables.

## Safety model

Inspection is read-only and available at the caller's current privilege.
Mutation must be visibly narrower:

- no automatic elevation or UAC prompting;
- no action inferred merely because a table is piped into a command;
- stopping a process requires an explicit PID argument in the MVP;
- `--tree` is explicit because descendants broaden impact;
- `--force` is explicit because graceful shutdown was skipped or failed;
- critical Windows processes, PID 0/4, ion-win itself, and ancestors of the
  current ion-win process are refused by default;
- an override for protected targets is outside the MVP and must not be added
  casually;
- each attempted action prints the resolved target identity before mutation and
  returns a stable result suitable for verification.

The product should help a user understand blast radius before making it larger.

## Non-goals

- remote fleet management;
- kernel debugging, ETW trace analysis, or packet capture;
- a continuously refreshing full-screen TUI in the MVP;
- replacing Performance Monitor or enterprise telemetry retention;
- silently requesting administrator privileges;
- generic WMI/CIM query syntax exposed as the user experience;
- accepting arbitrary piped process tables as bulk kill requests;
- service mutation until process inspection and termination are proven safe.

These constraints preserve a focused diagnostic loop and prevent a useful
inspection feature from becoming an unsafe administration framework.

## Acceptance demonstration

Killer App #3 is real when the compiled binary can perform this demonstration
on a clean Windows machine:

1. Start a test server as an ion-win background execution on a known port.
2. Discover the listener through `ports` without knowing its PID.
3. Use the structured PID to select the correct row from `processes`.
4. Correlate at least one relevant recent Windows event by timestamp or PID.
5. Show the execution relationship for the ion-managed process.
6. Stop the process tree through an explicit safe action.
7. Re-run `ports` and prove that the listener is gone.
8. Serialize the evidence and action result to JSON without scraping display
   output.

The demo must not invoke PowerShell, `wmic`, `tasklist`, `netstat`, or parse
fixed-width text. Native Windows APIs should back the structured producers.

## Success signal

The leading product signal is **diagnostic loops completed**: a session that
uses two or more system-state producers followed by a verification query.

A useful supporting metric is median time from the first observation to an
identified process or service. For the signature “what owns this port?” case,
the answer should take one command and under ten seconds for a new user.

Mutation counts are not a success metric. Frequent forced termination could
indicate that the product is making destructive action too easy.

## Positioning

**ion-win is the shell where Windows stops being a collection of consoles and
becomes one queryable system.**

Killer App #1 makes files and data composable. Killer App #2 promotes working
commands into trusted unattended automation. Killer App #3 closes the live
operations loop: observe the machine, find the cause, act deliberately, and
verify the outcome.
