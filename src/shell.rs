//! REPL loop and control-flow execution.
//!
//! Wires together the core-language interpreter (`interp`), condition
//! builtins (`builtins`), function definitions (`functions`), persistent
//! state (`state`), pipelines/redirection (`pipeline` + `pipeline_exec`),
//! and external process execution. Supports `if`/`else if`/`else`,
//! `while`, `for`/`in`, and `fn` (ion-manual pages 50-55, 60-61).
//!
//! Supports `break`/`continue` inside `while`/`for` loops (propagating
//! correctly out of nested `if` blocks via the `Flow` signal below), and
//! scope-based variable teardown (`ARCHITECTURE.md` section 10 — each
//! block execution gets its own scope frame via `exec_block`).
//!
//! Not yet implemented: user-defined functions as condition commands or
//! pipeline stages, and `fg`/`bg` job control (`jobs`/`wait`/`disown` are
//! implemented — see `jobs.rs`) — see ARCHITECTURE.md section 6 for the
//! upgrade roadmap.

/// Execution-flow signal threaded through statement/block execution so
/// `break`/`continue`/`exit` can propagate from wherever they're invoked up
/// to whatever should actually act on them: the nearest enclosing loop for
/// `Break`/`LoopContinue`, or the whole shell for `ShellExit`. An `if`
/// block just passes its chosen branch's `Flow` straight through — it
/// doesn't consume anything, so `break` inside an `if` inside a `while`
/// correctly stops the `while`, not just the `if`.
///
/// `Interrupted` is Ctrl+C's effect on a pure-Ion loop with no external
/// process to forward a console signal to (see `jobctl.rs`): `exec_block`
/// polls `jobctl::take_interrupt()` once per statement, and every loop
/// construct also checks it directly so an empty-bodied `while true; end`
/// is still interruptible. It propagates like `ShellExit` (all the way
/// back to the prompt/script, not just the nearest loop) but — unlike
/// `ShellExit` — doesn't exit the shell itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Normal,
    Break,
    LoopContinue,
    ShellExit,
    Interrupted,
}

use crate::builtin_names;
use crate::builtins;
use crate::editor::{self, EditorOutcome, LineEditor};
use crate::fs_builtins;
use crate::functions::{self, FunctionDef};
use crate::history;
use crate::interp::{Interpreter, Quoting, Token};
use crate::jobctl;
use crate::jobs;
use crate::pipeline;
use crate::{err_eprintln, err_println};
use crate::pipeline_exec;
use crate::state::StateHandle;
use crate::table::Table;
use crate::types;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;

/// Whether `line` opens a multi-line block. `fn` only opens a block when
/// it's a definition (`fn NAME ...`) — bare `fn`/`fn -h`/`fn --help` is the
/// function-listing builtin and executes immediately as a simple statement.
fn is_block_opener(line: &str) -> bool {
    let tokens = Interpreter::tokenize(line);
    match tokens.first().map(|t| t.text.as_str()) {
        Some("if") | Some("while") | Some("for") | Some("match") => true,
        Some("fn") => tokens.len() > 1 && !matches!(tokens[1].text.as_str(), "-h" | "--help"),
        _ => false,
    }
}

const DEFAULT_PROMPT: &str = "ion> ";

/// Keeps `$PWD` current before every prompt render, so a `PROMPT` function
/// (or any other script) can rely on it reflecting the shell's actual
/// current directory (ion-manual page 6's own example: `echo -n
/// "${PWD}# "`). Refreshed here rather than at each individual
/// directory-changing call site (`cd`, implicit cd, `dmark jump`) since
/// that would mean re-finding and updating every one of them instead of
/// one place that's always correct regardless of how the directory changed.
fn sync_pwd(interp: &mut Interpreter) {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    interp.set_scalar("PWD".to_string(), cwd);
}

/// Renders the interactive prompt: the manual's `PROMPT` function
/// (ion-manual page 6) if the user has defined one via `fn PROMPT ... end`,
/// else the plain default. `PROMPT`'s `echo` output is captured in-process
/// rather than printed (`Interpreter::echo_output`/`begin_echo_capture`) —
/// real Ion forks a subprocess and captures its stdout, but ion-win doesn't
/// fork, so this is the pragmatic equivalent for the one documented use
/// case. Falls back to the default if `PROMPT` is missing, mis-defined
/// (e.g. declared with parameters, which `call_function` rejects with a
/// printed error), or simply produces no output.
async fn render_prompt(interp: &mut Interpreter, state: &StateHandle) -> String {
    sync_pwd(interp);
    let Some(def) = interp.get_function("PROMPT") else {
        return DEFAULT_PROMPT.to_string();
    };
    interp.begin_echo_capture();
    let _ = call_function("PROMPT", &def, &[], interp, state).await;
    let captured = interp.end_echo_capture();
    if captured.is_empty() {
        DEFAULT_PROMPT.to_string()
    } else {
        captured
    }
}

pub async fn run(state: StateHandle) {
    println!("ion-win 0.8.0 Beta -- type 'exit' to quit, 'help' for builtins");

    let mut interp = Interpreter::new();
    history::seed_defaults(&mut interp);
    let mut editor = LineEditor::with_history(history::load(&interp));
    load_initrc(&mut interp, &state).await;

    loop {
        let prompt = render_prompt(&mut interp, &state).await;
        let Some(line) = editor.read_line(&prompt) else {
            break;
        }; // EOF (Ctrl+Z / Ctrl+D)
        editor.add_history(&line);
        if line.trim().is_empty() {
            continue;
        }

        let flow = if is_block_opener(&line) {
            let Some(body) = read_block_body(&mut editor) else {
                // Esc (pressed twice) cancelled the whole in-progress
                // block: discard it entirely, nothing gets executed.
                continue;
            };
            let mut block = vec![line];
            block.extend(body);
            block.push("end".to_string());
            exec_block(&block, &mut interp, &state).await
        } else {
            dispatch(&line, &mut interp, &state).await
        };

        match flow {
            Flow::ShellExit => break,
            Flow::Break => err_println!("ion: break: not inside a loop"),
            Flow::LoopContinue => err_println!("ion: continue: not inside a loop"),
            Flow::Interrupted => println!("^C"),
            Flow::Normal => {}
        }
    }

    history::save(&interp, editor.history());
}

/// Runs a script file non-interactively (ion-manual "Script Executions"):
/// `args[0]` is the script's own path, `args[1..]` are its arguments,
/// exposed to the script as the `@args` array. No line editor or history
/// is involved — the whole file's lines are read up front and executed
/// through the same `exec_block` engine used for interactive multi-line
/// blocks (which already handles a flat line list with nested `if`/
/// `while`/`for`/`fn` blocks, so no changes were needed there).
pub async fn run_script(path: &str, args: Vec<String>, state: StateHandle) -> i32 {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            err_eprintln!("ion-win: {path}: {e}");
            return 1;
        }
    };
    let lines: Vec<String> = contents.lines().map(str::to_string).collect();

    let mut interp = Interpreter::new();
    interp.set_array("args".to_string(), args);

    match exec_block(&lines, &mut interp, &state).await {
        Flow::Break => {
            err_println!("ion: break: not inside a loop");
            1
        }
        Flow::LoopContinue => {
            err_println!("ion: continue: not inside a loop");
            1
        }
        // 130 = 128 + SIGINT(2), the conventional exit code shells use for
        // an interrupted script (matches bash/POSIX convention).
        Flow::Interrupted => 130,
        Flow::Normal | Flow::ShellExit => 0,
    }
}

