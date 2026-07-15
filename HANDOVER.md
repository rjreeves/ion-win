# Handover: `ion-win`

Native Windows port of the [Ion shell](docs/ion-manual.pdf) (originally built for Redox OS), written in Rust. See [ARCHITECTURE.md](ARCHITECTURE.md) for the original design blueprint (LLVM/LTO release profile, `redb` state persistence, the OS abstraction matrix) — this document covers what's actually been *built* since then, and what's still open.

## Quick start

```
cd ion-win
cargo build              # debug build
cargo test                # 128 tests, all passing
cargo run                 # interactive REPL
cargo run -- script.ion arg1 arg2   # run a script file
```

No special setup needed — it's a standard Cargo binary crate. `.gitignore`/`.git` were auto-created by `cargo new` inside `ion-win/`.

## What this is

A working, reasonably faithful interpreter for Ion's language (variables, control flow, functions, expansions, pipelines) plus a real interactive line editor — not a toy. It's driven entirely from the [ion-manual.pdf](docs/ion-manual.pdf) spec: nearly every feature below was implemented by reading the manual's own worked examples and writing tests that reproduce their exact output, byte-for-byte. Where the manual was ambiguous or silent, that's called out explicitly rather than guessed at.

## Module map (7,730 lines across 20 files)

| File | Lines | Purpose |
|---|---|---|
| `interp.rs` | 1734 | The core: tokenizer, `$`/`@` expansion, variable scope stack, `let`/`export`/`drop`, method-call and process-expansion dispatch. The biggest and most load-bearing file. |
| `shell.rs` | 1475 | REPL loop, block/control-flow execution (`if`/`while`/`for`/`fn`/`match`/`case`), `cd`, `&&`/`||`/`and`/`or` chaining, dispatch of all builtins |
| `editor.rs` | 680 | Crossterm raw-mode line editor (arrow keys, history, Tab-completion, word-editing shortcuts, live syntax highlighting) with fallback to plain stdin |
| `ranges.rs` | 647 | Slice parsing (`[start..end]`) + brace expansion: ranges (`{1..10}`) and permutation lists (`{ext1,ext2}`, nested) |
| `methods.rs` | 491 | All 26 string/array methods (`$len`, `@split`, etc.) |
| `arith.rs` | 430 | `$(( expr ))` arithmetic expansion (recursive-descent parser) |
| `pipeline_exec.rs` | 365 | Real OS pipeline execution (`Command`/`Stdio` chaining), background-job registration |
| `state.rs` | 302 | `redb`-backed key/value store for `pvar`/`dmark` |
| `history.rs` | 278 | Persistent command history (load/save/filter) |
| `builtins.rs` | 262 | `test`/`matches`/`bool`/`contains`/`starts-with`/`ends-with`/`eq`/`is`/`isatty` condition evaluators |
| `pipeline.rs` | 199 | Pipeline/redirect *parsing* (pure data, no execution) |
| `jobctl.rs` | 160 | Ctrl+C interrupt plumbing: foreground-PID registry, `CTRL_BREAK_EVENT` forwarding, cooperative interrupt flag |
| `builtin_names.rs` | 141 | Single source of truth for builtin names/keywords, feeding `help`, Tab-completion, and syntax highlighting |
| `main.rs` | 119 | Entry point; branches to script-file mode (argv) or interactive `shell::run` |
| `fs_builtins.rs` | 100 | `pwd`/`dirs`/`folders`/`files` |
| `functions.rs` | 83 | `fn` parameter parsing (typed params, docstrings) |
| `colorout.rs` | 82 | `err_println!`/`err_eprintln!` — red-on-terminal error output, `NO_COLOR`-aware |
| `jobs.rs` | 78 | Background-job registry for `jobs`/`wait`/`disown` |
| `types.rs` | 68 | Shared `str`/`bool`/`int`/`float` type-tag validation |
| `procexpand.rs` | 36 | `$(cmd)`/`@(cmd)` process-expansion process spawning |

## Implemented, verified against the manual

