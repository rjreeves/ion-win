# Architecture Specification: Ion Shell Windows Port (`ion-win`)

## 1. Executive Summary & Goals

The objective of this project is to port the **Ion Shell** (originally designed for Redox OS) to run as a first-class, native shell on Microsoft Windows. The tool is built using **Rust**, maximizing **LLVM optimizations** to deliver sub-millisecond startup latencies, robust memory safety, and minimal system resource usage.

### Core Pillars

- **Zero-VM Native Execution**: Compiled directly to target `x86_64-pc-windows-msvc`. No .NET CLR, JVM, or POSIX-emulation layers (like MSYS2/Cygwin) at runtime.
- **Embedded State Persistence**: Leveraging a pure-Rust, zero-copy embedded database (`redb`) to handle fast shell state, bookmarks, and persistent settings.
- **Advanced Macro Expansion**: A native combinatorial engine (`commandx`) that processed file wildcards into independent command streams over Cartesian products — since removed; see §4.

## 2. Compilation & LLVM Optimization Profile

To achieve the performance expected of a high-speed system shell, the compilation pipeline bypasses standard runtime compilation speeds in favor of aggressive global optimizations.

### `Cargo.toml` Production Configuration

```toml
[package]
name = "ion-win"
version = "0.1.0"
edition = "2021"

[dependencies]
tokio = { version = "1.0", features = ["full"] }
redb = "2.0"          # Pure Rust database engine
glob = "0.3"
crossterm = "0.27"    # Native Windows VT100 console support
windows-sys = { version = "0.52", features = ["Win32_System_Threading", "Win32_System_Console"] }
ctrlc = "3"

[profile.release]
opt-level = 3       # Maximize machine-code execution speed
lto = true          # Global Link-Time Optimization across all crates
codegen-units = 1   # Disables parallel codegen to allow LLVM global optimization
panic = "abort"     # Eliminates stack unwinding, drastically shrinking binary size
strip = true        # Drops all debug symbols and symbol tables from the .exe
```

Why `lto = true` and `codegen-units = 1` matter: this forces LLVM to treat the entire codebase and all of its dependencies (terminal handling, parsing crates, etc.) as a single, giant puzzle. It allows LLVM to inline functions aggressively across crate boundaries and eliminate dead code that Ion doesn't use, resulting in a much smaller and faster `.exe`.

## 3. Storage Architecture: Embedded State via `redb`

To replace the traditional text-based `initrc` state parsing with instant, transactional variables and workspace bookmarks, an embedded database is compiled directly into the binary.

`redb` is a pure-Rust, persistent, file-backed analogue of Rust's native `BTreeMap` — the insurance-safe alternative to LMDB without the C dependency. It provides:

- **Zero-copy reads**: thread-safe references straight to data, no allocation.
- **Flawless LLVM optimization**: 100% pure Rust means LTO can inline its internals directly into the shell's parsing loops.
- **ACID compliance & crash safety**: Copy-on-Write B+Tree — a crash mid-write rolls back to the last valid commit, never corrupting the file.
- **Non-blocking concurrent reads (MVCC)**: multiple terminal tabs can read simultaneously without blocking each other.

### Concurrency Isolation Engine

Because `redb` implements **Single-Writer, Multiple-Reader (SWMR)** file locking, multiple concurrent terminal tabs could cause database write collisions. To solve this, the shell decouples user commands from disk interaction using a non-blocking background queue:

```
[Main Terminal Thread]
    |
    +--> (Processes prompt / executes commands instantly)
    |
    +--> [Async Channel (tokio::sync::mpsc)]
              |
              v
        [Background DB Worker Thread]
              |
              v (Handles retries and transactional flushes)
        [state.redb File on Disk]
```

The main thread never talks to `redb` directly. On each command, it sends a small metadata struct (`CommandHistory { timestamp, cmd, duration, exit_code }`) into a channel and immediately renders the next prompt. A dedicated background worker (or `tokio::spawn_blocking` task) drains the channel and batch-writes to disk, so a temporary file lock or slow disk flush is never felt at the terminal prompt.

### Database Schema Map

```rust
use redb::TableDefinition;

// Persistent User Variables: Maps variable tokens to evaluated strings
pub const PERSISTENT_VARS: TableDefinition<&str, &str> = TableDefinition::new("shell_variables");

// Fast-Travel Bookmarks: Maps short aliases to absolute Windows file paths
pub const DIR_BOOKMARKS: TableDefinition<&str, &str> = TableDefinition::new("dir_bookmarks");
```

### Known Overheads (accepted trade-offs)

| Overhead | Cost | Mitigation |
|---|---|---|
| Binary size | +500KB–1MB to the compiled `.exe` | Negligible vs. DuckDB (+30MB+); LTO trims further |
| Runtime memory | mmap'd file; OS caches only hot B-tree pages | No action needed — physical RAM cost is a few MB |
| Storage/disk | Copy-on-Write causes file growth over time | Periodic `Database::compact()` / auto-compaction triggers |
| Multi-process write locks | SWMR — only one writer at a time; concurrent sub-shells can hit `DatabaseAlreadyOpen` | Deferred background writer with non-blocking retry queue (see above) |

### Proposed Shell UX Commands

```
# Set a persistent environment key globally
ion> pvar set sys_mode = "production"

# List all stored persistent variables (reads from redb)
ion> pvar list

# Delete a persistent state
ion> pvar delete sys_mode

# Bookmark your current project directory with a short name
ion> dmark add active_proj

# Jump straight to that folder from anywhere on your system
ion> jump active_proj

# List your saved fast-travel locations
ion> dmark list

# Save/restore an entire workspace snapshot (variables, bookmarks, cwd)
ion> workspace save code-session
ion> workspace load code-session
```

Ion's native inline method-expansion syntax can also query `redb` directly inside logic, using a `$db(...)` / `@db(...)` prefix convention:

```
if eq $db(user_tier) "admin"
    echo "Welcome back, Admin."
end

for server in @db(monitored_servers)
    ping $server
end
```

## 4. Combinatorial Macro Engine (`commandx`) — REMOVED

**Status: built, then removed by product decision.** This section is kept as a historical record of the original design blueprint below, but `commandx` no longer exists in the codebase (`src/commandx.rs` deleted, all dispatch/pipeline/completion wiring removed). It never gained the ability to actually *run* what it generated — see the removed "commandx doesn't execute what it generates" gap entry that used to be in `HANDOVER.md` — and rather than keep carrying that half-finished feature, it was dropped outright.

The `commandx` command acted as an inline cross-multiplication engine. It identifies wildcard anchors inside command skeletons, matches them against the Windows filesystem, and computes their Cartesian product to yield explicit multi-line execution scripts.

### Tokenization Flow

1. **Parser hook**: the lexer detects a `commandx( ... )` expression payload.
2. **Glob resolution**: inner wildcard values bounded by `< >` (e.g. `*d` or `f*`) are evaluated using native Windows filesystem indexing via the `glob` crate.
3. **Cross product generation**: a nested loop vectorizes the combinations into unique executable string allocations.

### Core Rust Execution Blueprint

```rust
use glob::glob;

pub fn expand_macro(blueprint_cmd: &str, pattern_1: &str, pattern_2: &str) -> Vec<String> {
    // Resolve OS wildcards
    let matches_1: Vec<String> = glob(pattern_1).unwrap().filter_map(|e| e.ok())
        .map(|p| p.to_string_lossy().into_owned()).collect();
    let matches_2: Vec<String> = glob(pattern_2).unwrap().filter_map(|e| e.ok())
        .map(|p| p.to_string_lossy().into_owned()).collect();

    let mut script_buffer = Vec::new();

    // Compute Cartesian product
    for match_a in &matches_1 {
        for match_b in &matches_2 {
            script_buffer.push(format!("{} \"{}\" \"{}\"", blueprint_cmd, match_a, match_b));
        }
    }

    script_buffer
}
```

### Shell Pipeline Integration

The output prints a fresh newline (`\n`) for every distinct variation, allowing it to feed downstream processing loops or background execution pools gracefully:

```
ion> commandx (abc<*d", f*>) | while read execution_line
    echo "Running generated task..."
    eval $execution_line
end
```

By embedding this combinatorial parser directly into the shell's native tokenizing pass, LLVM can optimize the nested loops into vectorized instructions, letting the shell map out thousands of file-processing commands across directories instantly without a single millisecond of UI hitching.

## 5. OS Abstraction Layer Matrix (Redox/POSIX → Windows)

To bypass the requirement of a Virtual Machine, platform-specific POSIX calls from the original Ion codebase must be translated into direct Win32 APIs during compilation.