/// Reads lines for an already-opened block until the matching `end`,
/// tracking nesting depth so inner `if`/`while`/`for` blocks (and their own
/// `end` lines) are captured as part of the body rather than terminating
/// the read early. The terminal `end` itself is consumed but not returned.
///
/// Returns `None` if the user cancelled the whole block via a second Esc
/// press on an empty continuation line (see `EditorOutcome::Aborted`) —
/// callers must discard everything typed so far rather than executing a
/// truncated block. EOF mid-block (`Some` with whatever was collected, no
/// closing `end`) keeps its prior behavior unchanged.
fn read_block_body(editor: &mut LineEditor) -> Option<Vec<String>> {
    let mut depth = 1i32;
    let mut lines = Vec::new();
    loop {
        let line = match editor.read_continuation_line("    ") {
            EditorOutcome::Line(line) => line,
            EditorOutcome::Eof => break, // unterminated block at EOF
            EditorOutcome::Aborted => return None,
        };
        editor.add_history(&line);
        let first_word = line.trim_start().split_whitespace().next().unwrap_or("");
        if is_block_opener(&line) {
            depth += 1;
        } else if first_word == "end" {
            depth -= 1;
            if depth == 0 {
                break;
            }
        }
        lines.push(line);
    }
    Some(lines)
}

/// Finds the line closing the block that opens at `lines[start]`, honoring
/// nested blocks. Returns (body slice, index just past the closing `end`).
fn extract_block(lines: &[String], start: usize) -> (&[String], usize) {
    let mut depth = 1i32;
    let mut j = start + 1;
    while j < lines.len() {
        let first_word = lines[j]
            .trim_start()
            .split_whitespace()
            .next()
            .unwrap_or("");
        if is_block_opener(&lines[j]) {
            depth += 1;
        } else if first_word == "end" {
            depth -= 1;
            if depth == 0 {
                return (&lines[start + 1..j], j + 1);
            }
        }
        j += 1;
    }
    (&lines[start + 1..], lines.len())
}

/// Executes a sequence of already-materialized lines (which may themselves
/// contain nested blocks). Returns the `Flow` signal to propagate outward.
///
/// Every call is one scope "execution" per ion-manual page 20 ("Scopes"):
/// pushes a fresh variable frame before running the lines and pops it
/// afterward, on every exit path (normal completion, `break`/`continue`/
/// `exit`, or an interrupt) — since `exec_block` is what actually runs an
/// `if` branch, one `while`/`for` iteration, or a function body, this is
/// the single place that needs to know about scope lifecycles at all; the
/// callers (`exec_if`, the `while`/`for` arms below, `call_function`)
/// don't do anything scope-related themselves.
fn exec_block<'a>(
    lines: &'a [String],
    interp: &'a mut Interpreter,
    state: &'a StateHandle,
) -> Pin<Box<dyn Future<Output = Flow> + 'a>> {
    Box::pin(async move {
        interp.push_scope();
        let flow = exec_block_statements(lines, interp, state).await;
        interp.pop_scope();
        flow
    })
}

/// The actual statement-execution loop, factored out of `exec_block` so
/// its scope push/pop wrapper only needs one entry and one exit point
/// regardless of how many `return`s the loop itself has.
fn exec_block_statements<'a>(
    lines: &'a [String],
    interp: &'a mut Interpreter,
    state: &'a StateHandle,
) -> Pin<Box<dyn Future<Output = Flow> + 'a>> {
    Box::pin(async move {
        let mut i = 0;
        while i < lines.len() {
            if jobctl::take_interrupt() {
                return Flow::Interrupted;
            }
            let line = &lines[i];
            let first_word = line.trim_start().split_whitespace().next().unwrap_or("");

            let flow = match first_word {
                "if" => {
                    let (body, next_i) = extract_block(lines, i);
                    let flow = exec_if(line, body, interp, state).await;
                    i = next_i;
                    flow
                }
                "while" => {
                    let (body, next_i) = extract_block(lines, i);
                    let cond_tokens = &Interpreter::tokenize(line)[1..];
                    let mut result = Flow::Normal;
                    loop {
                        // Checked explicitly here (not just inside
                        // exec_block's own per-statement poll) so an
                        // empty-bodied `while true; end` is still
                        // interruptible — exec_block would never even
                        // enter its loop for an empty body.
                        if jobctl::take_interrupt() {
                            result = Flow::Interrupted;
                            break;
                        }
                        if !eval_condition_tokens(cond_tokens, interp, state).await {
                            break;
                        }
                        match exec_block(body, interp, state).await {
                            Flow::Normal | Flow::LoopContinue => continue,
                            Flow::Break => break,
                            Flow::ShellExit => {
                                result = Flow::ShellExit;
                                break;
                            }
                            Flow::Interrupted => {
                                result = Flow::Interrupted;
                                break;
                            }
                        }
                    }
                    i = next_i;
                    result
                }
                "for" => {
                    let (body, next_i) = extract_block(lines, i);
                    let flow = exec_for(line, body, interp, state).await;
                    i = next_i;
                    flow
                }
                "match" => {
                    let (body, next_i) = extract_block(lines, i);
                    let flow = exec_match(line, body, interp, state).await;
                    i = next_i;
                    flow
                }
                "fn" if is_block_opener(line) => {
                    let (body, next_i) = extract_block(lines, i);
                    define_function(line, body, interp);
                    i = next_i;
                    Flow::Normal
                }
                _ => {
                    let flow = dispatch(line, interp, state).await;
                    i += 1;
                    flow
                }
            };

            if flow != Flow::Normal {
                return flow;
            }
        }
        Flow::Normal
    })
}

/// Executes an `if` / `else if` / `else` chain. `header` is the `if ...`
/// line; `body` is everything up to (excluding) the matching `end`.
async fn exec_if(
    header: &str,
    body: &[String],
    interp: &mut Interpreter,
    state: &StateHandle,
) -> Flow {
    // Split the body into (condition, branch_lines) pairs at top-level
    // `else` / `else if` markers.
    let header_tokens = Interpreter::tokenize(header);
    let mut branches: Vec<(Option<Vec<Token>>, Vec<String>)> = Vec::new();
    let mut current_cond = Some(header_tokens[1..].to_vec());
    let mut current_body: Vec<String> = Vec::new();
    let mut depth = 0i32;

    for line in body {
        let first_word = line.trim_start().split_whitespace().next().unwrap_or("");

        if depth == 0 && first_word == "else" {
            branches.push((current_cond.take(), std::mem::take(&mut current_body)));
            let tokens = Interpreter::tokenize(line);
            current_cond = if tokens.get(1).map(|t| t.text.as_str()) == Some("if") {
                Some(tokens[2..].to_vec())
            } else {
                None
            };
            continue;
        }

        if is_block_opener(line) {
            depth += 1;
        } else if first_word == "end" {
            depth -= 1;
        }
        current_body.push(line.clone());
    }
    branches.push((current_cond.take(), current_body));

    for (cond, branch_body) in branches {
        let take = match &cond {
            Some(tokens) => eval_condition_tokens(tokens, interp, state).await,
            None => true,
        };
        if take {
            return exec_block(&branch_body, interp, state).await;
        }
    }
    Flow::Normal
}

