//! Single source of truth for interactive-shell "words" that used to be
//! duplicated across three independently hand-maintained lists: the
//! `help` builtin's command index, Tab-completion's command list, and the
//! line editor's keyword-vs-command syntax highlighting (the latter two
//! both in `editor.rs`). Adding a builtin now means adding one entry here
//! instead of remembering to update all three — a real gap in practice:
//! `highlight` (added earlier this session) was added to `dispatch` and
//! the `help` text but silently missed Tab-completion's list until this
//! consolidation caught it.
//!
//! This does *not* cover `dispatch`'s actual match arms in `shell.rs` —
//! that's real per-builtin behavior (different signatures, sync vs async,
//! raw vs. expanded args), not just a name, so unifying it would need a
//! much larger function-pointer-table refactor. Adding a new builtin still
//! means adding both a `dispatch` match arm *and* an entry here.

pub struct Builtin {
    /// The literal word: what Tab-completion offers, and (if `is_keyword`)
    /// what gets magenta "keyword" coloring rather than blue "command"
    /// coloring in the line editor's syntax highlighter.
    pub name: &'static str,
    pub is_keyword: bool,
    /// How this entry appears in `help all`. `None` means it's
    /// folded into a sibling's display instead of getting its own (e.g.
    /// `else`/`in` are covered by `if`/`for`'s "if/else if/else"/"for/in").
    pub help_display: Option<&'static str>,
}