| POSIX / Redox Native Call | Target Windows Translation Strategy |
|---|---|
| `fork()` / `execve()` | Abstraction via `std::process::Command` (calls Win32 `CreateProcessW` directly under the hood) |
| POSIX Signals (`SIGINT`, `SIGHUP`) | Handled via Windows Console Events (`SetConsoleCtrlHandler`) using the `ctrlc` crate |
| File Path Separators (`/`) | Abstracted dynamically via `std::path::PathBuf`. Shell syntax accepts input with `/`, but translates internally to `\` for Windows compatibility |
| Unix TTY control sequences | Replaced by native Virtual Terminal (VT100) sequence handling via `crossterm`, targeting the Windows Console API directly (no emulation layer) |

## 6. Upgrade Roadmap (from the original Ion spec)

These are the specific upgrades applied on top of Ion's existing language design, which is otherwise preserved as-is (first-class typed variables, `@array`/`$string` sigils, method expansions, etc.):

1. **Native VT100 & Crossterm integration** — replace Ion's original Unix-focused line editor (`liner`) with `crossterm`/`ratatui` for flawless color, mouse, and text-reflow handling inside Windows Terminal.
2. **Native object bridge (structured data)** — allow pipes to carry structured maps/arrays (e.g. from JSON-emitting Windows tools) rather than forcing users to regex-parse flat strings.
3. **Path and environment normalization** — accept forward slashes in shell syntax, translate to Win32 paths seamlessly under the hood.
4. **Asynchronous execution (the Tokio upgrade)** — rewrite the synchronous job-management/process-spawning loop around `tokio` + `tokio::process` for concurrent background tasks with zero UI lockups.

## 7. Non-Goals / Explicitly Rejected Options

- **LiteDB**: written in C# for .NET — would require embedding a .NET runtime or a heavy interop layer, destroying native LLVM compilation goals.
- **DuckDB**: excellent for OLAP/SQL-on-files, but as a C++ codebase it adds 30MB+ to the binary and drastically increases compile times — contrary to the "tiny, lean system shell" identity. May be offered later as an optional `analytics` Cargo feature, never in `default`.
- **Structured pipelines as a v1 requirement**: Nushell/PowerShell-style typed pipes are acknowledged as superior, but are staged as a post-1.0 upgrade (Section 6, item 2) rather than a blocking requirement for the initial native port.

## 8. Known Language-Fidelity Gaps

Deliberate, documented deviations from the upstream Ion manual — not oversights:

- **Bitwise NOT (`$((a ~ b))`)** — resolved by product decision rather than manual verification. The manual's arithmetic-operator table (page 31) lists this with a *two-operand* syntax and no worked example; standard bitwise NOT is unary (`~x`), and its intended two-operand semantics (bit-clear? NAND? something else?) were never resolved — upstream's own arithmetic logic lives in a separate git-hosted `calc` crate (`gitlab.redox-os.org/redox-os/calc`) not fetchable in this environment (see §11). Rather than leave it permanently unimplemented, the user made the call: treat `~` as standard unary bit-complement (`~x`, flipping every bit — Rust's `!` on `i64`), ignoring the manual's ambiguous two-operand form. Implemented in `src/arith.rs`'s `parse_unary`, same precedence tier as unary `-`/`+` (which, per this module's existing convention, binds *tighter* than `**`: `~2 ** 2` is `(~2) ** 2` = `9.0`, not `~(2**2)`), integer operands only — consistent with every other bitwise operator (`&`/`|`/`^`/`<<`/`>>`) in the same module.
- **`matches` semantics — the manual contradicts itself.** Page 50's "If Statements" prose describes it as "a regex-based boolean match" and its own worked `if`/`else if` example uses genuine regex syntax (`matches $foo '[A-Ma-m]\w+'`, character classes and quantifiers — meaningless as literal text). But page 76's dedicated builtin-reference entry describes plain substring containment ("makes the exit status 0 if the first argument contains the second"), with an example (`matches xs x` → true, `matches x xs` → false) that happens to be ambiguous between the two readings. `builtins::eval_matches` implements the regex interpretation, since it's backed by an actual runnable worked example in the manual, which this project's testing philosophy treats as authoritative over a terser, contradicting one-line description. Revisit if upstream ever reconciles the two.

## 9. Signal Handling (Ctrl+C)

Implemented in `src/jobctl.rs`, wired into `src/shell.rs` and `src/pipeline_exec.rs`. Two independent mechanisms, since Ctrl+C needs to reach two different kinds of "currently running thing" with no common substrate on Windows:

1. **A foreground external process** (single command or pipeline stage) — every child is spawned via `jobctl::new_command`, which sets the Windows-specific `CREATE_NEW_PROCESS_GROUP` creation flag. This isolates the child from the default console-wide Ctrl+C broadcast (which would otherwise also hit the shell itself and every other child it's ever spawned). `jobctl::register_foreground`/`unregister_foreground` track which PIDs are the *current* foreground job (background/`&`/disowned/`&!` children are never registered, matching real shell job-control semantics). The single `ctrlc::set_handler` callback installed in `main.rs` (`jobctl::request_interrupt`) forwards a **`CTRL_BREAK_EVENT`** — not `CTRL_C_EVENT` — to every registered PID via `GenerateConsoleCtrlEvent`.

   This is a hard Windows API constraint, not a design choice: `GenerateConsoleCtrlEvent` can only ever broadcast `CTRL_C_EVENT` to *every* process sharing the console (`dwProcessGroupId` must be `0`); targeting a specific process group is only supported for `CTRL_BREAK_EVENT`. Since the child's own PID doubles as its process group ID (a consequence of `CREATE_NEW_PROCESS_GROUP`), `CTRL_BREAK_EVENT` is the only way to signal *just* that child without also killing the shell. Any program with no custom console-control handler terminates on `CTRL_BREAK_EVENT` exactly as it would on `CTRL_C_EVENT` (confirmed empirically: `timeout /t N` exits with `STATUS_CONTROL_C_EXIT`, `0xC000013A` / `3221225786`, on interrupt). A minority of programs install a custom handler that gives Ctrl+Break different semantics — `ping.exe` is the textbook example: it prints an interim statistics block ("Control-Break") and keeps running, mirroring Unix ping's `SIGQUIT` vs `SIGINT` distinction. This is that program's own documented behavior, not a bug in ion-win, and there is no Windows API-level workaround for it.

2. **A pure-Ion loop with no external process at all** (e.g. `while true; end` with only builtins/expansions inside) — there's no OS process to signal, so `request_interrupt` also sets a cooperative `AtomicBool` flag (`jobctl::take_interrupt`). `exec_block`'s statement loop, the `while` arm, and `exec_for` each poll this flag once per iteration (checked *before* evaluating the loop condition, so an empty-bodied loop is still interruptible even though its body never actually executes). A `Flow::Interrupted` signal propagates outward the same way `Break`/`ShellExit` do, through every exhaustive match on `Flow` (`exec_if`, `exec_for`, `call_function`, the top-level `run`/`run_script` loops). Interactively this prints `^C` and returns to a fresh prompt; non-interactively (`run_script`) it exits with code `130` (`128 + SIGINT`, matching bash/POSIX convention).

Verified via `scripts/exercise/ctrlc_pure_loop.ion`, `scripts/exercise/ctrlc_external.ion`, and manual testing in both interactive and script modes.

## 10. Scopes

Implemented in `src/interp.rs` (`Interpreter`'s scope-stack methods) and `src/shell.rs` (`exec_block`, `call_function`). Follows ion-manual page 20 ("Scopes") exactly, reproduced verbatim as a unit test (`interp::tests::scope_teardown_matches_manual_worked_example`) and a real-binary smoke test.

**The data structure.** `Interpreter::scalars`/`arrays` are each a `Vec<HashMap<String, _>>` — a stack of frames — instead of one flat map. Index 0 is the permanent global frame, created once and never popped.

**Where frames get pushed/popped.** `exec_block` is the single place that knows about scope lifecycles: every call pushes a fresh frame before running its lines and pops it afterward, on every exit path (normal completion, `break`/`continue`/`exit`, or an interrupt) — done by splitting the function into a thin scope-managing wrapper (`exec_block`) around the actual statement loop (`exec_block_statements`), so the push/pop only needs one entry and one exit point regardless of how many `return`s the loop itself has. Since `exec_block` is what actually executes an `if` branch, one `while`/`for` iteration, or a function body, every nested construct gets correct lexical nesting for free — callers (`exec_if`, the `while`/`for` arms, `call_function`) don't do anything scope-related themselves. A consequence worth knowing: each loop iteration gets its *own* fresh scope (not one scope shared across the whole loop), so a variable first-`let` inside a loop body doesn't persist to the next iteration — this isn't explicitly worked out in the manual's single-shot `if` example, but matches how block-scoped `let` behaves in most modern languages, and was the natural, minimal-code-change reading of "if, while, etc all take a scope to execute."

**The ownership rule.** `let`'s exact documented semantics: `set_scalar`/`set_array` first search every visible frame, innermost to outermost, for an existing binding — if found, they update it *in place* (mutating whichever frame currently owns it) rather than creating a shadowed copy in the current frame. Only when the name doesn't exist anywhere does it get defined fresh in the current (innermost) frame, which is what makes it get destroyed when that frame's block ends. `get_scalar`/`get_array` do the same innermost-first search for reads. `drop` mirrors this: it removes a name from whichever frame currently owns it, not just the current one.

**Function calls get a harder isolation, not just a new frame.** Page 20 also states "Functions have the scope they were defined in" — lexical, not dynamic, scoping: a function body must not see (or accidentally mutate) whatever local variables happen to be active at its call site, only the global scope plus its own parameters/locals. A plain `push_scope()` isn't enough for this, since the ownership-rule search would still walk down through the caller's intermediate frames and find/update a same-named caller-local. Instead, `call_function` calls `isolate_global_scope()`, which uses `Vec::split_off(1)` to detach every frame above index 0 and hand them back as a value; the function body then runs against just `[global, fresh-param-frame]`, and `restore_scope()` splices the caller's frames back on afterward. Parameters are bound via `define_local_scalar`/`define_local_array` — a direct insert into the current frame — rather than `set_scalar`/`set_array`, specifically so a parameter always creates a fresh local binding and shadows a same-named global instead of overwriting it (the ownership rule is for `let`, not for parameter binding).

**Found and fixed along the way**: the manual's own Scopes worked example uses `test 1 == 1`, but `builtins::eval_binary` only accepted `=`, not `==` — a real, separate bug this verification pass caught and fixed (`==` is now a synonym for `=`).

## 11. `and`/`or` and `previous_status` (`$?`)

`and`/`or` (ion-manual page 51's "Complete List of Conditional Builtins" checklist) are the one item on that checklist with no dedicated reference page or worked example anywhere in the 87-page manual — unlike everything else there, their syntax couldn't be verified from the manual alone. Resolved by reading upstream Ion's actual Rust source directly (`Linux/ion-master/src/lib/shell/flow.rs`, `parser/statement/parse.rs`), which is a full local checkout of the real ion-master repo rather than just its rendered manual.

**What they actually are.** Not simple builtins — statement-level keywords, parsed the same way `not`/`time`/`!` are (a prefix on a line, recursively wrapping whatever statement follows). At runtime they read `previous_status` — Ion's `$?` equivalent, literally named `self.previous_status` in the real source: `and STMT` runs `STMT` only if the previous statement succeeded (and `STMT`'s own result becomes the new status); `or STMT` runs it only if the previous statement failed. If the guard doesn't hold, nothing runs and the prior status is left untouched (matching upstream's `Condition::NoOp`) — this is what makes `false; and echo a; and echo b` correctly print neither: the first `and` doesn't update the status, so the second `and` still sees the original failure.

**Where they're wired**: `dispatch` in `shell.rs`, right alongside `exit`/`break`/`continue` — recognized as the first token of *any* top-level statement, not restricted to `if`/`while` headers. Implementation reconstructs the rest of the original line (`line.trim_start()[cmd.len()..].trim_start()`) and recurses into `dispatch` on it, rather than re-joining tokens, so quoting/spacing round-trip exactly as typed.

**`previous_status` itself** is new state on `Interpreter` (defaults to `true`, matching a fresh shell's `$? == 0`). `dispatch` resets it to `true` at the start of every statement that isn't itself `and`/`or` (see §14 for why this default was added after `&&`/`||` surfaced a real bug in the original design), and specific arms override it with their real result: condition builtins used as a statement (`test`, `matches`, `bool`, `contains`, `starts-with`, `ends-with`, `eq`/`is`, `exists`, `true`, `false`), `cd` (including implicit cd), external processes (`run_external`, now returning its success bool instead of `()`), and pipelines (`pipeline_exec::run`'s already-existing bool return, previously discarded in `dispatch`). `if`/`while` condition-header evaluations (`eval_condition_tokens`) still don't write their overall result back to `previous_status`, so `and`/`or` chaining off an `if` block's own condition (as opposed to a plain preceding statement) isn't covered — extend if broader `$?` fidelity is ever needed.

**Verified**: a real-binary smoke test covering success/failure chaining, chained `and`/`and`, short-circuit-on-failure, external process exit codes, and `cd` failure — all matched expected behavior exactly.

**Bonus finding from the same source dive**: `matches`'s implementation (`src/lib/builtins/mod.rs`) is unambiguous — real `Regex::new(...)`/`re.is_match(...)` code, confirming the regex interpretation already chosen in §8 above over the manual's contradicting one-line description was correct. Bitwise NOT remains unresolved: its real logic lives in a separate git-hosted crate (`gitlab.redox-os.org/redox-os/calc`) not vendored in this checkout and not fetchable in this environment.

## 12. `match`/`case`

Implemented in `src/shell.rs` (`exec_match` plus its header-parsing helpers). Follows ion-manual pages 56-57 exactly — all three of the manual's worked examples reproduced as a real-binary smoke test, plus match guards and the single-line inline form.

**Wiring.** `match` joins `if`/`while`/`for`/`fn` as a recognized block opener (`is_block_opener`) and gets its own arm in `exec_block_statements`, extracting its body via `extract_block` (the same helper every other block construct uses) and handing off to `exec_match`.

**The unified match rule.** The manual gives three worked examples — string-subject/string-case (equality), string-subject/array-case (subject is *in* the array), array-subject/string-case (case value is *in* the array) — that all collapse into one rule: expand both the subject and each case's pattern into a `Vec<String>` "value set" (a bare scalar becomes 1 element, an array becomes N), and a case matches if the two sets share any element. Array-subject/array-case has no worked example, so treating it the same way (shared element = match) is an inferred, clearly-documented extension rather than a verified one.

**A real bug caught along the way**: the value-set expansion initially used `Interpreter::expand_all`, which does *not* understand `[ ... ]` array literals on its own (confirmed by an existing doc comment on `resolve_method_arg` in `interp.rs` — this is a known, established quirk, not new). A pattern like `case [ four five six ]` was silently expanding to the single literal string `"[ four five six ]"` instead of three elements, so nothing ever matched. Fixed by expanding through `array_from_token` instead (the same helper `call_function`'s parameter binding already relies on for this exact reason) via a small `expand_match_operand` wrapper.

**Header parsing.** A `case` line's text (after stripping the literal `case ` prefix) can carry an optional match guard (`case PATTERN if CONDITION`, page 57) and/or a single-line inline statement (`case _; echo "not found"`, page 56 — notably with *no* space required before the `;`, ruling out simple whitespace-token splitting). Handled by two small quote/bracket-aware scanners (`split_at_top_level_semicolon`, `split_case_guard`) rather than reusing `Interpreter::tokenize` directly, since the tokenizer doesn't treat `;` as a token boundary at all (it would glue `_;` into one bareword) and guard/pattern text needs to survive re-tokenization later with its quoting intact.

**Body splitting.** Structurally mirrors `exec_if`'s `else`/`else if` splitting: body lines are grouped into branches at each top-level `case` line (depth-tracked via `is_block_opener`/`end`, so a nested `if`/`while`/`for` inside a case's body doesn't get mistaken for a new case boundary or the match's own closing `end`) — verified with a real nested-`if`-and-`for`-inside-a-`case` smoke test. Only one `end` closes the whole `match`, not one per `case`, matching the manual's examples exactly.

## 13. Job Control: `jobs`/`wait`/`disown`, deliberately not `fg`/`bg`

Implemented in `src/jobs.rs` (the registry) plus `src/pipeline_exec.rs` (registration) and `src/shell.rs` (the three builtins). Only the half of job control that maps cleanly onto Windows.

**The scope decision.** The manual's `fg`/`bg` semantics ("resuming it if it has stopped") assume POSIX job control — `SIGTSTP`/`SIGCONT`, a job that's been *stopped* and needs resuming. Windows has no clean equivalent (no standard signal-based process suspension), and ion-win doesn't implement job-*stopping* at all (matches the manual's own Unix-only "Suspending the Shell" page). Rather than ship an `fg`/`bg` that quietly does something smaller than what the name promises, they're skipped entirely. `jobs`, `wait`, and `disown` don't have this problem — they're pure bookkeeping (list/wait-for/stop-tracking), no signals involved.

**The registry** (`jobs.rs`) is a `Mutex<Vec<Job>>` behind a `OnceLock`, the same shape as `jobctl.rs`'s foreground-PID registry but holding the actual `Child` handles (not just PIDs), since `wait` needs to call `.wait()` on them and `jobs` needs to `.try_wait()` them to prune finished ones. Deliberately a *separate* module from `jobctl.rs` rather than folded into it: `jobctl` is specifically about the *current foreground* job and Ctrl+C signal delivery; `jobs` is about *background* jobs the shell keeps a longer-lived roster of — different lifetimes, different concerns, despite both being "job-related."

**Where registration happens**: `pipeline_exec.rs`'s existing background-handling branch (`pipeline.background || pipeline.disown`), which previously just printed a count and let the `Vec<Child>` drop (silently killing ion-win's only handle to those processes, though the OS processes themselves kept running untracked). Now, plain `&` (`pipeline.background`) registers each child into `jobs::register` before that branch returns; `&!` (`pipeline.disown`) still does nothing beyond the printed count — a disowned job was never meant to be tracked in the first place, so there's nothing to register.

**`disown`'s exact semantics**: the manual's `-a` flag is documented as "if no job IDs were supplied, remove all jobs" — extended here to apply the same rule to a bare `disown` with no flags at all, rather than inventing bash's unrelated "disown the most recent job" default that this manual never mentions. `-h` (don't forward SIGHUP to the disowned job) is accepted but a no-op, since Windows console apps have no real SIGHUP equivalent for ion-win to forward.

**Verified** via a real-binary test: start a background job, confirm `jobs` lists it with its PID and command text, `disown -a` it and confirm `jobs` goes empty, confirm a separately-started `&!` job never appears in `jobs` at all, and confirm `wait` genuinely blocks — proven via a marker-file race (the background job writes a file after a delay; the test checks the file exists *immediately* after `wait` returns), not just "the command didn't error."

## 14. `&&`/`||` as literal symbols

Implemented in `src/shell.rs`. Confirmed against upstream Ion's real parser (`Linux/ion-master/src/lib/parser/statement/splitter.rs`) that `cmd1 && cmd2` is *exactly* equivalent to `cmd1` on one line followed by `and cmd2` on the next — upstream's own splitter literally rewrites one into the other (`get_statement` wraps the trailing segment in `StatementVariant::And`/`Or` based on which operator preceded it). So neither of `&&`/`||`'s two call sites needed new runtime semantics — only a way to find the split point, since `and`/`or`'s existing `previous_status`-based logic (§11) already does the actual work.

**Two splitters, not one**, because the two call sites have genuinely different needs:
- `dispatch` (top-level statements) uses `split_at_top_level_chain_op`, a raw-string char scanner (quote/bracket-aware, same shape as `match`/`case`'s header scanners in §12) that returns real substring slices of the original line. This matters: `dispatch` recurses by re-tokenizing a string, so the split needs to hand back *exactly* what the user typed, not text reconstructed from already-parsed tokens (which would have lost quote characters and exact spacing).
- `eval_condition_tokens` (`if`/`while` headers) uses `split_chain_op_tokens`, a plain search over an already-tokenized `&[Token]` slice. No nesting-depth tracking needed here — confirmed empirically that `Interpreter::tokenize` already collapses every quote/bracket/expansion into one atomic token before this ever runs, so `&&`/`||` between two conditions always shows up as its own standalone `Quoting::None` token.

**Short-circuiting** falls out of Rust's own `&&`/`||` for free in `eval_condition_tokens` (`left && eval_condition_tokens(after, ...).await` — the right side, including any external process it would spawn, is never evaluated unless the left side leaves the answer undecided), and out of the early-return structure in `dispatch`.

**A real bug this surfaced, not just a missing feature**: `false || echo "recovered" && echo "then this too"` only printed `"recovered"` on the first pass. Root cause: `echo` was one of the statements `previous_status` deliberately didn't touch (§11's original design), so after it ran, the `&&` right after it was reading a *stale* status left over from `false` two statements earlier, not "did echo succeed." Fixed by inverting the default: `dispatch` now resets `previous_status` to `true` at the start of every statement except `and`/`or` (which must read the prior value first), and the specific arms with a real failure mode override that default — rather than only setting it for a curated allowlist and leaving everything else stale. Verified against the manual's own worked example (`if test $foo = "foo" && test $bar = "bar"`, both the matching and non-matching case), top-level `&&`/`||` chains including short-circuit-on-failure, multi-step chains, and the mixed `cmd1 || cmd2 && cmd3` case that exposed the bug — plus confirming plain `&`/`&!` backgrounding still works unconfused with the new `&&` detection (they're structurally distinguishable: a repeated-character scan for `&&` never matches a lone trailing `&`).

## 15. Brace permutation expansion

Implemented in `src/ranges.rs` (`expand_braces`), replacing the old narrow entry point that only handled a bare, whole-token `{range}` (`expand_brace_range` alone, called only when the *entire* token text was exactly `{...}`). The manual's actual primary form (pp.29-30) is an infix attached to surrounding literal text — `filename.{ext1,ext2}` — which the old code never matched at all.

**Design**, verified against both the manual's worked examples and, since the manual stops at simple cases, upstream Ion's own brace-expansion test suite (`Linux/ion-master/tests/braces.ion`/`.out` — real Ion's test fixtures, not just its docs):
- `parse_top_level_segments` scans a token's raw text into alternating literal/group segments, tracking `{`/`}` depth so a top-level group's *raw* content (including any nested braces, un-recursed) is captured whole.
- Each group's content is split on top-level commas (`split_top_level_commas`, depth-aware over nested braces, so `"A{1,2},B{1,2}"` splits into two pieces, not four).
- Each comma-separated element is tried as a range first (`expand_brace_range`, §-adjacent, unchanged), then recursively as its own nested brace expansion (`expand_braces` again — this is what makes `job_{01_{out,err},02_{out,err}}.txt` work), falling back to a literal. This mirrors upstream's real `expand_brace` (`Linux/ion-master/src/lib/expansion/mod.rs`), which does the identical three-way fallback per node.
- Literal segments and each group's resulting option list are then combined into the full cross product (plain nested loops — no need for upstream's `Permutator`/`SmallVec` machinery at ion-win's scale). Multiple groups in one word multiply out correctly (`job_{01,02}.{ext1,ext2}` → 4 results) and cross-checked against upstream's own harder cases: `1{A{1,2},B{1,2}}` → `1A1 1A2 1B1 1B2`, and `It{{em,alic}iz,erat}e{d,}` (nested group directly followed by literal within one comma-branch, plus an empty comma-branch meaning "or nothing") → `Itemized Itemize Italicized Italicize Iterated Iterate` — both reproduced exactly.
- `{}` (empty group, zero content) is left untouched as literal text rather than silently vanishing — not manual-documented either way, but the more intuitive behavior and consistent with `find`-style `{}` placeholder idioms staying literal.

**A real collision this surfaced and had to be guarded against**: this codebase already uses `${name}`/`@{name}` braces for variable-name disambiguation (`Interpreter::interpolate`, e.g. `${name}suffix` so the `_suffix` doesn't get swallowed into the variable name the way bare `$name_suffix` would). A naive brace-group scanner would misparse that `{name}` as a single-element permutation group with no comma — technically valid syntax — and silently collapse it, dropping the disambiguating braces and reflowing `${name}suffix` into `$namesuffix` (wrong variable). Fixed by never opening a permutation group on a `{` immediately preceded by `$` or `@` in `parse_top_level_segments`; that fragment is instead copied through verbatim (up to its own `}`) as ordinary literal text, left for `interpolate` to handle exactly as before. Verified both directions: `${name}suffix` still resolves the intended variable, and `${name}.{a,b}` (both syntaxes in one word) correctly expands only the permutation half.

Expansion order is brace-permutation first (pure text substitution), then `$`/`@` interpolation on each resulting word (`interp.rs`'s `expand_token` calls `self.interpolate` on every item `expand_braces` returns) — so `$name.{a,b}` with `$name` = `"report"` correctly produces `report.a report.b`, matching real shell semantics where brace expansion is lexical and happens before parameter expansion sees the result.

## 16. `PROMPT` function

Implemented across `src/shell.rs` (`render_prompt`/`sync_pwd`, wired into `run`'s REPL loop) and `src/interp.rs` (`Interpreter::begin_echo_capture`/`end_echo_capture`/`echo_output`, `split_echo_no_newline_flag`). Reproduces the manual's own worked example (repl.md, "Prompt Function", p.6) exactly:

```sh
fn PROMPT
    echo -n "${PWD}# "