/// Executes a `match EXPRESSION ... end` statement (ion-manual pages
/// 56-57): evaluates each `case` branch in order and runs the first one
/// that matches, or a `case _` catch-all if every other case fails.
///
/// The match rule is unified across all four documented/inferred input
/// combinations into one: expand both the subject and each case pattern
/// into a `Vec<String>` via `array_from_token` (a bare scalar becomes a
/// 1-element vec, a `[ ... ]` literal or `@array` reference becomes N
/// elements — plain `expand_all` does *not* suffice here, since it
/// doesn't parse `[ ... ]` array literals on its own, only `array_from_token`
/// does), and a case matches if the two sets share any element. This
/// reproduces "string subject, string case" (equality),
/// "string subject, array case" (subject is *in* the case array), and
/// "array subject, string case" (case value is *in* the subject array)
/// exactly as the manual's three worked examples show; "array subject,
/// array case" has no worked example, so this is an inferred, consistent
/// extension of the same rule (shared element = match) rather than a
/// verified one.
async fn exec_match(
    header: &str,
    body: &[String],
    interp: &mut Interpreter,
    state: &StateHandle,
) -> Flow {
    let header_tokens = Interpreter::tokenize(header);
    if header_tokens.len() < 2 {
        err_println!("ion: match: usage: match EXPRESSION");
        return Flow::Normal;
    }
    let subject_values = expand_match_operand(&header_tokens[1..], interp);

    // Split the body into (pattern, guard, branch_lines) triples at
    // top-level `case` lines — mirrors exec_if's `else`-splitting loop.
    struct CaseBranch {
        pattern: String,
        guard: Option<String>,
        lines: Vec<String>,
    }
    let mut branches: Vec<CaseBranch> = Vec::new();
    let mut current: Option<CaseBranch> = None;
    let mut depth = 0i32;

    for line in body {
        let first_word = line.trim_start().split_whitespace().next().unwrap_or("");

        if depth == 0 && first_word == "case" {
            if let Some(branch) = current.take() {
                branches.push(branch);
            }
            let rest = line.trim_start()["case".len()..].trim_start();
            let (pattern, guard, inline_stmt) = parse_case_header(rest);
            let mut lines = Vec::new();
            if let Some(stmt) = inline_stmt {
                lines.push(stmt);
            }
            current = Some(CaseBranch { pattern, guard, lines });
            continue;
        }

        if is_block_opener(line) {
            depth += 1;
        } else if first_word == "end" {
            depth -= 1;
        }
        if let Some(branch) = current.as_mut() {
            branch.lines.push(line.clone());
        }
    }
    if let Some(branch) = current.take() {
        branches.push(branch);
    }

    for branch in branches {
        let pattern_matches = if branch.pattern == "_" {
            true
        } else {
            let pattern_tokens = Interpreter::tokenize(&branch.pattern);
            let pattern_values = expand_match_operand(&pattern_tokens, interp);
            subject_values.iter().any(|s| pattern_values.contains(s))
        };
        if !pattern_matches {
            continue;
        }
        if let Some(guard) = &branch.guard {
            let guard_tokens = Interpreter::tokenize(guard);
            if !eval_condition_tokens(&guard_tokens, interp, state).await {
                continue;
            }
        }
        return exec_block(&branch.lines, interp, state).await;
    }
    Flow::Normal
}

/// Expands a `match` subject or `case` pattern into its "value set" for
/// the shared-element match rule. Delegates to `array_from_token` per
/// token (not `expand_all`) specifically because `array_from_token` is
/// the one that understands `[ ... ]` array literals; `@array`
/// references and bare scalars already work the same either way.
fn expand_match_operand(tokens: &[Token], interp: &Interpreter) -> Vec<String> {
    tokens.iter().flat_map(|t| interp.array_from_token(t)).collect()
}

/// Parses everything after a `case` line's literal `case ` prefix into
/// (PATTERN, optional GUARD, optional inline statement) — ion-manual page
/// 56's single-line form (`case _; echo "not found"`, no space needed
/// before the `;`) and page 57's match guards (`case PATTERN if GUARD`).
/// Quote/bracket-aware so a literal `;`/`if` inside a quoted pattern or
/// array-literal case isn't mistaken for a separator.
fn parse_case_header(text: &str) -> (String, Option<String>, Option<String>) {
    let (before_semi, inline) = split_at_top_level_semicolon(text);
    let (pattern, guard) = split_case_guard(before_semi);
    (pattern, guard, inline.map(|s| s.trim().to_string()))
}

/// Splits at the first unquoted, top-level `;`, if any.
fn split_at_top_level_semicolon(text: &str) -> (&str, Option<&str>) {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for (i, c) in text.char_indices() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            '[' => depth += 1,
            ']' => depth -= 1,
            ';' if depth == 0 => return (&text[..i], Some(&text[i + 1..])),
            _ => {}
        }
    }
    (text, None)
}

