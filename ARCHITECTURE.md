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