end
```

**Two real prerequisites, not just the headline feature**, discovered by trying to actually run this example rather than assuming it would just work once `PROMPT` was recognized:
- **`echo -n` didn't exist anywhere in the codebase.** Every one of `echo`'s four call sites (`shell.rs`'s two dispatch arms for top-level statements and `if`/`while` condition context, `pipeline_exec.rs`'s pipeline-producer stage, and `interp.rs`'s `$(cmd)`/`@(cmd)` capture shortcut) unconditionally appended a newline, and none stripped a leading `-n` from `args` — meaning `echo -n "x"` would have printed the literal text `-n x`. Fixed once, centrally: `split_echo_no_newline_flag` (a free function in `interp.rs`, since it's needed by both `shell.rs` and `pipeline_exec.rs`) strips a leading run of `-n` tokens and reports whether the newline should be suppressed; all four call sites now go through it. Only `-n` is recognized — real Ion's `echo` also has `-e`/`-s`, but neither appears anywhere in the manual (only `-n`, via this exact example), so they're out of scope.
- **`$PWD`/`${PWD}` didn't exist as a variable.** `get_scalar` only ever walked the interpreter's own scope stack, with no OS-environment fallback, so `${PWD}` would have failed as "variable does not exist" even with `echo -n` working. Rather than hook every directory-changing call site individually (`cd`, implicit cd, `dmark jump`, and any future one), `sync_pwd` refreshes a global-scope `PWD` scalar from `std::env::current_dir()` once, right before every prompt render — always correct regardless of *how* the directory last changed, verified via a real-binary test that `cd ..` mid-session is reflected in the very next prompt.

**The capture mechanism**: real Ion generates the prompt by forking a subprocess running the `PROMPT` function's body and capturing its stdout. ion-win doesn't fork, so `Interpreter` gained an in-process equivalent instead — an `Option<String>` field (`echo_capture`) that `echo_output` (the single routine every `echo` call site now goes through) appends to instead of printing, when active. `render_prompt` brackets a normal `call_function("PROMPT", ...)` call with `begin_echo_capture`/`end_echo_capture`, so the function body runs through the exact same `exec_block`/scope-isolation path as any other function call — no special-cased execution logic, just a different destination for `echo`'s output. Deliberately narrower than a true stdout redirect: only `echo` is capture-aware (matching the one documented use case, and the same scope decision already made for `run_process_expansion_scalar`'s `$(cmd)` capture, §-adjacent) — a `PROMPT` body that calls `pvar`/`dmark`/another builtin that prints directly would leak that output to the real terminal instead of folding it into the prompt string. Falls back to the previous hardcoded `"ion> "` when `PROMPT` isn't defined, is defined with the wrong parameter count (`call_function` already rejects that with a printed error), or simply produces no output.

Verified via a real-binary test piping a multi-line session into `ion-win.exe`'s stdin (this exercises the real REPL loop, unlike `.ion` script-file mode, which never calls `render_prompt` at all): the default `ion> ` shows before `PROMPT` is defined; once defined, the prompt becomes the real current directory plus `"# "` with no trailing newline (confirmed by the next command's output appearing on the same line); and the prompt correctly updates after `cd ..` mid-session, proving it's re-rendered fresh each time rather than cached from definition time.

## 17. Structured pipelines: `from-json` / `select` / `where` / `to-json`

Implemented in `src/table.rs` (the `Table` type) and `src/pipeline_exec.rs` (three new `Kind` variants and a new `Carry::Table`). This is §6 item 2's "native object bridge" and §7's explicitly-deferred "structured pipelines as a v1 requirement" — the first post-1.0 feature, and unlike everything documented above, it's not driven by the manual at all: real Ion's pipes are flat text like every other POSIX-style shell, and this is ion-win's own extension for JSON-emitting Windows tools (`winget list --format json`, `Get-Process | ConvertTo-Json`, REST API list responses) that would otherwise need regex-parsing flat strings.

**Scoped to "table" shape, not full JSON.** A `Table` is an ordered list of flat records (`Vec<Row>`, `Row = Vec<(String, String)>`) — no arbitrary nesting. This covers the common case (most Windows CLI JSON output is naturally row-shaped) without needing a real path/query syntax (`.a.b[0].c`) to reach into arbitrary depth. A field whose JSON value is itself an object or array isn't rejected — it's kept as that value's own compact JSON text (`{"tags":["a","b"]}` → cell text `["a","b"]`), so building a table never fails just because one field happens to be non-scalar; pipe that field's text through `from-json` again if you need to dig into it. `from_json` accepts either a JSON array of objects (one row each) or a single bare object (a one-row table, matching how many tools emit a bare object for a single-item result rather than wrapping it).

**Why explicit adapters, not implicit auto-parsing.** `from-json`/`select`/`where`/`to-json` are separate pipeline stages, not one command that guesses. This mirrors how PowerShell (`ConvertFrom-Json`/`ConvertTo-Json`) and Nushell handle the same problem: structured values only flow *in-process*, between stages that understand them, with an explicit conversion at the boundary to anything that doesn't (an external process can only ever be handed bytes — you can't pipe a Rust struct to a process that doesn't share your address space). `select COL...`/`where COLUMN OP VALUE` only accept a `Carry::Table` as input — not raw bytes — so the pipeline reads unambiguously left to right: `tool --json` (bytes) `| from-json` (bytes → table) `| where pid -gt 1000` (table → table) `| select name` (table → table) `| to-json` (table → bytes). Skipping the trailing `to-json` still works — a `Table` reaching the end of the pipeline (or a redirect target) is pretty-printed as JSON automatically, and a `Table` handed to a subsequent external process is implicitly textified the same way (`Carry::Table`'s arm in `External`'s incoming-carry match) — but there's no silent auto-detection of JSON-looking bytes anywhere; `from-json` is always the explicit trigger.

**The real implementation snag**: `pipeline_exec.rs`'s existing zero-copy design hands an external stage's stdout straight to the next spawned process as a raw `Stdio` handle, without ever reading it in ion-win's own memory — that's what lets `type hugefile.txt | findstr foo` stream through the OS without buffering. But once a `ChildStdout` is wrapped as `Stdio` (for handing to a *new* child's stdin), it can no longer be read directly in-process — there's no getting the readable handle back out. Since `from-json`/`select`/`where`/`to-json` need to actually read the bytes, not just relay a handle, the decision has to happen at the exact moment an `External` stage's stdout is claimed: a one-stage lookahead (`next_stage_needs_materialized_input`, checking `kinds.get(i + 1)`) decides whether to keep the existing fast `Carry::Stdio` path (next stage is another `External`, or the pipeline end) or `read_to_end` into a `Carry::Bytes` instead (next stage is one of the four structured-pipeline kinds). This only changes behavior for pipelines that actually use the new builtins — an ordinary `External | External` chain is untouched.

**`where`/`filter` COLUMN OP VALUE** (row filtering by condition) reuses `test`/`if`'s exact comparison operators (`=`/`==`/`!=`/`-eq`/`-ne`/`-lt`/`-le`/`-gt`/`-ge`, `src/builtins.rs`'s `eval_binary`) directly via `Table::filter` calling `builtins::eval_test`, rather than reimplementing comparison logic — `where pid -gt 1000` behaves exactly like `test $pid -gt 1000` would, including the same numeric-parse-failure handling. `filter` is a plain alias for `where`, same as `eq`/`is` elsewhere in this codebase. A row missing the named column never matches (nothing to compare) rather than erroring. The operator is validated *before* calling into `eval_test` — an unrecognized one gets a clearly-attributed `where:` error instead of leaking `eval_test`'s own `test:`-prefixed one, which would otherwise misattribute the error to the wrong command. Like `select`, `where` only accepts a `Carry::Table` as input (not raw bytes), so it needed the same one-stage-lookahead treatment as `select`/`to-json` to materialize a preceding `External` stage's stdout when needed.

**Deliberately not built yet**: standalone (non-piped) use of `from-json`/`select`/`where`/`to-json` (they're only reachable via `pipeline_exec::run`, which requires an actual pipe — `pipeline::is_trivial()` routes a bare single-stage command through `shell.rs`'s own dispatch instead, where these names aren't registered, so e.g. bare `from-json '...'` fails as "command not found" rather than doing anything useful); and consuming a `Table` after a `^|`/`&|` (stderr or combined) pipe, which still produces an unread `Carry::Stdio`/`Carry::Merge` that `from-json`/`where`/`select` report a clear "not supported yet"/"expected a table" error for rather than mishandling silently. Storing a `Table` in a shell variable *is* now built — see §18.

Verified via `src/table.rs`'s unit tests (JSON-shape validation, nested-field stringification, column projection, numeric/string filtering, round-tripping) and a real-binary smoke test piping `echo`-produced JSON through every combination (`from-json` alone, `| select`, `| select | to-json`, `| where`, the `filter` alias, a full `from-json | where | select | to-json` chain, a bare single-object input, a nested-field input, and every error path — unrecognized operator, wrong argument count, `where`/`select` used before `from-json`) — output field order was initially wrong (alphabetically re-sorted instead of matching the source JSON) because `serde_json::Map` is `BTreeMap`-backed by default; fixed by enabling its `preserve_order` Cargo feature (backs it with `indexmap` instead), reconfirmed via the same smoke test.

## 18. Storing a `Table` in a shell variable

Implemented across `src/interp.rs` (a new `tables: Vec<HashMap<String, Table>>` scope stack on `Interpreter`), `src/pipeline_exec.rs` (`Kind::TableSource`, `run_capturing_table`), and `src/shell.rs` (`try_dispatch_let_table`). Closes the "deliberate v1 boundary" §17 called out: `let` didn't know `Table` existed as a value kind at all, so a table only ever lived transiently within one pipeline's execution.

**Storage**: `tables` is a fourth scope-stack field alongside `scalars`/`arrays`/`functions`, following the exact same shape and ownership rule as the other two (`set_table` walks every visible frame and updates in place if the name already exists, "first `let` owns it," matching `set_scalar`) — `push_scope`/`pop_scope`/`isolate_global_scope`/`restore_scope`/`builtin_drop` were all extended in lockstep so table variables tear down correctly with their scope, stay hidden-not-destroyed across a function call the same way scalars/arrays do, and `drop NAME` removes them too. `isolate_global_scope`/`restore_scope`'s return type grew from a 2-tuple to a 3-tuple; the one call site (`call_function`) needed no changes at all, since it's a pure `let saved = ...; ...; restore_scope(saved);` round-trip with no field access in between.

**`let NAME = PIPELINE` syntax**: this is the one genuinely new piece of control flow. `dispatch` (`shell.rs`) always checks `pipeline::parse(&raw_tokens).is_trivial()` on the *whole* line before ever looking at what the first word is — so `let procs = tool --json | from-json` would, unmodified, have been handed whole to that pipeline parser, which has no concept of `let NAME =` as a prefix; it would see stage 1 as `let procs = tool --json` (classified `Kind::Unsupported("let")`, since `let` is in `pipeline_exec.rs`'s blocklist) and abort the entire line with a "piping/redirection is only supported for..." error. `try_dispatch_let_table` intercepts *before* that gate: if the line is `let NAME = ...` and the right-hand side, parsed as its own pipeline, has a *last* stage whose command is one of `from-json`/`select`/`where`/`filter` or an existing table variable, the right-hand side (everything after `=`) is parsed and run on its own via a new `pipeline_exec::run_capturing_table`, and the resulting `Table` is stored under `NAME` via `set_table` instead of falling through to `builtin_let`'s ordinary scalar/array/arithmetic handling.

**Checking the *last* stage, not the first, is the detail that makes this actually useful.** The obvious real usage is `let procs = some-tool --json | from-json | where cpu -gt 5` — here the right-hand side's *first* word is `some-tool` (an ordinary external command), not a structured-pipeline builtin at all; `from-json` only shows up partway through. An earlier draft checked the RHS's first word and consequently failed to recognize this exact pattern (`echo '[...]' | from-json` fell through to the old "piping/redirection..." error) — caught via the real-binary smoke test before it shipped. Checking the pipeline's last stage instead correctly captures based on what the pipeline *actually produces*, matching how `finish_table_stage`'s own terminal-stage logic already decides whether there's a table to hand back.

**Why `to-json` deliberately opts out of capture**: `is_table_producing_command` matches `from-json`/`select`/`where`/`filter` and table variables, but not `to-json` — a `let` right-hand side ending in `to-json` is explicitly asking for JSON *text*, not a table, so it's intentionally left unintercepted (falling through to the pre-existing "piping/redirection..." error, unchanged from before this feature). This was also verified the hard way: an earlier version included `to-json` in the matched set, which meant `let x = tool | from-json | to-json` got intercepted, ran the pipeline, and produced *both* an unwanted JSON print to real stdout (since `Kind::ToJson`'s own terminal-stage logic isn't capture-aware — only `finish_table_stage`'s is) *and* a "did not produce a table" error afterward — a confusing double failure mode, fixed by excluding it.

**Reusing a stored table**: a bare table-variable name is recognized by `classify_stages` (checked via `interp.get_table(cmd)`, positioned after the fixed builtin names so a variable can't accidentally shadow `select`/`where`/etc.) and becomes `Kind::TableSource` — an independent producer stage, structurally identical to `Kind::Echo`: it ignores whatever fed it (`drop(incoming)`) rather than trying to merge the two, since referencing a stored table mid-pipe (`external.exe | mytable | ...`) wouldn't have a sensible merge semantics anyway. This is what makes `procs | where pid -gt 1000 | to-json` and `let big = procs | where ...` (deriving a *second* table variable from the first) both work with no additional special-casing — a table variable is just another valid pipeline source, and `let` capturing its result is the same `run_capturing_table` mechanism regardless of whether the source was `from-json` or another table variable.

**The capture mechanism itself**: `finish_table_stage` (already shared by `FromJson`/`Select`/`Where`/`TableSource`'s terminal-stage handling) gained an `Option<&mut Option<Table>>` parameter — when the pipeline's actual last stage is reached and there's a capture slot, the table is written there instead of printed. `pipeline_exec::run` and the new `run_capturing_table` are both thin wrappers around a shared `run_impl` that carries this optional slot through the stage loop, reborrowed once per loop iteration (`capture.as_mut().map(|s| &mut **s)`) since only one iteration will ever actually be the last stage.

Verified via a real-binary smoke test: capturing a `from-json`-only pipeline, reusing the stored table as a fresh pipeline source (`procs | to-json`), filtering it (`procs | where pid -gt 1000 | to-json`), deriving a second table variable from the first and further transforming *that* (`let big = procs | where ...` then `big | select name | to-json`), confirming ordinary `let x = 5` and `let arr = [ a b c ]` are completely unaffected, confirming a non-table-producing right-hand side (`let y = echo hi`) falls through to `let`'s pre-existing plain-scalar behavior untouched, and confirming a table variable persists correctly across later statements in the same scope.

**Deliberately not built**: no sigil/interpolation support for table variables (`$mytable`/`${mytable}` in an ordinary string context still errors "variable does not exist" — the only supported way to reference one is as a bare pipeline-stage name, or (§19) a `for` loop variable); `exists`/`intersects` don't know about table variables either. Both are reasonable follow-ups, not required for this slice.

## 19. `for VAR in TABLE` iteration

Implemented in `src/shell.rs`'s `exec_for`. Closes the last practical gap in reading a table variable back out: §18 gave tables a pipeline-source form (`mytable | select ...`) and a `let`-capture form, but the only way to actually *consume* one inside ordinary script logic — one row at a time, with real control flow around each — was still more piping. A `for` loop is the natural fit.

**Detection**: `exec_for` already special-cased nothing about its "in" clause before this — `for VAR in EXPR` always expanded `EXPR` as a flat scalar/array value (`interp.expand_all`). The new check runs first: if the "in" clause is *exactly one token* and that token names an existing table variable (`interp.get_table`), the loop iterates the table's rows instead. Requiring exactly one bare token (not, say, `for row in @mytable` or `for row in mytable extra`) means a table and an ordinary expansion never look the same syntactically — `for x in @arr`/`for x in 1 2 3` are completely unaffected, verified via the real-binary smoke test.

**What gets bound each iteration**: not a scalar, and not the row's raw data — `VAR` is set (via `set_table`, the same "first `let` owns it" ownership rule as everywhere else) to a fresh **one-row `Table`** wrapping just that row. This is what keeps the loop body able to reuse every mechanism §17/§18 already built with zero new primitives: `row | to-json` prints it, `row | select name` projects it, `row | where pid -gt 1000` filters it (typically down to itself-or-empty, useful for a per-row conditional), and `let derived = row | where ...` captures a further transformation of just that row — all exercised in the real-binary smoke test, including a case that nests a `let`-table-capture *inside* the loop body.

**Scoping matches the existing scalar `for` exactly**: `set_table` is called in the same place the scalar path calls `builtin_let` — right before `exec_block(body, ...)`, which pushes the loop body's own frame. So `VAR` lives in the loop-owning frame (persisting/updating across iterations, like the scalar case), not the body's frame, and disappears when the `for` statement's own enclosing scope tears down. `break`/`continue`/`Ctrt+C` interrupt handling is untouched — identical `match` on `exec_block`'s `Flow` result as the scalar path, and the same per-iteration `jobctl::take_interrupt()` check for an empty-bodied loop.

**Deliberately not built**: no direct scalar field access inside the loop (`$name`/`$pid` aren't bound per-row — only the whole-row-as-a-table is; reading one field still means `row | select name | to-json`, which yields a JSON array of one object, not a bare string). A dedicated field-accessor is a reasonable follow-up but out of scope here, same reasoning as §17/§18's own "not built yet" boundaries: ship the narrower, genuinely useful slice first.

Verified via a real-binary smoke test: iterating a two-row table (both rows print correctly), `break` after the first iteration, a nested `let`-inside-the-loop deriving another table per row, confirming ordinary `for n in @arr` and `for x in a b c` are unaffected, and confirming a zero-row table iterates zero times without error.

## 20. `cat FILE...`

Implemented in `src/fs_builtins.rs` (`cat`, alongside `pwd`/`dirs`/`folders`/`files`), with dispatch wiring in `src/shell.rs` (`handle_cat`, standalone) and `src/pipeline_exec.rs` (`Kind::Cat`, as a pipeline producer). Not a documented Ion feature, and not needed on Unix — real Ion (and every other POSIX shell) just relies on the system's own `/bin/cat`. Windows has no equivalent standalone executable (`type` is a `cmd.exe`-internal command, not spawnable via `Command::new`), so before this, there was **no way at all** to get a file's contents into a pipeline — `somefile.json | from-json` (§17) simply didn't work; only `echo`-produced or process-piped text did. This surfaced while reviewing an external JSON-accessor spec against ion-win's actual structured-pipeline design (the spec assumed file loading, which nothing in ion-win could actually provide).

**One `Result<String, String>`-returning function, three call sites, for free.** `fs_builtins::capture(name, args) -> Option<Result<String, String>>` already existed as the shared implementation behind `pwd`/`dirs`/`folders`/`files`, consumed from two places: `shell.rs`'s `handle_fs_builtin` (standalone dispatch) and `interp.rs`'s `run_process_expansion_scalar` (the `$(cmd)` capture path, which already special-cases in-process builtins like `echo` since they have no real backing executable to spawn). Adding `"cat" => Some(cat(args))` to that same match means `$(cat file.json)` (scalar capture) works with **zero changes to `interp.rs`** — the existing generic fallback (`crate::fs_builtins::capture(&args[0], &args[1..])`) already tries every `fs_builtins` name. Only the third call site — `cat` as a pipeline *producer* (`cat file.json | from-json`) — needed new code: `Kind::Cat(Vec<String>)` in `pipeline_exec.rs`, classified alongside `Echo`, executing by calling the exact same `fs_builtins::capture("cat", ...)` and feeding the result into `Carry::Bytes`/print/redirect depending on position — the identical three-way branch `Echo`'s arm already uses.

**`let NAME = cat FILE | from-json` and `for row in (a cat-built table)` both work with zero additional code**, which is a direct payoff of two earlier design decisions holding up under a new combination:
- §18's `try_dispatch_let_table` checks the right-hand side's *last* pipeline stage, not its first — exactly the fix that made `echo '...' | from-json` capturable also makes `cat file.json | from-json` capturable, since `cat` being first doesn't matter.
- §19's `for VAR in TABLE` only cares that the named variable resolves to a `Table`, regardless of how it was populated.

**One deliberate asymmetry from `Echo`'s printing**: `Echo`'s arm always constructs a *synthesized* string and appends `\n` unless `-n` suppresses it. `Cat`'s arm never appends anything — a file's bytes end however they already end, and most text files already end in a newline, so reusing `Echo`'s "add a newline" logic would risk a spurious blank line at EOF for the common case. `handle_cat` (standalone dispatch) mirrors this: `print!`, not `println!`, plus an explicit stdout flush (the same reason `echo -n`, §16, needs one — Rust's stdout is line-buffered, so text with no trailing newline could sit unflushed until the next prompt read blocks on stdin).

**Multi-file behavior**: `cat a b` concatenates in argument order, matching real `cat`. Reading stops at the *first* unreadable file rather than skipping it and continuing with the rest (unlike GNU `cat`, which keeps going and reports a nonzero exit at the end) — every other error path in this module and in the structured-pipeline stages is fail-fast with no partial-success case, and a silently-incomplete concatenation would be a worse surprise than stopping. Files are read as raw bytes and decoded lossily as UTF-8 (`String::from_utf8_lossy`), consistent with how `from-json` already treats piped bytes elsewhere in this file.

Verified via `fs_builtins.rs`'s unit tests (single file, multi-file concatenation order, missing-argument usage error, missing-file error, stop-at-first-failure) and a real-binary smoke test exercising all three call sites end to end: standalone `cat`, piped into `from-json | select | to-json`, scalar capture via `$(cat ...)`, table capture via `let procs = cat FILE | from-json`, iterating that table with `for row in procs`, and both missing-file error paths (standalone and mid-pipe).

## 21. `stat FILE... [--hash sha256]` — and ion-win's first real use of threading

Implemented in `src/stat.rs` (the core logic), wired into `src/pipeline_exec.rs` (`Kind::Stat`, a `Table` producer alongside `FromJson`) and `src/shell.rs` (`handle_stat`, standalone use). This grew directly out of a "gather file info into a manifest" use case previously done in PowerShell, and a design conversation about where ion-win could actually put Windows' native threading to use — the answer turned out to be narrower and more useful than a general "run script code on a thread" feature would have been.

**Why this, and not generic concurrent script execution.** `Interpreter` (scalars/arrays/tables/functions) is built entirely around single-threaded `&mut self` ownership, with no synchronization anywhere — making arbitrary ion script run concurrently on another thread would mean answering real language-semantics questions (what shared state can a thread see, what happens when it mutates a variable, how do results join back) that are a much bigger project than this. Hashing many files, by contrast, needs none of that: each file's hash is a pure, self-contained computation with zero Interpreter/script state involved — a worker just needs a path in and a `(path, size, hash)` result out. That made it the one genuinely embarrassingly-parallel, CPU/IO-bound piece worth actually parallelizing, without touching the Interpreter's execution model at all.

**Columns**: `path`, `size` (bytes), `modified` (Unix epoch seconds — no datetime-formatting dependency needed for that), `is_dir`, and, only when `--hash sha256` is given, a column *named after the algorithm* (`sha256`, not a generic `hash`) so supporting more algorithms later doesn't need a column-naming redesign. Only `sha256` is recognized for now; anything else is a clear `stat: unsupported hash algorithm '...'` error rather than silently doing nothing.

**Two input modes, mirroring `cat`'s "args win" precedent**: explicit file-path arguments (`stat a.txt b.txt`), or, if none are given, newline-separated paths read from piped-in text — so `stat` composes with a future recursive file-lister (`find . --recurse | stat`) without needing that to exist yet, and is fully testable today via anything that emits one path per line (verified in the real-binary smoke test via `cat pathsfile.txt | stat --hash sha256`).

**The actual threading**: only the hashing step runs concurrently — `stat`-ing a file (`std::fs::metadata`) is one cheap syscall, not worth parallelizing on its own. `build_table` spawns one `tokio::task::spawn_blocking` per file (reusing ion-win's existing tokio runtime — `pipeline_exec.rs` is already fully async throughout — rather than hand-rolling a separate thread pool alongside it), collects the `JoinHandle`s in file order, and `.await`s them in that same order. Each task actually progresses concurrently on tokio's blocking-worker threads regardless of await order, so wall-clock time is bounded by the slowest file rather than the sum of all of them, while the resulting `Table`'s rows still come out in stable input order — verified directly with a real-binary-adjacent unit test (`build_table_preserves_input_order_across_concurrent_hashing`, 8 files hashed concurrently, asserting row order matches input order).

**A deliberate reversal of `cat`'s error policy.** `cat` fails the whole pipeline on the first unreadable file, because a partial concatenation would be silently wrong — reproducing exact bytes only means something if it's *all* the bytes. `stat` does the opposite: an unreadable file is skipped with a printed warning, and every other file's row still comes through. This isn't an oversight or an inconsistency with `cat` — it's the right call for what each command is actually for. `stat` describes a *batch*, and a file vanishing mid-scan (a real race during a directory walk) shouldn't discard results for every other file in it; a manifest tool that aborts entirely because one of ten thousand files got deleted between listing and stating it would be far more annoying than one that just notes the miss and moves on.

**`let`/`for` integration required zero new plumbing.** `stat` was added to `is_table_producing_command` (`shell.rs`, §18) and to `next_stage_needs_materialized_input` (`pipeline_exec.rs`, §17) — two lines total — and `let manifest = stat *.txt --hash sha256` / `for row in manifest` both worked immediately, the same payoff `cat` (§20) got from the same two integration points.

Verified via `src/stat.rs`'s unit tests (arg parsing, missing-file skip-not-abort behavior, a real sha256 hash check computed independently within the test rather than a hardcoded recalled digest, and the concurrent-hashing order-preservation test) and a real-binary smoke test covering: standalone `stat` with and without `--hash`, piped newline-separated paths via `cat`, `let`-capture followed by `where`/`select` filtering, `for row in` iteration, a missing-file case confirmed via a real hash cross-check against the well-known SHA-256 test vector for `"hello world\n"`, and all three error paths (unsupported algorithm, `--hash` with no value, unknown flag).

## 22. `find [--all] [--recurse] [PATH]`

Implemented in `src/fs_builtins.rs` (`find`/`walk_files`, alongside `pwd`/`dirs`/`folders`/`files`/`cat`), wired into `pipeline_exec.rs` (`Kind::Find`) and `shell.rs` (added to the existing `handle_fs_builtin` dispatch arm — no new handler function needed). This was the last deliberately-deferred piece of the original "gather file info into a manifest" plan: `stat` (§21) could describe files, but nothing could recursively *find* them in the first place.

**Files only, not directories** — matching the motivating use case (gathering files, piping into `stat`) — recursing *into* subdirectories when `--recurse` is given, but never emitting a directory itself as a result. Dotfiles are skipped unless `--all`/`-a` is given, mirroring `files`/`folders`'s existing convention exactly.

**A real bug the real-binary smoke test caught, not just the unit tests.** The first version built each result path from an accumulated prefix that only ever reflected subdirectories discovered *during* the walk — never the starting `PATH` argument itself. `find some_dir` would print bare `top.txt` instead of `some_dir/top.txt`. Standalone, this looked completely fine (the output printed a plausible-looking filename); the unit tests, which happened to assert against exactly this bare-name output, passed. It only broke visibly once chained into the actual target use case, `find some_dir --recurse | stat --hash sha256`: `stat` tried to open literal `top.txt` relative to the shell's cwd, where it doesn't exist (it's at `some_dir/top.txt`), and failed with a file-not-found error for every single result. Fixed by seeding the recursive walk's prefix with the given `PATH` itself (normalized to end in exactly one `/`) rather than starting it empty — `PATH` defaulting to `.` is the one exception, where an empty prefix (bare `top.txt`, not `./top.txt`) reads cleaner and is just as valid relative to cwd. The three existing unit tests had encoded the bug as correct behavior and needed updating alongside the fix, not just the implementation.

**Two more things worth calling out, both déjà vu from `cat`/`stat`**: a subdirectory that fails mid-walk (permissions, a race during the scan) is skipped with a printed warning rather than aborting the whole `find` — the same "batch operation, don't let one bad spot ruin the whole scan" reasoning `stat` uses (§21), not `cat`'s fail-fast (§20). And `find`'s pipeline-producer output gets exactly one synthesized trailing newline when printed or written (`Kind::Find`'s arm mirrors `Echo`'s policy) rather than `Cat`'s transparent zero-newline passthrough — because `find`'s output is a synthesized list of names, not raw file bytes being reproduced exactly.

**Standalone use, `$(find ...)` scalar capture, and piping into `stat`/`let`/`for` all needed no new plumbing beyond the fix above.** `find` slotted into the exact same `fs_builtins::capture` dispatch table `pwd`/`dirs`/`folders`/`files`/`cat` already use, so `shell.rs`'s standalone dispatch and `interp.rs`'s `$(cmd)` capture path both picked it up automatically — no `handle_find` and no `interp.rs` changes were written at all. `find . --recurse | stat --hash sha256 | to-json` and `let manifest = find . --recurse | stat --hash sha256` both work purely because `find` sits *before* `stat` in the pipeline, and every `let`/`for`-in-table check already looks at a pipeline's *last* stage, not its first (the same lesson learned twice already, in §18 for `echo | from-json` and again in §21 for `cat | stat`).

This closes the original PowerShell-replacement goal completely: `find . --recurse | stat --hash sha256 | to-json > manifest.json` is now one real, working ion-win pipeline — verified end to end via the real-binary smoke test (a small directory tree with a nested file and a dotfile; non-recursive, recursive, and `--all` listings; `$(find ...)` scalar capture; the full `find | stat | to-json` chain producing a correct manifest with `path`/`size`/`modified`/`sha256` per file; `let`-capture followed by `select`; and both error paths).

## 23. `to-csv` / `from-csv`

Implemented in `src/table.rs` (`Table::to_csv`/`from_csv`, plus a hand-rolled `parse_csv_records`/`csv_escape`), wired into `pipeline_exec.rs` (`Kind::ToCsv`/`Kind::FromCsv`) exactly mirroring `Kind::ToJson`/`Kind::FromJson`'s existing shape. Grew out of a direct question — "why does the manifest have to be JSON, isn't `Table` itself better?" — and the honest answer: JSON was never chosen because it fits `Table`'s own shape best, it was chosen because `from-json`/`to-json` (§17) exist specifically for interop with JSON-emitting tools. If the actual goal is a file only ever meant to be read back as a table — or opened in Excel/pandas/`Import-Csv` — CSV is the more natural fit: no repeated column names on every row, one header line, and near-universal tabular-tool support that a bespoke ion-win-only format would never have.

**No new dependency.** Unlike JSON (`serde_json`, because getting JSON parsing exactly right is genuinely hard and not worth reinventing) or hashing (`sha2`, real cryptography), CSV reading and RFC4180-style quoting is well within "straightforward to hand-roll correctly" — a small character-by-character state machine (`parse_csv_records`) handles quoted fields containing literal commas, embedded quotes (doubled, `""`), and literal newlines, without needing a crate.

**`to_csv`'s column set is the first-seen union across every row, not just the first row's keys** — the same reasoning `select`'s "missing column" handling already established: `Table` never assumes every row shares the same columns (real-world JSON sometimes doesn't), so a row missing a column present elsewhere gets an empty cell rather than being silently dropped from the header entirely.

**A real, one-way lossiness versus JSON, stated plainly rather than hidden**: CSV has no way to represent "this row never had a value for this column" separately from "this row's value for this column is empty" — every row gets a cell for every header column, whether or not that row originally had one. Round-tripping a table where rows share identical columns is lossless (verified: `round_trips_through_to_csv_and_back_when_rows_share_columns`); round-tripping a table with genuinely differing per-row columns is not, and that's an inherent property of CSV itself, not a bug in this implementation.

**`from_csv`'s error policy**: a data row with *fewer* fields than the header just leaves the trailing columns absent from that row (mirroring `select`'s existing "absent, not empty-string-present" convention) — but a row with *more* fields than the header is a clear `from-csv` error, since there's no column name to attribute the extra value to and silently dropping it would be a worse failure mode than saying so. Empty input, or a header with zero data rows, both produce an empty table rather than an error.

**Wiring was almost entirely copy-adapt from `from-json`/`to-json`**: `Kind::FromCsv` was added to `next_stage_needs_materialized_input` (it consumes `Carry::Bytes`, same as `FromJson`); `Kind::ToCsv` was not (it consumes `Carry::Table`, same as `ToJson`); `"from-csv"` was added to `is_table_producing_command` in `shell.rs` so `let manifest = cat file.csv | from-csv` captures correctly, while `"to-csv"` deliberately was not, for the identical reason `"to-json"` isn't — it converts *out* of table form. One real difference from `ToJson`'s arm: `to_csv()` already ends every line (including the last) with its own newline, so `Kind::ToCsv`'s arm doesn't append an extra one the way `Kind::ToJson`'s does for `to_json()`'s output.

Verified via `table.rs`'s unit tests (header/row writing, quoting of commas/quotes/embedded newlines, first-seen column-union ordering, the JSON-vs-CSV round-trip distinction, short-row and too-many-fields handling, empty/header-only input) and a real-binary smoke test: JSON piped through `from-json | to-csv` (including a field containing a literal comma, correctly quoted), a full round trip back through `from-csv | to-json`, `let`-capturing a CSV-sourced table and filtering/projecting it, `for row in` iterating it, writing real `.csv` bytes to disk via a redirect and reloading them, and both error paths (`to-csv` with nothing piped in, `from-csv` on a genuinely malformed file with too many fields for its header).

## 24. `copy` / `cp` — the first file-operation builtin acting on a manifest

Implemented in `src/copy.rs`, wired into `shell.rs` (`handle_copy`, standalone) and `pipeline_exec.rs` (`Kind::Copy`). This is the first of a planned small family of manifest-driven file operations (copy, then compress, then possibly delete) — copy was deliberately chosen to go first, since unlike delete it can never destroy the source.

**Two forms, resolved at execution time by positional argument count, not by two separate `Kind`s.** `copy [--force] SRC... DEST` (explicit files — ignores whatever was piped in, matching `Cat`'s "explicit args win" precedent) and `TABLE | copy [--force] DEST` (sources come from the incoming table's `path` column — the exact column name `stat`, §21, already produces, so `manifest | where size -lt 1000000 | copy backup/` needs no separate scalar-extraction builtin at all, sidestepping a whole feature — reading one field out of a row as a bare value — that would otherwise have been a prerequisite). `Kind::Copy` strips `--force`/`--help` once, then branches purely on how many positional arguments remain: two or more means the explicit-files form; exactly one means the table-consuming form, requiring `Carry::Table`.

**Safer defaults than real `cp`, deliberately.** Real `cp` overwrites an existing destination silently. `copy` refuses unless `--force` is given — a considered choice, not an oversight: this is ion-win's own extension with no manual precedent to match, and overwriting is a (partial) form of data loss, worth requiring an explicit flag for. A source that fails to copy is skipped with a printed warning rather than aborting the whole batch, the same "batch operation, don't let one bad spot ruin the rest" reasoning `stat`/`find` already established — copying a manifest of thousands of files shouldn't abort entirely because one had a permission problem.

**A real bug, caught by the unit tests before it ever reached the smoke test this time**: the table-consuming form is supposed to preserve each row's full relative path under `DEST` (`backup/demo_project/sub/nested.txt`, not a flattened `backup/nested.txt` — deliberately different from the explicit-files form, which uses just the basename, since multiple files from a recursive `find` can share a basename in different subdirectories and flattening them would silently collide). The first version computed this via a plain `dest.join(path)`. `Path::join` has a sharp edge: when its argument is *absolute*, it replaces the base entirely rather than concatenating — so an absolute `path` column value would silently compute a target identical to the source itself, and since that "destination" already exists, every single file was reported as skipped with a spurious "destination already exists" error. Caught immediately by a unit test using real (necessarily absolute) temp-directory paths, before any real-binary testing was needed. Fixed by extracting the target-path computation into its own pure function (`table_row_target`, no file I/O), which strips any Windows drive-prefix/root-directory component from `path` before joining — a no-op for the already-relative paths `find`/`stat` normally produce, but now correct for an absolute one too. Testing this fix without mutating the test process's actual working directory (this project's own testing philosophy explicitly rules out `std::env::set_current_dir` in an in-process `#[test]`) is exactly why the pure-function split was worth doing, not just a refactor for its own sake.