/// Splits `text` into whitespace-separated words, keeping quote/bracket
/// characters intact (so the result can be re-tokenized faithfully) and
/// never splitting *inside* a quoted string or `[ ... ]` literal.
fn split_case_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for c in text.chars() {
        if let Some(q) = quote {
            current.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                current.push(c);
            }
            '[' => {
                depth += 1;
                current.push(c);
            }
            ']' => {
                depth -= 1;
                current.push(c);
            }
            c if c.is_whitespace() && depth == 0 => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// Splits at a standalone top-level `if` word, if any — the pattern is
/// everything before it, the guard is everything after.
fn split_case_guard(text: &str) -> (String, Option<String>) {
    let words = split_case_words(text);
    if let Some(pos) = words.iter().position(|w| w == "if") {
        (words[..pos].join(" "), Some(words[pos + 1..].join(" ")))
    } else {
        (text.trim().to_string(), None)
    }
}

/// Executes a `for VAR in EXPR ... end` loop. Only single-variable form is
/// supported so far (chunked `for a b c in ...` is a follow-up).
async fn exec_for(
    header: &str,
    body: &[String],
    interp: &mut Interpreter,
    state: &StateHandle,
) -> Flow {
    let tokens = Interpreter::tokenize(header);
    // tokens: ["for", VAR, "in", ...EXPR]
    if tokens.len() < 4 || tokens[2].text != "in" {
        err_println!("ion: for: usage: for VAR in EXPR");
        return Flow::Normal;
    }
    let var = tokens[1].text.clone();

    // `for VAR in TABLE` (ARCHITECTURE.md §19): a bare reference to a
    // table variable iterates its rows, each bound as its own one-row
    // `Table` (rather than being flattened into scalar text) — so the
    // loop body can keep using the same "table variable as a pipeline
    // source" mechanism (`row | select col`, `row | to-json`, ...)
    // structured pipelines already rely on elsewhere. Only recognized
    // when the whole "in" clause is *exactly* one token naming an
    // existing table variable, so ordinary `for x in @arr`/`for x in 1 2
    // 3` are completely unaffected — a table and a plain expansion never
    // look the same.
    if tokens.len() == 4 {
        if let Some(table) = interp.get_table(&tokens[3].text) {
            let rows = table.rows.clone();
            for row in rows {
                if jobctl::take_interrupt() {
                    return Flow::Interrupted;
                }
                interp.set_table(var.clone(), Table { rows: vec![row] });
                match exec_block(body, interp, state).await {
                    Flow::Normal | Flow::LoopContinue => continue,
                    Flow::Break => break,
                    Flow::ShellExit => return Flow::ShellExit,
                    Flow::Interrupted => return Flow::Interrupted,
                }
            }
            return Flow::Normal;
        }
    }

    let elements = interp.expand_all(&tokens[3..]);

    for element in elements {
        // Checked explicitly per-iteration (not just relying on exec_block's
        // own poll) so an empty-bodied `for x in @range; end` is still
        // interruptible.
        if jobctl::take_interrupt() {
            return Flow::Interrupted;
        }
        interp.builtin_let(&[
            Token::from(var.clone()),
            Token::from("="),
            Token::from(element),
        ]);
        match exec_block(body, interp, state).await {
            Flow::Normal | Flow::LoopContinue => continue,
            Flow::Break => break,
            Flow::ShellExit => return Flow::ShellExit,
            Flow::Interrupted => return Flow::Interrupted,
        }
    }
    Flow::Normal
}

/// Finds the first unquoted `&&`/`||` token in `tokens`, splitting into
/// (before, operator, after) as token slices. Unlike
/// `split_at_top_level_chain_op`'s raw-string scan (used by `dispatch`,
/// which needs real substrings to re-tokenize each half faithfully),
/// this can search the token list directly and slice it, with no
/// nesting-depth tracking needed — `Interpreter::tokenize` has already
/// collapsed every quote/bracket/expansion into one atomic token by the
/// time this runs, confirmed empirically: `&&` between two `test`
/// invocations tokenizes as its own standalone `Quoting::None` token.
fn split_chain_op_tokens(tokens: &[Token]) -> Option<(&[Token], ChainOp, &[Token])> {
    let i = tokens
        .iter()
        .position(|t| t.quoting == Quoting::None && (t.text == "&&" || t.text == "||"))?;
    let op = if tokens[i].text == "&&" { ChainOp::And } else { ChainOp::Or };
    Some((&tokens[..i], op, &tokens[i + 1..]))
}

/// Evaluates a condition line's tokens (used by `if`/`while`), reusing the
/// same builtins and external-process exit-status rules as top-level
/// statement dispatch, per the manual's rule that `if`'s exit status of 0
/// is truthy.
fn eval_condition_tokens<'a>(
    tokens: &'a [Token],
    interp: &'a mut Interpreter,
    state: &'a StateHandle,
) -> Pin<Box<dyn Future<Output = bool> + 'a>> {
    Box::pin(async move {
        // ion-manual page 51: `if test $foo = "foo" && test $bar = "bar"`
        // — `&&`/`||` inside a condition short-circuit exactly like Rust's
        // own `&&`/`||` already do, so this reads as plainly as it looks:
        // the right side is only ever evaluated (and only ever spawns
        // whatever external process it names) when the left side's
        // result doesn't already decide the answer.
        if let Some((before, op, after)) = split_chain_op_tokens(tokens) {
            let left = eval_condition_tokens(before, interp, state).await;
            return match op {
                ChainOp::And => left && eval_condition_tokens(after, interp, state).await,
                ChainOp::Or => left || eval_condition_tokens(after, interp, state).await,
            };
        }

        let Some(head) = tokens.first() else {
            return false;
        };

        match head.text.as_str() {
            "not" => !eval_condition_tokens(&tokens[1..], interp, state).await,
            "true" => true,
            "false" => false,
            "bool" => builtins::eval_bool(&interp.expand_all(&tokens[1..])),
            "contains" => builtins::eval_contains(&interp.expand_all(&tokens[1..])),
            "starts-with" => builtins::eval_starts_with(&interp.expand_all(&tokens[1..])),
            "ends-with" => builtins::eval_ends_with(&interp.expand_all(&tokens[1..])),
            "eq" | "is" => builtins::eval_eq(&interp.expand_all(&tokens[1..])),
            "exists" => eval_exists(&tokens[1..], interp),
            "intersects" => eval_intersects(&tokens[1..], interp),
            "isatty" => builtins::eval_isatty(&interp.expand_all(&tokens[1..])),
            "test" => builtins::eval_test(&interp.expand_all(&tokens[1..])),
            "matches" => builtins::eval_matches(&interp.expand_all(&tokens[1..])),
            _ => {
                let parsed = pipeline::parse(tokens);
                if !parsed.is_trivial() {
                    return pipeline_exec::run(&parsed, interp, state).await;
                }

                let args = interp.expand_all(&tokens[1..]);
                match head.text.as_str() {
                    "echo" => {
                        let (rest, no_newline) = crate::interp::split_echo_no_newline_flag(&args);
                        interp.echo_output(&rest.join(" "), !no_newline);
                        true
                    }
                    "pvar" => {
                        handle_pvar(&args, state).await;
                        true
                    }
                    "dmark" => {
                        handle_dmark(&args, state).await;
                        true
                    }
                    other => run_external_status(other, &args),
                }
            }
        }
    })
}

/// `&&`/`||` (ion-manual page 51): confirmed against upstream Ion's real
/// parser (`Linux/ion-master/src/lib/parser/statement/splitter.rs`) that
/// `cmd1 && cmd2` is *exactly* equivalent to `cmd1` on one line followed
/// by `and cmd2` on the next — upstream's splitter literally rewrites one
/// into the other. So neither of `&&`/`||`'s two call sites (`dispatch`,
/// `eval_condition_tokens`) need new runtime semantics, only a way to find
/// the split point; `and`/`or`'s existing `previous_status` logic does
/// the rest.
enum ChainOp {
    And,
    Or,
}

/// Finds the first top-level (unquoted, unbracketed) `&&`/`||` in `line`,
/// splitting it into (everything before, the operator, everything after)
/// as real substring slices — not reconstructed/rejoined text — so
/// `dispatch`'s recursive re-tokenization of each half sees exactly the
/// same quoting/spacing the user typed. A raw-string scan rather than
/// `Interpreter::tokenize` + a token search (which `eval_condition_tokens`
/// below uses instead, since it never needs to go back to a string).
fn split_at_top_level_chain_op(line: &str) -> Option<(&str, ChainOp, &str)> {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut chars = line.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => depth -= 1,
            '&' if depth == 0 && chars.peek().map(|&(_, c2)| c2) == Some('&') => {
                let (j, _) = chars.next().unwrap();
                return Some((&line[..i], ChainOp::And, &line[j + 1..]));
            }
            '|' if depth == 0 && chars.peek().map(|&(_, c2)| c2) == Some('|') => {
                let (j, _) = chars.next().unwrap();
                return Some((&line[..i], ChainOp::Or, &line[j + 1..]));
            }
            _ => {}
        }
    }
    None
}