pub const BUILTINS: &[Builtin] = &[
    Builtin {
        name: "exit",
        is_keyword: false,
        help_display: Some("exit"),
    },
    Builtin {
        name: "quit",
        is_keyword: false,
        help_display: None,
    },
    Builtin {
        name: "help",
        is_keyword: false,
        help_display: Some("help"),
    },
    Builtin {
        name: "let",
        is_keyword: true,
        help_display: Some("let"),
    },
    Builtin {
        name: "export",
        is_keyword: true,
        help_display: Some("export"),
    },
    Builtin {
        name: "drop",
        is_keyword: true,
        help_display: Some("drop"),
    },
    Builtin {
        name: "read",
        is_keyword: false,
        help_display: Some("read"),
    },
    Builtin {
        name: "echo",
        is_keyword: false,
        help_display: Some("echo"),
    },
    Builtin {
        name: "cd",
        is_keyword: false,
        help_display: Some("cd"),
    },
    Builtin {
        name: "pushd",
        is_keyword: false,
        help_display: Some("pushd DIRECTORY"),
    },
    Builtin {
        name: "popd",
        is_keyword: false,
        help_display: Some("popd"),
    },
    Builtin {
        name: "pwd",
        is_keyword: false,
        help_display: Some("pwd"),
    },
    Builtin {
        name: "dirs",
        is_keyword: false,
        help_display: Some("dirs"),
    },
    Builtin {
        name: "folders",
        is_keyword: false,
        help_display: Some("folders"),
    },
    Builtin {
        name: "files",
        is_keyword: false,
        help_display: Some("files"),
    },
    Builtin {
        name: "find",
        is_keyword: false,
        help_display: Some("find [--all] [--recurse] [PATH]"),
    },
    Builtin {
        name: "cat",
        is_keyword: false,
        help_display: Some("cat FILE..."),
    },
    Builtin {
        name: "stat",
        is_keyword: false,
        help_display: Some("stat FILE... [--hash sha256]"),
    },
    Builtin {
        name: "copy",
        is_keyword: false,
        help_display: Some("copy/cp [--force] SRC... DEST"),
    },
    Builtin {
        name: "cp",
        is_keyword: false,
        help_display: None,
    },
    Builtin {
        name: "mkdir",
        is_keyword: false,
        help_display: Some("mkdir/md DIR..."),
    },
    Builtin {
        name: "md",
        is_keyword: false,
        help_display: None,
    },
    Builtin {
        name: "move",
        is_keyword: false,
        help_display: Some("move/mv [--force] SRC... DEST"),
    },
    Builtin {
        name: "mv",
        is_keyword: false,
        help_display: None,
    },
    Builtin {
        name: "rename",
        is_keyword: false,
        help_display: Some("rename/ren [--force] SOURCE NEW_NAME"),
    },
    Builtin {
        name: "ren",
        is_keyword: false,
        help_display: None,
    },
    Builtin {
        name: "compress",
        is_keyword: false,
        help_display: Some("compress [--force] SRC... DEST.zip"),
    },
    Builtin {
        name: "delete",
        is_keyword: false,
        help_display: Some("delete [--recurse] PATH..."),
    },
    Builtin {
        name: "if",
        is_keyword: true,
        help_display: Some("if/else if/else"),
    },
    Builtin {
        name: "else",
        is_keyword: true,
        help_display: None,
    },
    Builtin {
        name: "while",
        is_keyword: true,
        help_display: Some("while"),
    },
    Builtin {
        name: "for",
        is_keyword: true,
        help_display: Some("for/in"),
    },
    Builtin {
        name: "in",
        is_keyword: true,
        help_display: None,
    },
    Builtin {
        name: "fn",
        is_keyword: true,
        help_display: Some("fn"),
    },
    Builtin {
        name: "match",
        is_keyword: true,
        help_display: Some("match/case"),
    },
    Builtin {
        name: "case",
        is_keyword: true,
        help_display: None,
    },
    Builtin {
        name: "end",
        is_keyword: true,
        help_display: None,
    },
    Builtin {
        name: "break",
        is_keyword: true,
        help_display: Some("break"),
    },
    Builtin {
        name: "continue",
        is_keyword: true,
        help_display: Some("continue"),
    },
    Builtin {
        name: "test",
        is_keyword: false,
        help_display: Some("test"),
    },
    Builtin {
        name: "matches",
        is_keyword: false,
        help_display: Some("matches"),
    },
    Builtin {
        name: "not",
        is_keyword: true,
        help_display: Some("not"),
    },
    Builtin {
        name: "true",
        is_keyword: false,
        help_display: Some("true"),
    },
    Builtin {
        name: "false",
        is_keyword: false,
        help_display: Some("false"),
    },
    Builtin {
        name: "bool",
        is_keyword: false,
        help_display: Some("bool"),
    },
    Builtin {
        name: "contains",
        is_keyword: false,
        help_display: Some("contains"),
    },
    Builtin {
        name: "starts-with",
        is_keyword: false,
        help_display: Some("starts-with"),
    },
    Builtin {
        name: "ends-with",
        is_keyword: false,
        help_display: Some("ends-with"),
    },
    Builtin {
        name: "eq",
        is_keyword: false,
        help_display: Some("eq/is"),
    },
    Builtin {
        name: "is",
        is_keyword: false,
        help_display: None,
    },
    Builtin {
        name: "exists",
        is_keyword: false,
        help_display: Some("exists"),
    },
    Builtin {
        name: "intersects",
        is_keyword: false,
        help_display: Some("intersects"),
    },
    Builtin {
        name: "isatty",
        is_keyword: false,
        help_display: Some("isatty"),
    },
    Builtin {
        name: "and",
        is_keyword: true,
        help_display: Some("and"),
    },
    Builtin {
        name: "or",
        is_keyword: true,
        help_display: Some("or"),
    },
    Builtin {
        name: "which",
        is_keyword: false,
        help_display: Some("which/type"),
    },
    Builtin {
        name: "type",
        is_keyword: false,
        help_display: None,
    },
    Builtin {
        name: "eval",
        is_keyword: false,
        help_display: Some("eval"),
    },
    Builtin {
        name: "pvar",
        is_keyword: false,
        help_display: Some("pvar set|get|list|delete"),
    },
    Builtin {
        name: "dmark",
        is_keyword: false,
        help_display: Some("dmark add|list|jump"),
    },
    Builtin {
        name: "jobs",
        is_keyword: false,
        help_display: Some("jobs"),
    },
    Builtin {
        name: "wait",
        is_keyword: false,
        help_display: Some("wait"),
    },
    Builtin {
        name: "disown",
        is_keyword: false,
        help_display: Some("disown [-a|PID...]"),
    },
    Builtin {
        name: "source",
        is_keyword: false,
        help_display: Some("source"),
    },
    Builtin {
        name: "highlight",
        is_keyword: false,
        help_display: Some("highlight on|off"),
    },
    Builtin {
        name: "cls",
        is_keyword: false,
        help_display: Some("cls"),
    },
    Builtin {
        name: "from-json",
        is_keyword: false,
        help_display: Some("from-json"),
    },
    Builtin {
        name: "select",
        is_keyword: false,
        help_display: Some("select COL..."),
    },
    Builtin {
        name: "where",
        is_keyword: false,
        help_display: Some("where/filter COL OP VAL"),
    },
    Builtin {
        name: "filter",
        is_keyword: false,
        help_display: None,
    },
    Builtin {
        name: "to-json",
        is_keyword: false,
        help_display: Some("to-json"),
    },
    Builtin {
        name: "from-csv",
        is_keyword: false,
        help_display: Some("from-csv"),
    },
    Builtin {
        name: "to-csv",
        is_keyword: false,
        help_display: Some("to-csv"),
    },
];