Verified via `copy.rs`'s unit tests (single-file exact-destination copy, multi-file directory copy by basename, overwrite refusal and `--force` override, skip-a-missing-source-and-continue, the relative-path-preservation pure-function test, and the absolute-path-stripping fix both as a pure computation and end-to-end with real file I/O) and a real-binary smoke test covering both standalone forms, overwrite refusal, the full `find --recurse | stat --hash sha256 | copy backup/` chain with every copied file's relative location and content verified by reading it back, a filtered copy (`where size -lt N | copy ...`), and both error paths (no table piped in with no source args either, and an unrecognized flag).

**Made concurrent across files, reusing the exact `stat.rs` (§21) pattern**: both `copy_files` and `copy_table` now spawn one `tokio::task::spawn_blocking` per file (instead of looping `copy_one` in-line), awaited back in the same order they were spawned so the copied/skipped tally stays deterministic regardless of which finishes first — wall-clock time for a batch is bounded by the slowest single copy, not the sum of all of them. `copy_table` seeds the shared tally helper (`await_copies`) with a starting `skipped` count for rows rejected before any task is even spawned (a missing `path` column), rather than string-parsing a `summary()` result back into numbers — an early draft did exactly that (formatting a summary, then re-parsing "copied N" / "skipped N" out of it to combine two counts), caught as needless complexity before it was ever tested and replaced with a plain extra parameter. Verified as real cross-core execution, not just cooperative scheduling on one thread, via the same temporary `GetCurrentProcessorNumber()` diagnostic §21 used: copying 40 real 2&nbsp;MB files landed on 11 distinct physical cores across ~40 distinct OS threads, confirmed then removed before committing (not a permanent feature — the diagnostic exists only to prove the mechanism during development, same as §21's own verification). A new concurrency-specific unit test (`copy_files_concurrently_copies_many_files_correctly`) copies 32 files at once and checks every single one landed with correct contents, guarding against any future regression silently dropping or corrupting a file under concurrent access.

## 25. `compress` — ion-win's second manifest-driven file-operation builtin, defaulting to `.zip`

Implemented in `src/compress.rs` (new dependency: `zip = "8"`, `default-features = false, features = ["deflate", "time"]` — pulls in only a pure-Rust DEFLATE backend (`zlib-rs`/`zopfli` via `flate2`, no C compiler or system zlib needed) rather than the crate's much larger default feature set (`bzip2`/`lzma`/`xz`/`zstd`/`ppmd`/`aes-crypto`), none of which this builtin needs), wired into `shell.rs` (`handle_compress`, standalone) and `pipeline_exec.rs` (`Kind::Compress`). The second of the small planned family of manifest-driven file operations §24 introduced (copy, then compress, then possibly delete), and structured identically to `copy` in almost every way — same flag parsing shape, same two-forms-resolved-by-positional-argument-count dispatch, same `path`-column convention for its table-consuming form.

**Always produces a plain `.zip`, deliberately, with no `--format` flag.** Real Ion has no `compress` builtin at all to take a cue from, and Windows' own "Compress to ZIP file" Explorer action, WinZip, and 7-Zip all read/write the same standard zip format — so rather than inventing a flag nobody asked for, "the default archive format is a plain `.zip`" is itself the whole product decision here, matching how `copy` deliberately chose safer-than-`cp` overwrite defaults as its own considered, undocumented-by-the-manual call.

**Two forms, resolved exactly like `copy`.** `compress [--force] SRC... DEST.zip` (explicit files, flat archive, entries named by basename) and `TABLE | compress [--force] DEST.zip` (sources come from the incoming table's `path` column — the same column `stat`, §21, produces — with each entry stored under its relative path inside the archive, so extracting the result reconstructs the manifest's original directory structure, exactly mirroring why `copy_table` preserves relative paths on disk rather than flattening to a bare basename).

**The actual multi-core work: parallel DEFLATE compression, not just parallel I/O.** Unlike a file copy, compression is genuinely CPU-bound — compressing many files one at a time on a single thread wastes every core but one. But the `zip` crate's public `ZipWriter` API only supports *sequential* writes into a single archive (`start_file`/`write_all` compress internally as you write, and the format's central directory can only be assembled by one writer making one pass) — there's no supported way to hand it pre-compressed bytes for a brand-new entry. The design that gets genuine parallelism anyway, using only the crate's public, documented API rather than hand-rolling the zip format: each source file is independently compressed, in parallel via `tokio::task::spawn_blocking` (one task per file, the same pattern `stat.rs`/`copy.rs` established), into its own complete, self-contained *one-entry* in-memory zip archive (a real, valid archive on its own — separate zip entries never share compression state, so this is a legitimate unit of independent work). Once every file's mini-archive is built, a single sequential pass reads each one back with `ZipArchive::new` and splices its one entry into the real, final archive via `ZipWriter::raw_copy_file` — which copies the entry's already-compressed bytes across without re-running DEFLATE on them. So the expensive part (compression) runs fully in parallel across cores; the part that's genuinely sequential (assembling one archive's central directory) only ever copies bytes, never compresses them.