/// Whether `cmd` is a pipeline stage that produces a `Table`
/// (`ARCHITECTURE.md` §18): one of the structured pipeline builtins that
/// yields a table (not `to-json`, which deliberately converts *out* of
/// table form into text — a `let` right-hand side ending in `to-json`
/// intentionally isn't captured this way), or an existing table variable
/// (so `let derived = mytable | where ...` re-derives from a previously
/// stored table).
fn is_table_producing_command(cmd: &str, interp: &Interpreter) -> bool {
    matches!(cmd, "from-json" | "select" | "where" | "filter") || interp.get_table(cmd).is_some()
}

/// Intercepts `let NAME = PIPELINE` — where `PIPELINE`'s *last* stage
/// produces a `Table` — before the ordinary pipeline-vs-simple-statement
/// gate below, since a line like `let procs = tool --json | from-json |
/// where cpu -gt 5` would otherwise be handed whole to `pipeline::parse`,
/// which has no idea `let NAME =` is meant to capture the result rather
/// than being the pipeline's own (unsupported) first stage. Checking the
/// *last* stage (not the first) matters: `echo '[...]' | from-json` is a
/// table-producing pipeline even though its first word, `echo`, isn't a
/// table-producing command by itself. Runs the right-hand side through
/// `pipeline_exec::run_capturing_table` and stores whatever `Table` it
/// produces under `NAME`, rather than `builtin_let`'s ordinary
/// scalar/array/arithmetic handling. Returns `None` (falling through to
/// normal dispatch) when the line isn't of this shape at all, so ordinary
/// `let x = 5` and `let arr = [ ... ]` are completely unaffected.
async fn try_dispatch_let_table(
    raw_tokens: &[Token],
    interp: &mut Interpreter,
    state: &StateHandle,
) -> Option<Flow> {
    if raw_tokens.first().map(|t| t.text.as_str()) != Some("let") {
        return None;
    }
    let name = raw_tokens.get(1)?.text.clone();
    if raw_tokens.get(2).map(|t| t.text.as_str()) != Some("=") {
        return None;
    }

    let rhs_pipeline = pipeline::parse(&raw_tokens[3..]);
    let last_stage_cmd = rhs_pipeline.stages.last()?.tokens.first()?.text.as_str();
    if !is_table_producing_command(last_stage_cmd, interp) {
        return None;
    }

    let (ok, captured) = pipeline_exec::run_capturing_table(&rhs_pipeline, interp, state).await;
    interp.set_previous_status(ok);
    match captured {
        Some(table) => {
            interp.set_table(name, table);
        }
        None => err_println!("ion-win: let: right-hand side did not produce a table"),
    }
    Some(Flow::Normal)
}

/// Handles one simple (non-block) line of input. Returns the `Flow` signal
/// to propagate outward (`ShellExit` for `exit`/`quit`, `Break`/
/// `LoopContinue` for `break`/`continue`, `Normal` otherwise).
async fn dispatch(line: &str, interp: &mut Interpreter, state: &StateHandle) -> Flow {
    if let Some((before, op, after)) = split_at_top_level_chain_op(line) {
        let flow = Box::pin(dispatch(before, interp, state)).await;
        if flow != Flow::Normal {
            return flow;
        }
        let should_run = match op {
            ChainOp::And => interp.previous_status(),
            ChainOp::Or => !interp.previous_status(),
        };
        return if should_run {
            Box::pin(dispatch(after, interp, state)).await
        } else {
            Flow::Normal
        };
    }

    let raw_tokens = Interpreter::tokenize(line);

    if let Some(flow) = try_dispatch_let_table(&raw_tokens, interp, state).await {
        return flow;
    }

    let parsed = pipeline::parse(&raw_tokens);
    if !parsed.is_trivial() {
        // A failed pipeline (bad exit code, missing command, unsupported
        // stage) doesn't kill the interactive shell — only `exit`/`quit` do.
        let ok = pipeline_exec::run(&parsed, interp, state).await;
        interp.set_previous_status(ok);
        return Flow::Normal;
    }

    let Some(cmd) = raw_tokens.first().map(|t| t.text.clone()) else {
        return Flow::Normal;
    };
    let raw_args = &raw_tokens[1..];

    // `and`/`or` (confirmed against upstream Ion's real source,
    // `shell/flow.rs`) are statement-level keywords, not simple builtins:
    // `and STMT` runs STMT only if the *previous* statement succeeded (its
    // result becomes the new status; otherwise the prior failure stands
    // untouched); `or STMT` is the mirror image. Recurses on whatever text
    // follows the keyword in the original line, so `and`/`or` can
    // themselves be chained or precede any other statement, including
    // another `and`/`or`. Handled *before* the default-status reset below
    // — it must read whatever status the previous statement left behind,
    // not a freshly-reset one.
    if cmd == "and" || cmd == "or" {
        let rest = line.trim_start()[cmd.len()..].trim_start();
        let run = if cmd == "and" { interp.previous_status() } else { !interp.previous_status() };
        return if run {
            Box::pin(dispatch(rest, interp, state)).await
        } else {
            Flow::Normal
        };
    }

    // Default every other statement to "succeeded" before it runs, so
    // `&&`/`||` chaining sees a sensible status even for builtins with no
    // natural failure mode of their own (`echo`, `let`, `pvar`, ...)
    // instead of a stale leftover from whatever ran two statements ago.
    // Specific arms below (`test`, `cd`, external processes, ...) still
    // override this with their real result.
    interp.set_previous_status(true);

    match cmd.as_str() {
        "exit" | "quit" => return Flow::ShellExit,
        "break" => return Flow::Break,
        "continue" => return Flow::LoopContinue,

        "help" => println!("{}", builtin_names::help_text()),

        // Not part of upstream Ion — an ion-win-specific runtime toggle for
        // the interactive line editor's live syntax highlighting, since
        // it's cosmetic and some terminals/preferences may not want it.
        "highlight" => {
            let args = interp.expand_all(raw_args);
            handle_highlight(&args);
        }

        // `let`, `export`, `drop`, and `read` operate on raw (unexpanded)
        // tokens themselves, since the left-hand side is a name, not a
        // value to expand.
        "let" => interp.builtin_let(raw_args),
        "export" => interp.builtin_export(raw_args),
        "drop" => interp.builtin_drop(raw_args),
        "read" => handle_read(raw_args, interp),
        // `exists` also needs raw tokens: its `-a`/`-s`/`--fn` flags take a
        // bare variable/array/function NAME (ion-manual page 72's own
        // examples: `exists -s myVar`, not `exists -s $myVar`), since
        // checking existence needs the identifier itself, not its
        // expanded value.
        "exists" => {
            let ok = eval_exists(raw_args, interp);
            interp.set_previous_status(ok);
        }
        // `intersects` also needs raw (unexpanded) tokens: it takes bare
        // array NAMES, same reasoning as `exists -a` above.
        "intersects" => {
            let ok = eval_intersects(raw_args, interp);
            interp.set_previous_status(ok);
        }

        "source" => {
            let args = interp.expand_all(raw_args);
            return handle_source(&args, interp, state).await;
        },

        "end" => {
            // A stray `end` with no matching opener (e.g. mismatched block).
            err_println!("ion: syntax error: unexpected 'end'");
        }

        // Bare `fn` / `fn -h` / `fn --help` lists defined functions
        // (ion-manual page 74). `fn NAME ...` definitions never reach here —
        // they're intercepted earlier as a block by `is_block_opener`.
        "fn" => handle_fn_builtin(interp),

        // Everything else: check user-defined functions first (using raw,
        // unexpanded args — array-typed parameters need to see `[ ... ]`
        // literals and `@array` tokens intact, not pre-flattened), then
        // fall back to builtins/external processes.
        _ => {
            // Implicit cd (ion-manual page 5): a bare path-looking word
            // with no other arguments changes directory automatically,
            // e.g. `~/Documents`, `..`, `.config`, `examples/`.
            if raw_args.is_empty() && looks_like_implicit_cd_target(&cmd) {
                let ok = handle_cd(&[cmd]);
                interp.set_previous_status(ok);
                return Flow::Normal;
            }

            if let Some(def) = interp.get_function(&cmd) {
                return call_function(&cmd, &def, raw_args, interp, state).await;
            }

            let args = interp.expand_all(raw_args);
            match cmd.as_str() {
                "echo" => {
                    let (rest, no_newline) = crate::interp::split_echo_no_newline_flag(&args);
                    interp.echo_output(&rest.join(" "), !no_newline);
                }
                "cd" => {
                    let ok = handle_cd(&args);
                    interp.set_previous_status(ok);
                }
                "pwd" | "dirs" | "folders" | "files" => handle_fs_builtin(cmd.as_str(), &args),
                "pvar" => handle_pvar(&args, state).await,
                "dmark" => handle_dmark(&args, state).await,
                "jobs" => handle_jobs(),
                "wait" => jobs::wait_all(),
                "disown" => handle_disown(&args),
                "source" => return handle_source(&args, interp, state).await,
                "test" => {
                    let ok = builtins::eval_test(&args);
                    interp.set_previous_status(ok);
                }
                "matches" => {
                    let ok = builtins::eval_matches(&args);
                    interp.set_previous_status(ok);
                }
                // ion-manual page 68/73/83: `true`/`false`/`bool` are real
                // builtins, not just condition-context keywords — running
                // them standalone must not fall through to `run_external`
                // (which would only "work" by accident if some external
                // true.exe/false.exe happens to be on PATH).
                "true" => interp.set_previous_status(true),
                "false" => interp.set_previous_status(false),
                "bool" => {
                    let ok = builtins::eval_bool(&args);
                    interp.set_previous_status(ok);
                }
                "contains" => {
                    let ok = builtins::eval_contains(&args);
                    interp.set_previous_status(ok);
                }
                "starts-with" => {
                    let ok = builtins::eval_starts_with(&args);
                    interp.set_previous_status(ok);
                }
                "ends-with" => {
                    let ok = builtins::eval_ends_with(&args);
                    interp.set_previous_status(ok);
                }
                "eq" | "is" => {
                    let ok = builtins::eval_eq(&args);
                    interp.set_previous_status(ok);
                }
                "isatty" => {
                    let ok = builtins::eval_isatty(&args);
                    interp.set_previous_status(ok);
                }
                "which" | "type" => handle_which(&args, interp),
                // ion-manual page 71: joins its arguments with spaces and
                // evaluates the result as a single new command.
                "eval" => return Box::pin(dispatch(&args.join(" "), interp, state)).await,
                other => {
                    let ok = run_external(other, &args);
                    interp.set_previous_status(ok);
                }
            }
        }
    }

    Flow::Normal
}

