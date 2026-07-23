# ion-win

A native Windows port of the [Ion shell](https://gitlab.redox-os.org/redox-os/ion) — originally built for Redox OS — written in Rust. Ion's language (typed variables, `@array`/`$string` sigils, method expansions, `and`/`or`/`&&`/`||`, `match`/`case`, functions with typed parameters, brace/range expansion, ...) is preserved as-is; what changes is everything underneath it: real Windows process spawning and job control, `Ctrl+C` handled the Windows way, a `crossterm`-based line editor, and `redb`-backed persistent state instead of Redox-specific mechanisms that don't exist here.

It's driven directly from [Ion's own manual](docs/ion-manual.pdf): almost every feature was implemented by reading the manual's worked examples and writing tests that reproduce their exact output. Where the manual is ambiguous, silent, or self-contradictory, that's called out explicitly in [ARCHITECTURE.md](ARCHITECTURE.md) rather than guessed at quietly.

## Quick start

```sh
cargo build --release
cargo test
cargo run
```

No special setup — it's a standard Cargo binary crate.

```sh
ion-win.exe                        # interactive REPL
ion-win.exe script.ion arg1 arg2   # run a script, exposing @args
```

## A taste of the language

```sh
let name = "world"
echo "Hello, $name!"

let numbers = [ 1 2 3 4 5 ]
for n in @numbers
    if test $n -gt 2
        echo "$n is big"
    end
end

fn greet name:str
    echo "Hi, $name"
end
greet "Ion"

# Custom prompt, sourced from a function's output
fn PROMPT
    echo -n "${PWD}# "
end
```

## What's implemented

- **Core language**: `let` (scalar/array, arithmetic compound assignment), `drop`, scoping rules, `fn` with typed/array parameters and docstrings
- **Expansion**: `$name`/`@name`/`${name}`/`@{name}`, `$((arithmetic))`, `$(cmd)`/`@(cmd)` process expansion, string/array methods, slicing, brace ranges and permutation lists (`{1..10}`, `{ext1,ext2}`, nesting)
- **Control flow**: `if`/`else if`/`else`, `while`, `for`/`in`, `match`/`case` with guards, `break`/`continue`, `and`/`or`/`&&`/`||`
- **Process execution**: pipelines (`|` `^|` `&|`), redirection (`>` `>>` `^>` `&>`), background/disown (`&` `&!`), `jobs`/`wait`/`disown`
- **Shell UX**: a real interactive line editor (history, Tab-completion, word-editing shortcuts, live syntax highlighting), implicit `cd`, persistent `pvar`/`dmark` state, a custom `PROMPT` function, `Ctrl+C` that interrupts the running command without killing the shell
- **Structured data**: table variables, JSON/CSV pipelines, `$len(table)` row counts, and `$field(row column)` scalar access during row iteration
- **Conditionals/builtins**: `test`, `matches`, `contains`/`starts-with`/`ends-with`, `eq`/`is`, `exists`, `which`/`type`, `eval`, and more

See [HANDOVER.md](HANDOVER.md) for the full, current list of what's implemented and verified, and what's deliberately not (e.g. `fg`/`bg` and Vi keybindings have no clean fit on Windows / are out of scope by choice, not oversight).

## Docs

- [HANDOVER.md](HANDOVER.md) — what's built, what's verified, what's open, testing philosophy
- [ARCHITECTURE.md](ARCHITECTURE.md) — design decisions and the reasoning behind them, section by section
- [docs/ion-manual.pdf](docs/ion-manual.pdf) — the language spec this project targets