**Verified as real cross-core execution**, the same way §21/§24 were: a temporary `GetCurrentProcessorNumber()` diagnostic in `compress_one`, removed before committing, showed 40 real 2&nbsp;MB files landing on 40 distinct OS threads across 14 distinct physical cores, with the final archive still assembling correctly (all 40 entries present, extractable, correct contents).

**Entry naming for the table form** (`table_row_entry_name`) mirrors `copy.rs`'s `table_row_target` but produces a zip-internal name instead of a filesystem path: zip entry names are conventionally POSIX-style (`/`-separated, no drive letter or root) regardless of host OS, so `Component::Prefix`/`Component::RootDir` are dropped from the row's `path` before joining the remaining components with `/` — a no-op for the already-relative paths `find`/`stat` normally produce, correct for an absolute one too. `Component::ParentDir` (`..`) is dropped rather than specially handled, since `find`/`stat` never produce one; a deliberately unhandled edge case for a hand-built table with an unusual `path` value, not an oversight.

**Same safety default as `copy`, for the same reason**: refuses to overwrite an existing destination archive unless `--force`/`-f` is given. Checked once, up front, before any compression work even starts (unlike `copy`'s per-file check, `compress` only ever produces one output file). A source file that fails to read is skipped with a printed warning rather than aborting the whole archive, the same "batch operation, don't let one bad spot ruin the rest" reasoning `stat`/`find`/`copy` already established.

Verified via `compress.rs`'s unit tests (explicit multi-file archive with basename entries, table-consuming form preserving relative paths as entry names — including the absolute-path-stripping case, tested via real absolute temp-directory paths rather than `std::env::set_current_dir`, matching `copy.rs`'s own testing-philosophy fix — overwrite refusal and `--force` override, skip-a-missing-source-and-continue, a no-`path`-column row, and the 32-file concurrency regression guard reading every entry back out afterward) — every archive-content assertion reads the real `.zip` bytes back with the `zip` crate's own reader, not just trusting the writer — and a real-binary smoke test covering both standalone forms, the full `find --recurse | stat | compress backup.zip` chain, overwrite refusal and `--force`, both error paths (no table piped in with no source args, and an unrecognized flag/non-table input), and — critically, since this project's own reader passing isn't proof of real-world compatibility — extracting both produced archives with Windows' own built-in `Expand-Archive` (PowerShell), independent of any of ion-win's own code, confirming correct file contents and, for the manifest form, correct reconstructed directory structure.