/// Whether `s` looks like a path per ion-manual page 5's implicit-`cd`
/// rule: starts with `.` (covers `..`, `.config`, `./foo`), `/`, or `~`, or
/// ends with `/`.
fn looks_like_implicit_cd_target(s: &str) -> bool {
    s.starts_with('.') || s.starts_with('/') || s.starts_with('~') || s.ends_with('/')
}

/// `cd [DIRECTORY]` (ion-manual page 68): with no argument, changes to the
/// home directory (`%USERPROFILE%` on Windows); with an argument, changes
/// to that directory (a leading `~` expands to the home directory first).
/// Returns whether the directory change succeeded, for `previous_status`
/// (`$?`) tracking.
fn handle_cd(args: &[String]) -> bool {
    let target = match args.first() {
        Some(path) => expand_tilde(path),
        None => match home_dir() {
            Some(home) => home,
            None => {
                err_println!("ion-win: cd: could not determine home directory (%USERPROFILE% not set)");
                return false;
            }
        },
    };
    if let Err(e) = std::env::set_current_dir(&target) {
        err_println!("ion-win: cd: {target}: {e}");
        return false;
    }
    true
}

fn handle_fs_builtin(name: &str, args: &[String]) {
    match fs_builtins::capture(name, args) {
        Some(Ok(text)) if !text.is_empty() => println!("{text}"),
        Some(Ok(_)) => {}
        Some(Err(e)) => err_println!("ion-win: {e}"),
        None => {}
    }
}

/// `highlight` (no args) prints whether live syntax highlighting is
/// currently on; `highlight on`/`highlight off` toggles it at runtime.
fn handle_highlight(args: &[String]) {
    match args.first().map(String::as_str) {
        None => {
            let state = if editor::highlight_enabled() { "on" } else { "off" };
            println!("highlight: {state}");
        }
        Some("on") => {
            editor::set_highlight_enabled(true);
            println!("highlight: on");
        }
        Some("off") => {
            editor::set_highlight_enabled(false);
            println!("highlight: off");
        }
        Some(other) => err_println!("ion: highlight: usage: highlight [on|off] (got '{other}')"),
    }
}

/// `which`/`type` (ion-manual page 83): "searches for the alias/builtin/
/// function/executable that would be executed if you ran that command."
/// ion-win has no `alias`, so only builtin/function/PATH-executable are
/// checked.
fn handle_which(args: &[String], interp: &Interpreter) {
    if args.is_empty() {
        err_println!("ion: which: usage: which PROGRAM...");
        return;
    }
    for name in args {
        if builtin_names::names().any(|b| b == name) {
            println!("{name}: builtin");
        } else if interp.get_function(name).is_some() {
            println!("{name}: function");
        } else if let Some(path) = resolve_on_path(name) {
            println!("{path}");
        } else {
            err_println!("ion: which: {name}: not found");
        }
    }
}

/// Searches `PATH` for `name`, trying each `PATHEXT` extension in turn
/// (matching how Windows itself resolves a bare command name to an
/// executable) unless `name` already has an extension.
fn resolve_on_path(name: &str) -> Option<String> {
    let path_var = std::env::var_os("PATH")?;
    let has_ext = std::path::Path::new(name).extension().is_some();
    let exts: Vec<String> = if has_ext {
        vec![String::new()]
    } else {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .map(str::to_string)
            .collect()
    };

    for dir in std::env::split_paths(&path_var) {
        for ext in &exts {
            let candidate = dir.join(format!("{name}{ext}"));
            if candidate.is_file() {
                return Some(candidate.display().to_string());
            }
        }
    }
    None
}

