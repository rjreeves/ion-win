# ion-win v1.0.0

A native Windows port of the [Ion shell](https://gitlab.redox-os.org/redox-os/ion) — originally built for Redox OS — written in Rust. Ion's language is preserved as-is; everything underneath it is rebuilt for Windows: real process spawning and job control, `Ctrl+C` handled the Windows way, a `crossterm`-based line editor, and `redb`-backed persistent state instead of Redox-specific mechanisms.

This is the first tagged release. It was built directly from [Ion's own manual](docs/ion-manual.pdf): almost every feature was implemented by reproducing the manual's worked examples byte-for-byte, backed by both unit tests and real-binary smoke tests. Where the manual was ambiguous, silent, or self-contradictory, that's documented explicitly in [ARCHITECTURE.md](ARCHITECTURE.md) rather than guessed at.

## Highlights

- **Core language**: typed scalars/arrays via `let`, scoped correctly per block (`ARCHITECTURE.md` §10), `fn` with typed/array parameters and docstrings
- **Expansion**: `$name`/`@name`/`${name}`/`@{name}`, `$((arithmetic))`, `$(cmd)`/`@(cmd)` process expansion, all 26 string/array methods, slicing, and full brace expansion — both ranges (`{1..10}`) and general permutation lists (`{ext1,ext2}`, nested)
- **Control flow**: `if`/`else if`/`else`, `while`, `for`/`in`, `match`/`case` with guards, `break`/`continue`, and statement chaining via `and`/`or` and their literal-symbol form `&&`/`||`
- **Process execution**: pipelines (`|` `^|` `&|`), redirection (`>` `>>` `^>` `&>`), background/disown (`&` `&!`), and job bookkeeping via `jobs`/`wait`/`disown`
- **Interactive shell**: a real `crossterm` line editor (history, Tab-completion, word-editing shortcuts, live syntax highlighting), implicit `cd`, persistent `pvar`/`dmark` state backed by `redb`, and a custom **`PROMPT` function** — `fn PROMPT; echo -n "${PWD}# "; end` now renders as the live prompt, matching the manual's own example
- **Signal handling**: `Ctrl+C` interrupts the running foreground command or a pure-Ion loop without killing the shell itself (`ARCHITECTURE.md` §9)
- **Conditionals/builtins**: `test`, `matches`, `contains`/`starts-with`/`ends-with`, `eq`/`is`, `exists`, `intersects`, `isatty`, `which`/`type`, `eval`, `true`/`false`/`bool`, and `source`

## Deliberately not included

- `fg`/`bg` — no clean Windows equivalent to `SIGTSTP`/`SIGCONT`, and shipping a half-faithful version was judged worse than skipping it
- Vi keybindings — real modal editing is a much bigger lift than an incremental addition; a documented but niche feature even in real shells
- The five Polish-notation comparison operators (`<`, `<=`, `>`, `>=`, `=`) — unchecked in upstream Ion too, not just this port

See [HANDOVER.md](HANDOVER.md) for the complete, current list of what's implemented and what's open.

## Getting started

```sh
cargo build --release
cargo test      # 130 tests, all passing
cargo run
```

## Docs

- [README.md](README.md) — overview and a runnable language example
- [HANDOVER.md](HANDOVER.md) — full implemented/gaps list, testing philosophy
- [ARCHITECTURE.md](ARCHITECTURE.md) — every non-obvious design decision, with the reasoning behind it