## 26. Chaining `copy`/`compress` in one pipe

Both `copy` (§24) and `compress` (§25) originally consumed a `Table` and ended the pipeline there — the operation's result was a printed summary string (`"copied N file(s)"`), not the table itself, so a second manifest-driven operation couldn't follow in the same pipe: `manifest | compress out.zip | copy backup` would fail with `compress: expected a table`, since `compress`'s own output (the summary text) isn't a `Table`. Getting the same result meant three separate `manifest | ...` statements, each independently re-reading the stored table variable.

**Changed so the table-consuming form of `copy`/`compress` forwards the very same table it consumed to whatever comes next in the pipe**, rather than replacing it with the summary text. The explicit-files form (`copy a.txt b.txt dest/`) has no table to forward — it never had one — so that path is unaffected; only the `TABLE | copy DEST` / `TABLE | compress DEST.zip` form changes. Implementation-wise, `Kind::Copy`/`Kind::Compress`'s execution arms in `pipeline_exec.rs` now hold onto the `Table` they pattern-matched out of `Carry::Table` (via a local `forwarded_table: Option<Table>`, populated only in the table-consuming branch) and, after running the operation, set `carry = Carry::Table(t)` — the same table, unchanged, no clone — instead of the previous `Carry::Bytes(summary)`. This makes `manifest | compress out.zip | copy backup | to-csv > manifest.csv` (or ending in `to-json`, or another `copy`/`compress`) a single working pipeline: each stage acts on the same manifest and passes it straight through.