/// All literal builtin words, for Tab-completion.
pub fn names() -> impl Iterator<Item = &'static str> {
    BUILTINS.iter().map(|b| b.name)
}

/// Whether `word` should be colored as a keyword (vs. a plain command) by
/// the line editor's syntax highlighter.
pub fn is_keyword(word: &str) -> bool {
    BUILTINS.iter().any(|b| b.is_keyword && b.name == word)
}

const OVERVIEW: &str = "\
ion-win help

Usage:  help TOPIC

Getting started
  help syntax      variables, arrays, expansion, pipes, and redirection
  help tables      structured pipelines and manifest workflows
  help methods     string and array methods
  help history     shared history settings

Commands by category
  Navigation       cd  pushd  popd  pwd  dirs  folders  files  find  dmark
  Files            mkdir  cat  stat  copy  move  rename  compress  delete
  Language         let  export  drop  read  echo  eval  source  fn
  Control flow     if  while  for  match  and  or  break  continue
  Conditions       test  matches  bool  contains  starts-with  ends-with
                   eq/is  exists  intersects  isatty  true  false  not
  Tables           from-json  from-csv  select  where/filter  to-json  to-csv
  State & jobs     pvar  jobs  wait  disown  which/type
  Editor           highlight  cls

Examples
  help delete
  help test
  help tables

Tab completes command names. `help all` prints the complete command index.";

fn command_index() -> String {
    let commands: Vec<_> = BUILTINS
        .iter()
        .filter_map(|builtin| builtin.help_display)
        .collect();
    format!(
        "Complete command index\n\n  {}\n\nUse `help COMMAND` for usage, examples, and important safety notes.",
        commands.join("\n  ")
    )
}

fn page(usage: &str, summary: &str, examples: &[&str], notes: &[&str]) -> String {
    let mut out = format!("Usage:  {usage}\n\n{summary}");
    if !examples.is_empty() {
        out.push_str("\n\nExamples:");
        for example in examples {
            out.push_str("\n  ");
            out.push_str(example);
        }
    }
    if !notes.is_empty() {
        out.push_str("\n\nNotes:");
        for note in notes {
            out.push_str("\n  ");
            out.push_str(note);
        }
    }
    out
}