/// `exists [EXPRESSION]` (ion-manual page 72): exit status 0 if the given
/// item exists, else 1. `-a`/`-s`/`--fn` deliberately take a *bare* name
/// (`raw_args`, unexpanded) — the manual's own examples are explicit about
/// this: "Don't use the `$` sigil, but only the name of the variable to
/// check" — since existence-checking needs the identifier itself, not
/// whatever it currently expands to. `-b`/`-d`/`-f` and the bare-STRING
/// form use normal expansion, mirroring `test`'s equivalent flags.
fn eval_exists(raw_args: &[Token], interp: &Interpreter) -> bool {
    match raw_args {
        [] => false,
        [flag, name] if flag.text == "-a" => {
            interp.get_array(&name.text).is_some_and(|v| !v.is_empty())
        }
        [flag, name] if flag.text == "-s" => {
            interp.get_scalar(&name.text).is_some_and(|v| !v.is_empty())
        }
        [flag, name] if flag.text == "--fn" => interp.get_function(&name.text).is_some(),
        [flag, rest @ ..] if flag.text == "-b" => {
            resolve_on_path(&interp.expand_all(rest).join(" ")).is_some()
        }
        [flag, rest @ ..] if flag.text == "-d" => {
            std::path::Path::new(&interp.expand_all(rest).join(" ")).is_dir()
        }
        [flag, rest @ ..] if flag.text == "-f" => {
            std::path::Path::new(&interp.expand_all(rest).join(" ")).is_file()
        }
        rest => !interp.expand_all(rest).join(" ").is_empty(),
    }
}

/// `intersects ARRAY1 ARRAY2` — ion-win extension, not in upstream Ion
/// (unlike everything else on the manual's "Complete List of Conditional
/// Builtins" checklist, page 51, it's unchecked there too — no real
/// implementation to verify against). Product decision: exit status 0 if
/// the two named arrays share at least one element. Takes bare array
/// NAMES (`raw_args`, unexpanded, like `exists -a`), since `@array`
/// expansion would fan both arrays out into one flat argument list with
/// no way to tell which elements came from which side.
fn eval_intersects(raw_args: &[Token], interp: &Interpreter) -> bool {
    match raw_args {
        [a, b] => match (interp.get_array(&a.text), interp.get_array(&b.text)) {
            (Some(arr_a), Some(arr_b)) => arr_a.iter().any(|x| arr_b.contains(x)),
            _ => false,
        },
        _ => {
            err_println!("ion-win: intersects: usage: intersects ARRAY1 ARRAY2");
            false
        }
    }
}

fn home_dir() -> Option<String> {
    std::env::var("USERPROFILE").ok()
}

/// Expands a leading `~` to the home directory (`~/Documents` ->
/// `%USERPROFILE%/Documents`, bare `~` -> `%USERPROFILE%`).
fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix('~') {
        if let Some(home) = home_dir() {
            return format!("{home}{rest}");
        }
    }
    path.to_string()
}

/// `source FILE` executes another Ion file in the current interpreter so
/// variables and functions defined there remain visible to the caller.
async fn handle_source(args: &[String], interp: &mut Interpreter, state: &StateHandle) -> Flow {
    let [file] = args else {
        err_println!("ion-win: source: usage: source FILE");
        return Flow::Normal;
    };
    let path = expand_tilde(file);
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            err_println!("ion-win: source: {path}: {e}");
            return Flow::Normal;
        }
    };
    let lines: Vec<String> = contents.lines().map(str::to_string).collect();
    exec_block(&lines, interp, state).await
}

/// Path to the interactive-startup config file (ion-manual pages 4/63:
/// a file literally named `initrc`, found via the XDG config dir —
/// `~/.config/ion/initrc` by default on Linux, overridable via
/// `$XDG_CONFIG_HOME`). ion-win has no real `$HOME`/XDG on Windows, so it
/// mirrors `state.rs`'s existing `%APPDATA%` convention as the default,
/// while still honoring `$XDG_CONFIG_HOME` if set, matching the manual's
/// documented override mechanism.
fn initrc_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("ion").join("initrc");
    }
    if let Ok(appdata) = std::env::var("APPDATA") {
        return PathBuf::from(appdata).join("ion-win").join("initrc");
    }
    PathBuf::from("ion-win-initrc")
}

/// Runs the initrc file at interactive shell startup, if one exists.
/// Silently does nothing when it doesn't — the common case for a fresh
/// install — unlike an explicit `source` of a missing file, which is a
/// user error worth reporting.
async fn load_initrc(interp: &mut Interpreter, state: &StateHandle) {
    let path = initrc_path();
    if !path.is_file() {
        return;
    }
    let Some(path_str) = path.to_str() else {
        return;
    };
    handle_source(&[path_str.to_string()], interp, state).await;
}

/// Stores a `fn NAME ...` header plus its already-extracted body as a
/// `FunctionDef` in the interpreter's function table.
fn define_function(header: &str, body: &[String], interp: &mut Interpreter) {
    let tokens = Interpreter::tokenize(header);
    let name = tokens[1].text.clone();
    let param_texts: Vec<String> = tokens[2..].iter().map(|t| t.text.clone()).collect();
    let (params, doc) = functions::parse_params(&param_texts);
    interp.define_function(
        name,
        FunctionDef {
            params,
            body: body.to_vec(),
            doc,
        },
    );
}

/// Binds `raw_args` to `def`'s parameters (validating/normalizing types,
/// per ion-manual page 60's `expected int, found value 'a'` error), runs
/// the body, then restores whatever the parameter names held before the
/// call. This is deliberately just parameter-level scoping, not a full
/// lexical call frame.
async fn call_function(
    name: &str,
    def: &FunctionDef,
    raw_args: &[Token],
    interp: &mut Interpreter,
    state: &StateHandle,
) -> Flow {
    if raw_args.len() != def.params.len() {
        println!(
            "ion: {name}: expects {} argument(s), got {}",
            def.params.len(),
            raw_args.len()
        );
        return Flow::Normal;
    }

    // Resolve/validate every argument *before* isolating scope, since a raw
    // arg can reference the caller's own local variables (e.g. `square $x`
    // where `x` is local to the caller) — those need the caller's scope
    // still visible to resolve correctly.
    let mut scalar_params: Vec<(String, String)> = Vec::new();
    let mut array_params: Vec<(String, Vec<String>)> = Vec::new();
    for (param, raw) in def.params.iter().zip(raw_args) {
        if param.array {
            let mut values = interp.array_from_token(raw);
            if let Some(ty) = param.ty {
                for v in values.iter_mut() {
                    match types::validate(v, ty) {
                        Ok(normalized) => *v = normalized,
                        Err(e) => {
                            err_println!("ion: function argument has invalid type: {e}");
                            return Flow::Normal;
                        }
                    }
                }
            }
            array_params.push((param.name.clone(), values));
        } else {
            let mut value = interp.scalar_from_token(raw);
            if let Some(ty) = param.ty {
                match types::validate(&value, ty) {
                    Ok(normalized) => value = normalized,
                    Err(e) => {
                        err_println!("ion: function argument has invalid type: {e}");
                        return Flow::Normal;
                    }
                }
            }
            scalar_params.push((param.name.clone(), value));
        }
    }

    // ion-manual page 20: "Functions have the scope they were defined in"
    // — lexical, not dynamic, scoping. Hide every scope above the global
    // one for the duration of the call, so the function body can't see
    // (or accidentally clobber) whatever local variables happen to be
    // active at the call site, then bind parameters as fresh bindings in
    // their own frame (shadowing, not updating, any same-named global).
    let saved = interp.isolate_global_scope();
    interp.push_scope();
    for (name, value) in scalar_params {
        interp.define_local_scalar(name, value);
    }
    for (name, value) in array_params {
        interp.define_local_array(name, value);
    }

    let flow = exec_block(&def.body, interp, state).await;

    interp.pop_scope();
    interp.restore_scope(saved);

    // `break`/`continue` are lexically scoped to loops within this function
    // body, not the caller's — a function's own loops already consume
    // theirs via exec_block's while/for handling, so anything that escapes
    // here means a bare break/continue with no enclosing loop in this
    // function. `exit`/`quit` still terminates the whole shell regardless
    // of call depth.
    match flow {
        Flow::Break => {
            err_println!("ion: break: not inside a loop");
            Flow::Normal
        }
        Flow::LoopContinue => {
            err_println!("ion: continue: not inside a loop");
            Flow::Normal
        }
        Flow::Normal | Flow::ShellExit | Flow::Interrupted => flow,
    }
}