**Both stages now print their own status line immediately, regardless of pipeline position** — previously, being non-terminal meant the summary text silently became the next stage's input bytes instead of being shown to the user at all (moot before this change, since chaining didn't work anyway). `copy`/`compress` are side-effecting: whether files actually got copied/compressed is real information the user needs to see happen, unlike a pure transform (`select`/`where`) where only the final result matters. So unlike `select`/`where`/`from-json`, which only surface output when they're the pipeline's actual last stage, `copy`/`compress` always print (or write to a redirect target, if one is attached to that specific stage) as soon as they run, in addition to forwarding the table onward.

Verified via a real-binary smoke test: `manifest | compress out.zip | copy backup | to-json` prints compress's summary, then copy's summary, then the full manifest as JSON (proving the same table survived both side-effecting stages unchanged); a chain ending in `copy` with nothing after it (`manifest | compress out.zip | copy backup`) still behaves exactly as before this change (both summaries print, no table auto-dumped, since nothing consumes the final forwarded table); and `examples/backup.ion` (§ see `HANDOVER.md`) was rewritten to use this in one line — `manifest | compress backup.zip | copy backup | to-csv > backup_manifest.csv` — replacing three separate `manifest | ...` statements with one chained pipeline, reconfirmed end to end.

## 27. Tokenizer fix: pipe/redirect/chain/background operators split from adjacent words without whitespace