/// Categorized overview with focused command/topic pages.
pub fn help_text(topic: Option<&str>) -> Result<String, String> {
    let Some(topic) = topic else {
        return Ok(OVERVIEW.to_string());
    };
    let topic = topic.to_ascii_lowercase();
    let text = match topic.as_str() {
        "all" | "commands" => command_index(),
        "help" => page(
            "help [TOPIC]",
            "Shows the categorized overview or focused help for a command or topic.",
            &["help", "help delete", "help syntax", "help all"],
            &[],
        ),
        "syntax" => page(
            "COMMAND [ARG...]",
            "Ion supports scalars ($name), arrays (@name), arithmetic ($((...))), process expansion ($(cmd) / @(cmd)), blocks, and shell pipelines.",
            &[
                "let name = value",
                "let items = [one two three]",
                "echo \"$name: @items\"",
                "find . --recurse | stat | to-json > manifest.json",
                "command && next-command || recovery-command",
            ],
            &[
                "Pipes: | stdout, ^| stderr, &| combined.",
                "Redirects: > overwrite, >> append, ^> stderr, &> combined.",
                "Background: & tracked, &! disowned.",
                "Environment namespace: ${env::NAME}.",
            ],
        ),
        "tables" | "table" => page(
            "SOURCE | TABLE-STAGE...",
            "Tables are structured rows passed in-process between JSON/CSV adapters, filters, file metadata, and manifest-driven operations.",
            &[
                "let manifest = find . --recurse | stat --hash sha256",
                "manifest | where size -gt 1000000 | select path size | to-csv",
                "manifest | copy backup",
                "for row in manifest; echo $field(row path); end",
            ],
            &[
                "$len(manifest) returns the row count.",
                "$field(row column) reads a scalar from an exactly-one-row table.",
                "copy and compress forward a consumed table; delete intentionally does not.",
            ],
        ),
        "methods" | "method" => page(
            "$method(value [ARG...])  |  @method(value [ARG...])",
            "The $ form returns a scalar; the @ form returns an array. Methods include len, len_bytes, lines, chars, graphemes, split, split_at, join, find, replace, replacen, reverse, repeat, case conversion, path helpers, and escaping.",
            &[
                "echo $len(\"hello\")",
                "let words = @split(\"one two three\" \" \")",
                "echo $join(@words \", \")",
                "echo @graphemes(\"👩‍💻\")",
            ],
            &["String indexing, slicing, length, and reversal use Unicode grapheme boundaries."],
        ),
        "history" => page(
            "let HISTORY_SETTING = VALUE",
            "History is appended immediately and shared safely across ion-win windows. Each prompt refreshes Up-arrow recall.",
            &[
                "echo $HISTFILE",
                "let HISTORY_TIMESTAMP = true",
                "let HISTORY_IGNORE = [duplicates whitespace no_such_command]",
                "let HISTFILE_ENABLED = false",
            ],
            &[
                "Supported ignore rules: all, whitespace, duplicates, no_such_command, regex:PATTERN.",
                "HISTFILE changes take effect live.",
            ],
        ),
        "exit" | "quit" => page("exit", "Exits the current ion-win shell.", &[], &["`quit` is an alias."]),
        "let" => page(
            "let NAME[: TYPE] = VALUE",
            "Defines or updates a scalar, array, arithmetic value, function-adjacent value, or captured table pipeline.",
            &["let name = Robert", "let nums = [1 2 3]", "let total: int = 4", "let manifest = find . --recurse | stat"],
            &["Types include str, bool, int, and float."],
        ),
        "export" => page("export NAME = VALUE", "Sets an Ion scalar and exports it to child-process environments.", &["export MODE = production"], &[]),
        "drop" => page("drop NAME...", "Deletes scalar, array, table, or function variables from the active scope.", &["drop temporary manifest"], &[]),
        "read" => page("read VARIABLE...", "Reads one input line and assigns whitespace-separated fields to named variables.", &["read first last"], &[]),
        "echo" => page("echo [-n] VALUE...", "Prints expanded values. `-n` suppresses the trailing newline.", &["echo \"Hello $name\"", "echo -n \"prompt> \""], &[]),
        "cd" => page("cd [DIRECTORY]", "Changes directory; with no argument, goes to %USERPROFILE%.", &["cd ~/Documents", "..", "examples/"], &["Path-looking bare commands perform an implicit cd."]),
        "pushd" => page("pushd DIRECTORY", "Saves the current directory on an in-memory stack, then changes to DIRECTORY.", &["pushd build", "popd"], &["A failed directory change does not alter the stack."]),
        "popd" => page("popd", "Returns to the most recently saved pushd directory.", &[], &["Fails without changing directory when the stack is empty."]),
        "pwd" => page("pwd", "Prints the current directory.", &[], &[]),
        "dirs" => page("dirs", "Lists directory entries in the current directory.", &[], &[]),
        "folders" => page("folders [--all] [--full] [PATH]", "Lists directories only.", &["folders --all ."], &[]),
        "files" => page("files [--all] [--full] [PATH]", "Lists files only.", &["files --full examples"], &[]),
        "find" => page("find [--all] [--recurse] [PATH]", "Lists files for inspection or piping into stat.", &["find . --recurse", "find . --all --recurse | stat"], &["Directories are not emitted as rows."]),
        "cat" => page("cat FILE...", "Writes file contents unchanged, in argument order.", &["cat manifest.json | from-json"], &[]),
        "stat" => page("stat FILE... [--hash sha256]", "Builds a table with path, size, modified time, and is_dir; optionally hashes files concurrently.", &["find . --recurse | stat --hash sha256 | to-json"], &[]),
        "copy" | "cp" => page(
            "copy [--force] SRC... DEST\n        TABLE | copy [--force] DEST",
            "Copies explicit files or every path in a table. Table paths retain their relative layout.",
            &["copy report.txt backup", "manifest | copy backup"],
            &["Existing destinations are skipped unless --force is supplied.", "`cp` is an alias."],
        ),
        "mkdir" | "md" => page(
            "mkdir DIR...",
            "Creates one or more directories, including missing parent directories.",
            &["mkdir reports/2026/july", "mkdir cache output"],
            &["Existing directories are counted and left unchanged.", "`md` is an alias."],
        ),
        "move" | "mv" => page(
            "move [--force] SRC... DEST\n        TABLE | move [--force] DEST",
            "Moves files or folders. A table supplies sources from its `path` column and preserves relative layout under DEST.",
            &["move draft.txt final.txt", "move one.txt two.txt archive", "manifest | move archive"],
            &["Existing destinations are skipped unless --force is supplied.", "Existing directories are never replaced.", "`mv` is an alias."],
        ),
        "rename" | "ren" => page(
            "rename [--force] SOURCE NEW_NAME",
            "Renames one file or folder in place. Use move when changing its parent directory.",
            &["rename draft.txt final.txt", "rename old-folder new-folder"],
            &["NEW_NAME must be a name, not a path.", "`ren` is an alias."],
        ),
        "compress" => page(
            "compress [--force] SRC... DEST.zip\n        TABLE | compress [--force] DEST.zip",
            "Creates a standard ZIP from explicit files or table paths.",
            &["compress report.txt report.zip", "manifest | compress snapshot.zip"],
            &["Existing archives are skipped unless --force is supplied."],
        ),
        "delete" => page(
            "delete [--recurse] PATH...\n        TABLE | delete [--recurse]",
            "Moves files to the Windows Recycle Bin by default. Tables supply paths from their `path` column.",
            &["delete old.txt", "manifest | delete", "delete --recurse old-folder"],
            &[
                "Permanent deletion requires both --permanent and --force.",
                "Directories require --recurse, including Recycle Bin deletion.",
                "Filesystem roots, the current directory, and its ancestors are refused.",
            ],
        ),
        "if" | "else" => page("if CONDITION; ...; [else if CONDITION; ...;] [else; ...;] end", "Runs the first matching branch.", &["if test $count -gt 0; echo positive; else; echo empty; end"], &[]),
        "while" => page("while CONDITION; ...; end", "Repeats a block while its condition succeeds.", &["while test $n -lt 10; let n += 1; end"], &[]),
        "for" | "in" => page("for NAME in VALUES...; ...; end", "Iterates arrays, expanded values, or rows of a table.", &["for item in @items; echo $item; end", "for row in manifest; echo $field(row path); end"], &[]),
        "fn" | "end" => page("fn NAME [PARAM[: TYPE]...]; ...; end", "Defines a function. Bare `fn` lists definitions.", &["fn greet name: str; echo \"Hello $name\"; end"], &[]),
        "match" | "case" => page("match VALUE; case PATTERN [if CONDITION]; ...; case _; ...; end", "Selects a branch by scalar/array pattern intersection, with optional guards.", &["match $name; case Robert; echo owner; case _; echo guest; end"], &[]),
        "break" | "continue" => page(topic.as_str(), "Controls the nearest enclosing while/for loop.", &[], &["`break` exits the loop; `continue` starts its next iteration."]),
        "test" => page(
            "test EXPR",
            "Evaluates truthiness, string/numeric comparisons, or file predicates.",
            &["test -n \"$name\"", "test 2 -eq 2", "test $n -ge 10", "test -f report.txt"],
            &["String: = == !=. Numeric: -eq -ne -lt -le -gt -ge. Files: -e -f -d."],
        ),
        "matches" => page("matches VALUE REGEX", "Succeeds when VALUE matches the regular expression.", &["matches $name \"^[A-Z]\""], &[]),
        "not" => page("not CONDITION", "Inverts a condition's result.", &["if not test -e file.txt; echo missing; end"], &[]),
        "true" | "false" => page(topic.as_str(), "Returns a fixed successful (`true`) or failed (`false`) status.", &[], &[]),
        "bool" => page("bool VALUE", "Succeeds only for `1` or `true`.", &["bool $enabled"], &[]),
        "contains" => page("contains VALUE TEST...", "Succeeds when VALUE contains any TEST.", &["contains \"hello world\" world"], &[]),
        "starts-with" => page("starts-with VALUE TEST...", "Succeeds when VALUE starts with any TEST.", &["starts-with report.csv report"], &[]),
        "ends-with" => page("ends-with VALUE TEST...", "Succeeds when VALUE ends with any TEST.", &["ends-with report.csv .csv"], &[]),
        "eq" | "is" => page("eq [not] VALUE VALUE", "Compares two values for equality (or inequality with `not`).", &["eq $mode production", "is not $left $right"], &["`is` is an alias."]),
        "exists" => page("exists [-s|-a|--fn] NAME  |  exists PATH", "Checks for a scalar, array, function, or filesystem path.", &["exists -s name", "exists --fn PROMPT", "exists report.txt"], &[]),
        "intersects" => page("intersects ARRAY1 ARRAY2", "Succeeds when two named arrays share at least one value.", &["intersects allowed selected"], &[]),
        "isatty" => page("isatty [FD]", "Checks whether stdin (0), stdout (1), or stderr (2) is a terminal. With no argument, succeeds.", &["isatty 1"], &[]),
        "and" | "or" => page(topic.as_str(), "`and COMMAND` runs after success; `or COMMAND` runs after failure.", &["test -f file && echo found", "test -f file || echo missing"], &["Symbol forms are && and ||."]),
        "which" | "type" => page("which PROGRAM...", "Reports whether each name is a builtin, function, or PATH executable.", &["which ion-win git copy"], &["`type` is an alias."]),
        "eval" => page("eval WORD...", "Joins arguments and evaluates them as a new Ion command.", &["eval \"echo $name\""], &[]),
        "pvar" => page("pvar set KEY = VALUE\n        pvar get KEY\n        pvar list\n        pvar delete KEY", "Stores persistent scalar values in the ion-win state database.", &["pvar set project = ion-win", "pvar get project"], &[]),
        "dmark" => page("dmark add NAME [PATH]\n        dmark list\n        dmark jump NAME", "Stores and revisits persistent directory bookmarks.", &["dmark add repo .", "dmark jump repo"], &[]),
        "jobs" => page("jobs", "Lists tracked background processes.", &["long-command &", "jobs"], &[]),
        "wait" => page("wait", "Waits for all tracked background processes.", &[], &[]),
        "disown" => page("disown [-a | PID...]", "Stops tracking selected or all background processes without terminating them.", &["disown -a"], &[]),
        "source" => page("source FILE", "Executes an Ion script in the current shell and scope.", &["source setup.ion"], &[]),
        "highlight" => page("highlight [on|off]", "Shows or changes live syntax highlighting in the line editor.", &["highlight off"], &[]),
        "cls" => page("cls", "Clears the terminal and moves the cursor to the top-left corner.", &[], &[]),
        "from-json" => page("BYTES | from-json", "Parses a JSON object or array of objects into a table.", &["cat data.json | from-json | select name"], &["Structured stages are pipeline-only."]),
        "from-csv" => page("BYTES | from-csv", "Parses CSV with a header row into a table.", &["cat data.csv | from-csv | where size -gt 100"], &["Structured stages are pipeline-only."]),
        "select" => page("TABLE | select COLUMN...", "Projects selected columns from each table row.", &["manifest | select path size | to-csv"], &[]),
        "where" | "filter" => page("TABLE | where COLUMN OP VALUE", "Keeps rows whose column satisfies a test operator.", &["manifest | where size -gt 1000000"], &["`filter` is an alias. Operators match `help test`."]),
        "to-json" => page("TABLE | to-json", "Serializes a table as JSON.", &["manifest | to-json > manifest.json"], &[]),
        "to-csv" => page("TABLE | to-csv", "Serializes a table as CSV.", &["manifest | select path size | to-csv > manifest.csv"], &[]),
        _ => {
            return Err(format!(
                "help: unknown topic '{topic}'. Run `help` for categories or `help all` for the command index."
            ))
        }
    };
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_overview_is_categorized_and_actionable() {
        let text = help_text(None).unwrap();
        assert!(text.contains("Commands by category"));
        assert!(text.contains("help tables"));
        assert!(text.contains("help all"));
        assert!(!text.contains("builtins: exit, help, let"));
    }

    #[test]
    fn every_registered_builtin_has_focused_help() {
        for builtin in BUILTINS {
            assert!(
                help_text(Some(builtin.name)).is_ok(),
                "missing focused help for {}",
                builtin.name
            );
        }
    }

    #[test]
    fn focused_help_includes_usage_examples_and_safety_notes() {
        let delete = help_text(Some("delete")).unwrap();
        assert!(delete.contains("Usage:"));
        assert!(delete.contains("Examples:"));
        assert!(delete.contains("--permanent"));
        assert!(delete.contains("Recycle Bin"));

        assert_eq!(help_text(Some("cp")), help_text(Some("copy")));
        assert!(help_text(Some("tables")).unwrap().contains("$field"));
    }

    #[test]
    fn unknown_help_topic_is_an_actionable_error() {
        let error = help_text(Some("nope")).unwrap_err();
        assert!(error.contains("unknown topic 'nope'"));
        assert!(error.contains("help all"));
    }

    #[test]
    fn is_keyword_distinguishes_keywords_from_commands() {
        assert!(is_keyword("while"));
        assert!(is_keyword("let"));
        assert!(!is_keyword("echo"));
        assert!(!is_keyword("pvar"));
        assert!(!is_keyword("nonexistent"));
    }

    #[test]
    fn names_includes_recently_added_builtins() {
        let all: Vec<&str> = names().collect();
        assert!(all.contains(&"highlight"));
        assert!(all.contains(&"source"));
        assert!(all.contains(&"bool"));
        assert!(all.contains(&"which"));
        assert!(all.contains(&"type"));
        assert!(all.contains(&"eval"));
        assert!(all.contains(&"from-json"));
        assert!(all.contains(&"select"));
        assert!(all.contains(&"where"));
        assert!(all.contains(&"filter"));
        assert!(all.contains(&"to-json"));
        assert!(all.contains(&"cat"));
        assert!(all.contains(&"stat"));
        assert!(all.contains(&"find"));
        assert!(all.contains(&"from-csv"));
        assert!(all.contains(&"to-csv"));
        assert!(all.contains(&"copy"));
        assert!(all.contains(&"cp"));
        assert!(all.contains(&"compress"));
        assert!(all.contains(&"delete"));
    }
}