/// `read VARIABLE...` (ion-manual page 78): reads one line from stdin and
/// splits it by whitespace across the named scalars, with the last
/// variable capturing any remainder — matching common shell `read`
/// semantics for the multi-variable case.
fn handle_read(names: &[Token], interp: &mut Interpreter) {
    if names.is_empty() {
        err_println!("ion-win: read: usage: read VARIABLE...");
        return;
    }

    let mut line = String::new();
    if io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
        return; // EOF: leave variables untouched
    }
    let line = line.trim_end();

    let parts: Vec<&str> = if names.len() <= 1 {
        vec![line]
    } else {
        line.splitn(names.len(), char::is_whitespace).collect()
    };

    for (i, token) in names.iter().enumerate() {
        let value = parts.get(i).copied().unwrap_or("").to_string();
        interp.set_scalar(token.text.clone(), value);
    }
}

/// `fn` invoked with no name (or `-h`/`--help`) prints every defined
/// function's name and docstring (ion-manual page 74).
fn handle_fn_builtin(interp: &Interpreter) {
    let funcs = interp.list_functions();
    if funcs.is_empty() {
        println!("ion: no functions defined");
        return;
    }
    for (name, doc) in funcs {
        match doc {
            Some(d) => println!("{name} -- {d}"),
            None => println!("{name}"),
        }
    }
}

/// Spawns `program` as a child process, inheriting stdio, and returns
/// whether it exited successfully. On Windows this goes through
/// `std::process::Command` -> `CreateProcessW` directly, per the OS
/// Abstraction Layer Matrix in ARCHITECTURE.md section 5. Registered as
/// the foreground job while it runs (see `jobctl.rs`), so Ctrl+C
/// interrupts it instead of doing nothing or killing the whole shell.
fn run_external_status(program: &str, args: &[String]) -> bool {
    match jobctl::new_command(program).args(args).spawn() {
        Ok(child) => jobctl::wait_foreground(child).map(|s| s.success()).unwrap_or(false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            err_println!("ion-win: command not found: {program}");
            false
        }
        Err(e) => {
            err_println!("ion-win: failed to run '{program}': {e}");
            false
        }
    }
}

/// Statement-context wrapper around `run_external_status` that also reports
/// a non-zero exit code, matching typical shell prompt feedback. Returns
/// whether the process succeeded, for `previous_status` (`$?`) tracking.
fn run_external(program: &str, args: &[String]) -> bool {
    match jobctl::new_command(program).args(args).spawn() {
        Ok(child) => match jobctl::wait_foreground(child) {
            Ok(status) => {
                if !status.success() {
                    if let Some(code) = status.code() {
                        err_eprintln!("ion-win: '{program}' exited with status {code}");
                    }
                }
                status.success()
            }
            Err(_) => false,
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            err_println!("ion-win: command not found: {program}");
            false
        }
        Err(e) => {
            err_println!("ion-win: failed to run '{program}': {e}");
            false
        }
    }
}

async fn handle_pvar(args: &[String], state: &StateHandle) {
    match args {
        [set, rest @ ..] if set == "set" => {
            let joined = rest.join(" ");
            if let Some((key, value)) = joined.split_once('=') {
                let (key, value) = (key.trim(), value.trim().trim_matches('"'));
                match state.set_var(key, value).await {
                    Ok(()) => println!("{key} = \"{value}\""),
                    Err(e) => println!("pvar: error setting {key}: {e}"),
                }
            } else {
                println!("pvar: usage: pvar set KEY = VALUE");
            }
        }
        [get, key] if get == "get" => match state.get_var(key.clone()).await {
            Some(v) => println!("{key} = \"{v}\""),
            None => println!("pvar: no such variable: {key}"),
        },
        [list] if list == "list" => {
            for (k, v) in state.list_vars().await {
                println!("{k} = \"{v}\"");
            }
        }
        [delete, key] if delete == "delete" => match state.delete_var(key.clone()).await {
            Ok(()) => println!("deleted {key}"),
            Err(e) => println!("pvar: error deleting {key}: {e}"),
        },
        _ => println!("pvar: usage: pvar set|get|list|delete ..."),
    }
}

async fn handle_dmark(args: &[String], state: &StateHandle) {
    match args {
        [add, name] if add == "add" => {
            let cwd = std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            match state.add_bookmark(name.clone(), cwd.clone()).await {
                Ok(()) => println!("Saved: {name} -> {cwd}"),
                Err(e) => println!("dmark: error saving {name}: {e}"),
            }
        }
        [list] if list == "list" => {
            for (name, path) in state.list_bookmarks().await {
                println!("{name} -> {path}");
            }
        }
        [jump, name] if jump == "jump" => match state.get_bookmark(name.clone()).await {
            Some(path) => {
                if std::env::set_current_dir(&path).is_ok() {
                    println!("Moved to: {path}");
                } else {
                    println!("dmark: path no longer exists: {path}");
                }
            }
            None => println!("dmark: no such bookmark: {name}"),
        },
        _ => println!("dmark: usage: dmark add|list|jump ..."),
    }
}

/// `jobs` (ion-manual page 75): lists all tracked background jobs.
fn handle_jobs() {
    let jobs = jobs::list();
    if jobs.is_empty() {
        println!("ion-win: no background jobs");
        return;
    }
    for (pid, command) in jobs {
        println!("{pid}\t{command}");
    }
}

/// `disown [--help | -r | -h | -a] [PID...]` (ion-manual page 69). `-a`/
/// `-r` and a bare `disown` with no PIDs all disown *every* tracked job —
/// the manual's own wording for `-a` ("if no job IDs were supplied,
/// remove all jobs") is extended to the no-flag case too, rather than
/// inventing bash's unrelated "most recent job" default the manual never
/// mentions. `-h` ("don't forward SIGHUP") is accepted but a no-op:
/// Windows console apps have no real SIGHUP equivalent for ion-win to
/// forward in the first place.
fn handle_disown(args: &[String]) {
    let mut pids: Vec<u32> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "-a" | "-r" | "-h" => {}
            "--help" => {
                println!("disown [--help | -r | -h | -a] [PID...]");
                return;
            }
            other => match other.parse::<u32>() {
                Ok(pid) => pids.push(pid),
                Err(_) => {
                    err_println!("ion-win: disown: '{other}': not a valid PID");
                    return;
                }
            },
        }
    }
    let count = jobs::disown(&pids);
    println!("ion-win: disowned {count} job(s)");
}