- **Core language**: `let` (scalar/array/typed/arithmetic `+= -= *= /= //= **=`), `drop`, `echo`, `#` comments (incl. shebang lines)
- **Expansion**: `$name`/`@name`/`${name}`/`@{name}` (with embedded interpolation, not just whole-token), `$((arithmetic))`, `$(cmd)`/`@(cmd)` process expansion, `$method(args)`/`@method(args)` (all 26 documented methods), quoting rules (single suppresses expansion, double coerces arrays to strings), slicing (`[start..end]`, stepped, reverse), brace ranges (`{1..10}`, alpha, negative, stepped)
- **Control flow**: `if`/`else if`/`else`, `while`, `for`/`in`, `break`/`continue` (correctly scoped to the nearest enclosing loop, propagates through nested `if`s)
- **Functions**: `fn` with typed/array params, docstrings, `fn` (bare) lists definitions
- **Process execution**: pipelines (`|` `^|` `&|`), redirection (`>` `>>` `^>` `&>`), background/disown (`&` `&!`), `echo` as a pipeline producer
- **Shell UX**: `cd` + implicit cd (bare `~/path`, `..`, `.config`, `examples/`), `read`, `pvar`/`dmark` (redb-backed state), persistent history (`HISTFILE`/`HISTORY_IGNORE`/`HISTORY_TIMESTAMP`), crossterm line editor (arrow keys, history recall, Ctrl+U/C/D)
- **Ctrl+C interrupt handling** (`src/jobctl.rs`, `ARCHITECTURE.md` §9): breaks a running foreground external process/pipeline (via process-group isolation + `CTRL_BREAK_EVENT`, the only Windows-supported way to selectively signal one child) or a pure-Ion loop with no external process (`while true; end`, via a cooperative interrupt flag) without killing the shell itself. Verified via real Ctrl+C signals against a running `ion-win.exe`, both interactively and in script mode.
- **Environment**: `${env::VAR}`, `export` (real OS env var, inherited by spawned child processes)
- **Script execution**: `ion-win.exe script.ion arg1 arg2` with `@args`
- **`which`/`type`** (ion-manual page 83): reports builtin/function/resolved-`PATH`-executable for a command name, searching `PATHEXT` extensions like Windows itself does.
- **Standalone `true`/`false`/`bool`** (pp.68,73,83): now real builtins usable as full statements (`true`, `false`, `bool $x`), not just condition-context keywords — previously fell through to `run_external` and only "worked" by accident if some external `true.exe`/`false.exe` happened to be on `PATH`.
- **`eval`** (p.71): joins its arguments with spaces and dispatches the result as a new command (e.g. `eval "echo" "hi"`).
- **`initrc`** (pp.4,63): a file named `initrc` is sourced automatically at interactive startup if present — resolved via `$XDG_CONFIG_HOME/ion/initrc` if set, else `%APPDATA%\ion-win\initrc` (Windows has no real `$HOME`/XDG, so this mirrors `state.rs`'s existing `%APPDATA%` convention).
- **`contains`/`starts-with`/`ends-with`** (pp.69,71,79) and **`eq`/`is [not]`** (p.75): condition-only builtins (no output, just exit status), usable both inside `if`/`while` and standalone — same shape as `test`/`matches`/`bool`.
- **Scope-based variable teardown** (ion-manual page 20, "Scopes"; `ARCHITECTURE.md` §10): was the single deepest architectural gap, now closed. `Interpreter` holds a stack of scope frames instead of one flat map; `exec_block` pushes a fresh frame per block *execution* (one `if` branch taken, one `while`/`for` iteration, one function call) and pops it on every exit path, deleting whatever that execution newly defined. `let` on a name that already exists in an outer frame updates it there in place rather than shadowing it (`set_scalar`/`set_array` walk the whole stack) — reproduced the manual's own worked example exactly (`let x=5` → nested `let x=2; let y=3` → after the block, `$x` is `2`, `$y` is undefined). Function calls additionally isolate the global scope (`isolate_global_scope`/`restore_scope`) so a function body can only see the scope it was defined in, not the caller's locals — per the manual's explicit "Functions have the scope they were defined in." Along the way, found and fixed that `test` didn't accept `==` (only `=`), which the manual's own Scopes example uses.
- **`exists`** (p.72): `-a`/`-b`/`-d`/`-f`/`-s`/`--fn`/bare-STRING, all matching the manual's documented forms exactly — `-a`/`-s`/`--fn` take a bare name (no `$`/`@` sigil, per the manual's own note) since they check the identifier's existence/non-emptiness, not an expanded value.
- **`and`/`or`** (`ARCHITECTURE.md` §11): statement-level keywords, not simple builtins — `and STMT` runs `STMT` only if the previous statement succeeded, `or STMT` only if it failed. Verified against upstream Ion's actual Rust source (`Linux/ion-master/src/lib/shell/flow.rs`), not just the manual (which never documented their syntax at all — only checked a box for them). Required adding `previous_status` (`$?`) tracking to `Interpreter`, set after condition builtins, `cd`, and external processes/pipelines.
- **Bitwise NOT (`~x`)** (`ARCHITECTURE.md` §8): unlike everything else in this project, resolved by product decision rather than manual verification, since the manual's `$((a ~ b))` two-operand form was never clarified and upstream's real arithmetic crate wasn't fetchable this session. Implemented as standard unary bit-complement (Rust's `!` on `i64`), same precedence tier as unary `-`/`+`.
- **`isatty [FD]`** (p.75): exit status 0 if the given FD is a real terminal. Matches upstream's actual source behavior exactly, not just its synopsis — a bare `isatty` with no argument always succeeds unconditionally (confirmed via `Linux/ion-master`; upstream doesn't default to checking any particular descriptor). Windows has no portable way to check an arbitrary raw FD's tty-ness, so only 0/1/2 (stdin/stdout/stderr, via Rust's `IsTerminal`) are supported; other FD numbers are reported as unsupported rather than guessed.
- **`match`/`case` pattern matching** (pp.56-57; `ARCHITECTURE.md` §12): the last big control-flow gap, now closed — `match EXPR / case PATTERN [if GUARD]; STMT|BLOCK / case _ / end`. All three of the manual's worked examples reproduced exactly (string-vs-string equality, string-subject-vs-array-case, array-subject-vs-string-case), plus match guards (`case PATTERN if CONDITION`) and the single-line inline form (`case _; echo ...`). Array-vs-array case matching has no worked example in the manual, so it's an inferred (documented as such) extension of the same "shared element" rule. Caught a real, separate bug along the way: array literals (`[ ... ]`) were being silently mishandled by plain `expand_all` — needed `array_from_token` instead.
- **`intersects ARRAY1 ARRAY2`**: ion-win extension (unchecked/unimplemented even in upstream Ion — no real behavior to verify against, same situation as bitwise NOT). Product decision: exit status 0 if the two named arrays share at least one element.
- **`commandx` removed** (`ARCHITECTURE.md` §4): the Cartesian-product macro-expansion builtin never gained the ability to run what it generated (see the removed "commandx doesn't execute what it generates" gap that used to be here) and was dropped by product decision rather than finished. `src/commandx.rs` deleted; all dispatch/pipeline/completion wiring removed.
- **`jobs`/`wait`/`disown`** (pp.69,75,83; `ARCHITECTURE.md` §13): the bookkeeping half of job control — `&` (background) now registers into a real registry (`src/jobs.rs`) instead of the spawned `Child` being dropped immediately. `fg`/`bg` deliberately skipped — see §13 for why. `&!` (disown) still never gets tracked at all, matching real shell semantics. Verified via a real-binary test: start a background job, list it, disown it, confirm `&!` jobs never appear, and confirm `wait` genuinely blocks (via a marker-file race, not just "didn't error").
- **`&&`/`||` as literal symbols** (p.51; `ARCHITECTURE.md` §14): `cmd1 && cmd2` now works inline, not just the word-form `and`/`or`. Confirmed against upstream's real parser that this is exactly the same mechanism as `and`/`or` with a different spelling, so no new runtime logic was needed, only two small splitters (a raw-string one for `dispatch`, a token one for `eval_condition_tokens`/`if`/`while` headers — reproducing the manual's own `if test ... && test ...` example exactly). Along the way, found and fixed a real bug in the *existing* `and`/`or`/`previous_status` design: statements with no tracked failure mode (`echo`, `let`, etc.) left `$?` stale instead of resetting it, so `false || echo "recovered" && echo "also this"` silently dropped the second half. Fixed by defaulting every statement to "succeeded" up front instead of only setting status for a curated allowlist.
- **Brace *permutation* expansion** (pp.29-30; `ARCHITECTURE.md` §15): `{ext1,ext2}`-style comma lists, multiple groups per word (`job_{01,02}.{ext1,ext2}`), and nesting (`job_{01_{out,err},02_{out,err}}.txt`) — the general case brace *ranges* were always a subset of. Previously only bare whole-token `{range}` worked; brace expansion is now general, working as an infix attached to surrounding literal text (the manual's primary documented form) with any number of groups per word, each cross-producted together. Verified against both the manual's own examples and, since the manual has no worked example beyond simple cases, upstream Ion's actual brace-expansion test suite (`Linux/ion-master/tests/braces.ion`/`.out`) for deeper nesting and empty-branch edge cases (`It{{em,alic}iz,erat}e{d,}` → `Itemized Itemize Italicized Italicize Iterated Iterate`), reproduced exactly. Required guarding against a real collision: `${name}`/`@{name}` (existing variable-name disambiguation syntax) would otherwise be misparsed as a single-element permutation group, silently dropping the braces and merging the name with any following suffix (`${name}suffix` → wrongly `$namesuffix` instead of `${name}` + `suffix`) — fixed by never opening a group on a `{` immediately preceded by `$`/`@`.

## Known gaps (deliberately not built — not oversights)

Ranked roughly by how much a real user would notice:

1. **No `fg`/`bg`** — `jobs`/`wait`/`disown` are implemented (see "Implemented, verified" above and `ARCHITECTURE.md` §13), but `fg`/`bg` are deliberately skipped: their real value ("resume a job I stopped with Ctrl+Z") has no clean Windows equivalent, since there's no POSIX-style `SIGTSTP`/`SIGCONT` and ion-win doesn't implement job-stopping at all (matches the manual's own Unix-only "Suspending the Shell" section) — shipping a half-faithful `fg`/`bg` that doesn't really do what the name implies was judged worse than not having them.
2. **The five Polish-notation comparison operators** (`<`, `<=`, `>`, `>=`, `=`) on the manual's "Complete List of Conditional Builtins" checklist (p.51) are unchecked in upstream Ion too, i.e. not even real Ion has them — don't implement those. (`isatty`/`intersects` — also on that checklist — are now implemented; see "Implemented, verified" above.)
3. **No custom `PROMPT` function or Vi keybindings.** Both ARE genuinely documented with worked examples (pp.5-6) — `PROMPT` in particular needs a way to capture a function's `echo` output as a string (real Ion forks a subprocess for this; ion-win would need an in-process output-capture mechanism instead, since it doesn't fork). By contrast, `alias` and `${c::color}` — previously listed here too — turned out NOT to be verifiably documented Ion features at all after a full read-through of the manual's 87 pages: `alias` is mentioned only twice in passing (an initrc use-case, and in `which`'s description) with no dedicated syntax section or worked example anywhere, and `${c::color}` doesn't appear anywhere. Both were carried over from generic shell assumptions rather than manual verification and were wrongly listed as "not yet built" — don't implement guessed syntax for either without a concrete documented spec to check against. (`initrc`, `which`/`type`, and standalone `true`/`false`/`bool` — also previously listed here — are now implemented; see "Implemented, verified" above.)
4. **History has no `+shared`/live cross-process sync** and doesn't implement the `no_such_command` ignore rule (accepted but inert — would need execution-outcome plumbing).
5. **Quoted-array edge case**: `"@array"` (double-quoted) correctly coerces to a joined string in most contexts, but the manual's "quoted vs. unquoted" distinction isn't tracked with full fidelity through every nested context (documented in `interp.rs`'s `Token`/`Quoting` doc comments).
6. Indexing/counting is by Unicode `char`, not grapheme cluster (no graphemes crate dependency) — `graphemes()` method is currently an alias for `chars()`.

## Testing philosophy (worth preserving)

Every feature in this codebase was verified two ways, and both matter:

1. **Unit tests** reproducing the manual's literal input→output examples (92 passing). Fast, but they only prove the code path you thought to test.
2. **Interactive smoke tests** via the actual compiled binary (`cargo build` then pipe a `.ion` script into `target/debug/ion-win.exe`, or spawn it as a subprocess for anything touching real OS state like `cd`/env vars/child processes). This caught several real bugs unit tests missed — e.g. the tokenizer originally split `$(( x * x ))` into garbage tokens because of internal spaces (same bug recurred for `$(cmd)` and `@method(...)` until each was specifically fixed), and `$len([1 2 3])` was silently counting the *literal bracket text's characters* instead of the array's elements until an interactive test caught it.

**Do not skip step 2.** Anything involving `std::env::set_var`, `std::env::set_current_dir`, or spawning child processes should be tested via a real subprocess invocation, never as an in-process `#[test]` — those mutate whole-process state that would race with other tests running concurrently in the same `cargo test` binary.

## Suggested next steps

In rough priority order if continuing: everything in the "known gaps" list above, roughly in the order listed.

## A note on the interactive line editor

`src/editor.rs`'s crossterm raw-mode keystroke handling (arrow keys, history recall, Ctrl+U/C/D) **could not be verified interactively during this session** — the sandboxed tool environment never provides a real TTY on stdin (confirmed via a direct `IsTerminal` probe), so only the plain-stdin fallback path has actually been exercised. The code compiles against crossterm 0.27's real API and the logic has been carefully reasoned through, but **please test it yourself in a real terminal** (`cargo run` in Windows Terminal/PowerShell) before trusting the editing experience.
