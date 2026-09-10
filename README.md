# ion-win

A native Windows port of the [Ion shell](https://gitlab.redox-os.org/redox-os/ion) — originally built for Redox OS — written in Rust. Ion's language (typed variables, `@array`/`$string` sigils, method expansions, `and`/`or`/`&&`/`||`, `match`/`case`, functions with typed parameters, brace/range expansion, ...) is preserved as-is; what changes is everything underneath it: real Windows process spawning and execution control, `Ctrl+C` handled the Windows way, a `crossterm`-based line editor, and `redb`-backed persistent state instead of Redox-specific mechanisms that don't exist here.

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
- **Expansion**: `$name`/`@name`/`${name}`/`@{name}`, `$((arithmetic))`, `$(cmd)`/`@(cmd)` process expansion, string/array methods, grapheme-aware length/slicing, brace ranges and permutation lists (`{1..10}`, `{ext1,ext2}`, nesting)
- **Control flow**: `if`/`else if`/`else`, `while`, `for`/`in`, `match`/`case` with guards, `break`/`continue`, `and`/`or`/`&&`/`||`
- **Process execution**: pipelines (`|` `^|` `&|`), redirection (`>` `>>` `^>` `&>`), background/disown (`&` `&!`), `jobs`/`wait`/`disown`
- **Shell UX**: a real Unicode/grapheme-safe interactive line editor (live history shared safely across windows, Tab-completion, multiline/bracketed paste, word editing, Shift-selection, Windows clipboard copy/cut/paste, syntax highlighting), enhanced `read -p`/`-s`/`-n`, implicit `cd`, persistent `pvar`/`dmark` state, a custom `PROMPT` function, and `Ctrl+C` that interrupts the running command without killing the shell
- **Discoverable help**: categorized `help`, focused `help COMMAND` pages for every builtin, and concept guides such as `help tables`, `help methods`, and `help history`
- **Windows conveniences**: recursive `mkdir`/`md`, safe `move`/`mv` (including table manifests), in-place `rename`/`ren`, `pushd`/`popd`, `elevate` through Windows UAC, and `cls`
- **Structured data**: table variables, JSON/CSV pipelines, `$len(table)` row counts, `$field(row column)` scalar access, and `date-column` transformations for parsing, formatting, timezone conversion, and calendar arithmetic across whole columns
- **Native date/time**: validated `date`, `time`, `datetime`, and `duration`/`interval` types; ISO constructors, extraction/truncation, instant-aware comparison/difference, formatting, true month/year intervals with end-of-month clamping, and IANA timezone/DST policies
- **Manifest operations**: `copy`, ZIP `compress`, and safe `delete` (Recycle Bin by default; permanent only with `--permanent --force`)
- **PostgreSQL console**: `pg-connect` waits for a PostgreSQL server to become ready, then opens an interactive `psql` session on the same connection
- **Conditionals/builtins**: `test`, `matches`, `contains`/`starts-with`/`ends-with`, `eq`/`is`, `exists`, `which`/`type`, `eval`, and more

See [HANDOVER.md](HANDOVER.md) for the full, current list of what's implemented and verified, and what's deliberately not (e.g. `fg`/`bg` and Vi keybindings have no clean fit on Windows / are out of scope by choice, not oversight).

## PostgreSQL console

`pg-connect` waits until PostgreSQL accepts connections and then leaves an
interactive `psql` session attached to the ion-win terminal. Both `pg_isready`
and `psql` must be available on `PATH`.

```ion
pg-connect
pg-connect --host localhost --port 5432 --database postgres --user postgres
```

The defaults are `localhost`, port `5432`, database `postgres`, and user
`postgres`. Authentication follows normal PostgreSQL/libpq behavior: use the
`PGPASSWORD` environment variable, `%APPDATA%\postgresql\pgpass.conf`, or the
interactive `psql` password prompt. ion-win does not embed or persist a
PostgreSQL password.

From the repository root, register and run a reusable console task with:

```ion
ion-win.exe scripts/register_postgres_console_task.ion
task run postgres-console
```

Use `help pg-connect` inside ion-win for the focused command reference.

## Elevating a Windows program

`elevate` starts one external program through the standard Windows UAC consent
prompt. It never stores credentials or bypasses UAC.

```ion
elevate notepad.exe C:\Windows\System32\drivers\etc\hosts
elevate --wait --cwd C:\Work installer.exe /quiet
```

Without `--wait`, ion-win returns after Windows launches the elevated process.
With `--wait`, ion-win waits for it to finish and reports success only when the
program exits with code `0`. Cancelling the UAC prompt is reported as a command
failure.

The first version intentionally accepts no pipeline input. To run several Ion
commands with administrator privileges, put them in a script and elevate a new
ion-win process:

```ion
elevate --wait ion-win.exe admin-maintenance.ion
```

Scripts can test the current token without displaying UAC:

```ion
if is-elevated
    echo "Running as administrator"
else
    elevate --wait ion-win.exe admin-maintenance.ion
end
```

`is-elevated` communicates through command status, so it composes naturally
with `if`, `&&`, and `||` without producing text that must be parsed. See
`examples/elevated_check.ion` for a harmless self-elevation example.

Use `help elevate` and `help is-elevated` for focused command references.

## Docs

- [HANDOVER.md](HANDOVER.md) — what's built, what's verified, what's open, testing philosophy
- [ARCHITECTURE.md](ARCHITECTURE.md) — design decisions and the reasoning behind them, section by section
- [docs/ion-manual.pdf](docs/ion-manual.pdf) — the language spec this project targets