Found while debugging a user's real interactive session: `let manifest = find --all| stat` silently failed to create `manifest` at all, and using it afterward reported `command not found: manifest` — a confusing error pointing at the wrong thing entirely. Root cause was in `Interpreter::tokenize` (`src/interp.rs`), not in `let`/`stat`/`find`/anything table-related: the tokenizer's final fallback (a plain bareword reader) only ever stopped at whitespace, so `find|` — with no space before the pipe — was read as one single bareword token `"find|"` rather than two tokens `"find"` and `"|"`. `pipeline.rs`'s parser (documented in its own top comment, before this fix, as assuming "normal spacing") only recognizes a pipe/redirect by *exact* token text, so `"find|"` matched nothing, the whole line fell through as one plain (nonexistent) external command, and the failure surfaced two statements later as a totally unrelated-looking "command not found: manifest" — silent at the point of the actual mistake, confusing at the point it was noticed.

**This wasn't unique to `find`/`stat`/`compress`/`copy`** — every real shell treats `|`, `>`, `&`, etc. as metacharacters that always split into their own token regardless of adjacent whitespace (`cmd|cmd2`, `cmd>file`, `cmd&` are all valid without spaces in bash); ion-win's tokenizer never implemented that, it just happened not to matter until a user's actual typing habit exposed it.

**Fixed** by adding `operator_len(&chars) -> Option<usize>` (`src/interp.rs`), a pure lookahead (via `chars.clone()`, exactly the same non-destructive style already used for the `$(`/`@(` lookaheads elsewhere in this tokenizer) that recognizes, at the current position: `>>`, `&&`, `&|`, `&>`, `&!`, `||`, `^|`, `^>` (two characters) or a lone `|`, `>`, `&` (one character) — two-character forms checked first so `>>` isn't mistaken for two separate `>` tokens. Wired in two places: (1) a new branch at the top of the main tokenizer loop (alongside the existing quote/`[`/`$`/`@` special cases) that consumes and emits the operator as its own token the moment one starts; (2) the plain-bareword loop's stopping condition now also breaks on `operator_len(&chars).is_some()`, not just whitespace — so a bareword being read (`find`) stops the instant an operator character shows up next, rather than swallowing it.

**A bare `^` not immediately followed by `|`/`>` is deliberately left alone** — it isn't one of this grammar's defined operators on its own (only `^|`/`^>` are; `^` isn't a metacharacter in POSIX shells and Ion only borrowed it as the stderr-redirect prefix), so `a^b` as a bareword is untouched, avoiding any risk of breaking a literal caret that happened to appear in a word.

Verified via a new unit test (`operators_split_from_adjacent_words_without_whitespace`) covering every operator form with and without surrounding whitespace, plus the `^`-not-an-operator-alone edge case, and a real-binary smoke test: the user's exact original failing input (`let manifest = find --all| stat` followed by `manifest | to-csv > ...`) now works correctly, and a broader pass over every operator glued directly to adjacent words (`echo hi|echo piped`, `echo x>out.txt`, `echo x>>out.txt`, `true&&echo`, `false||echo`, `echo job&`) produces identical results to the already-passing spaced forms — confirming zero regressions in the existing pipe/redirect/chain-op test suites, all of which continued passing unchanged.

## 28. `Table` scalar accessors: `$len(table)` and `$field(row column)`

Structured tables were intentionally kept separate from scalar and array expansion in §18: implicitly converting a table to text would make pipeline behavior ambiguous and discard its row/column shape. That separation left two practical gaps, both encountered while using manifests: there was no direct row count, and `for row in manifest` could transform or serialize its one-row table but could not read one field as a scalar.

`$len(manifest)` now returns `manifest.rows.len()`. It is recognized only when `len` receives exactly one bare argument naming an existing table; strings, arrays, quoted literals, and all existing `$len` behavior continue through the ordinary `MethodArg` dispatcher unchanged.

`$field(row column)` is the matching narrow scalar bridge. Its first argument must be a bare table-variable name and that table must contain exactly one row, which makes it a natural fit inside `for row in manifest`. The column argument follows existing method-argument resolution, so it may be a literal column name or a scalar variable containing one. A zero-row or multi-row table is rejected rather than silently choosing a row, and a missing column is an explicit error rather than an empty string that could be mistaken for real data.

Both accessors live in `Interpreter::call_string_method_here`, ahead of ordinary string/array method dispatch. This avoids adding a `Table` variant to `MethodArg`, which would force every unrelated method to invent table-to-string and table-to-array coercion semantics. Three unit tests cover row counting, literal and variable column lookup, multi-row rejection, and missing-column rejection. The compiled executable was also verified with `scripts/exercise/11_table_accessors.ion`: a two-row JSON table reports `rows=2`, then row iteration extracts `alpha.txt 10` and `beta.txt 20` as scalars.

## 29. `HISTORY_IGNORE=no_such_command`

The documented default `no_such_command` history rule was previously accepted but inert because history was recorded before dispatch and execution returned only a success boolean. A normal nonzero exit must remain in history, so failure status alone cannot identify the rule's target.

`Interpreter` now carries a dedicated `command_not_found` signal, cleared before each simple interactive input and set only when process creation returns `ErrorKind::NotFound`. Both direct external commands and external pipeline stages set it; ordinary exit code failures do not. After execution, the REPL consults the live `HISTORY_IGNORE` array and removes the just-recorded line when the signal is set and `no_such_command` is enabled. `LineEditor::remove_last_history_if` only removes an exact latest match, preventing an older identical entry from being accidentally deleted. Multi-line blocks retain their existing per-line history recording because one aggregate execution signal cannot safely identify which of several entered lines failed.

Verified with unit tests for live rule detection and exact editor removal, the full suite, and a real piped interactive session against the compiled executable. With the default rules, `echo kept`, a deliberately nonexistent command, and `exit` produced a persisted history containing exactly `echo kept` and `exit`.

## 30. Quoted-array fidelity in nested and adjacent word contexts

Nested method arguments already re-tokenized their inner text, but this behavior was not protected by focused tests. Those tests now establish that `$len("@arr")` sees the quoted array as one joined string and `@reverse("@arr")` receives one scalar-coerced element rather than two array elements.

The real mismatch was adjacent quote concatenation. `prefix"@arr"suffix` was previously tokenized as one unquoted string containing literal quote characters, producing `prefix"one two"suffix`; a word beginning with quotes followed by a suffix was split incorrectly. Double-quoted segments are now consumed as part of the surrounding word, their delimiters are removed, and the resulting token retains double-quoted coercion. When a quoted segment ends in a variable immediately followed by a suffix, the tokenizer internally applies Ion's documented braced disambiguation (`"@arr"tail` becomes `@{arr}tail`) so interpolation does not look for a nonexistent `arrtail` variable.

Focused tests cover both prefix/suffix directions and nested method arguments. The compiled smoke script prints `adjacent prefixone twosuffix`, confirming one coerced word with no literal quote leakage.

## 31. Unicode grapheme-aware string operations

The old `@graphemes()` implementation was only an alias for `@chars()`, and string length/slicing counted Unicode scalar values. That splits user-perceived characters such as `e` plus a combining acute accent and multi-code-point emoji joined with ZWJ.

The `unicode-segmentation` crate now supplies extended grapheme cluster boundaries. `@graphemes()` returns those clusters; `$len(string)`, string `find` positions, scalar `reverse`, `split_at`, and `[...]` indexing/slicing use the same user-facing units. `@chars()` intentionally continues to expose scalar values, while `@bytes()` and `$len_bytes()` retain byte semantics, so every abstraction level remains available explicitly.

Tests use `e\u{301}👩‍💻`: it has two graphemes, multiple scalar values, and fourteen UTF-8 bytes. They verify counting, search position, reversal, splitting, grapheme enumeration, and forward/reverse slicing without separating the accent or emoji sequence. `scripts/exercise/12_language_fidelity.ion` confirms the same behavior through the compiled executable.
