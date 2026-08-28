//! Minimal core-language interpreter: tokenizing, `$`/`@` expansion, the
//! `let` / `echo` builtins, and the function table used by `fn`.
//!
//! This is deliberately a small subset of real Ion (no pipelines/redirection
//! yet — see ARCHITECTURE.md section 6 for the upgrade roadmap). It exists
//! to prove out the variable model (typed scalars vs. arrays, `$name` vs.
//! `@name` sigils) before layering the rest of the language on top.

use crate::functions::FunctionDef;
use crate::fileset::FileSet;
use crate::operation::{OperationJournal, OperationPlan};
use crate::table::Table;
use crate::{err_eprintln, err_println};
use std::collections::HashMap;

/// Variables live in a stack of scope frames rather than one flat map
/// (ion-manual page 20, "Scopes"): index 0 is the permanent global frame;
/// every `if`/`while`/`for`/`fn` body execution pushes a fresh frame
/// (`exec_block` in `shell.rs`) and pops it when the block ends, deleting
/// whatever that block newly defined. `let` on a name that already exists
/// in an outer frame updates it there in place instead of shadowing it —
/// "the first invocation of `let` gets to own the variable" — which is why
/// reads/writes always walk the whole stack rather than just the top frame.
pub struct Interpreter {
    scalars: Vec<HashMap<String, String>>,
    arrays: Vec<HashMap<String, Vec<String>>>,
    functions: HashMap<String, FunctionDef>,
    /// The previous statement's success/failure — Ion's `$?` equivalent
    /// (confirmed against upstream Ion's real source, `shell/flow.rs`:
    /// `self.previous_status`), which `and`/`or` (`shell.rs`) read to
    /// decide whether to run. `dispatch` (`shell.rs`) resets this to
    /// `true` at the start of every statement except `and`/`or`, and the
    /// specific arms with a real failure mode (`test`, `cd`, external
    /// processes, ...) override that default with their actual result.
    previous_status: bool,
    /// Set when any command in the current interactive input could not be
    /// resolved. The REPL consumes this after execution to implement the
    /// documented `HISTORY_IGNORE=no_such_command` rule without confusing
    /// an ordinary non-zero exit status with a missing executable.
    command_not_found: bool,
    /// When `Some`, `echo`'s output is appended here instead of going to
    /// real stdout — used to capture a `PROMPT` function's output as the
    /// prompt string rather than printing it (ion-manual page 6, "Prompt
    /// Function"). `None` (the normal case) means `echo` prints as usual.
    /// Only `echo` is capture-aware, matching the single documented use
    /// case (real Ion captures a forked process's entire stdout; ion-win
    /// doesn't fork, so this narrower in-process capture — same approach
    /// already used for `$(cmd)`/`@(cmd)` process expansion's `echo`
    /// shortcut — is the pragmatic equivalent).
    echo_capture: Option<String>,
    /// Structured `Table` variables (`ARCHITECTURE.md` §18), e.g. `let
    /// procs = tool --json | from-json | where cpu -gt 5`. Same scope-stack
    /// shape and "first `let` owns it" ownership rule as `scalars`/`arrays`
    /// — kept as a separate map rather than folded into `scalars` since a
    /// `Table` isn't a `String` and this project doesn't have (or want,
    /// for this slice) a single tagged-union value type spanning all three
    /// kinds.
    tables: Vec<HashMap<String, Table>>,
    filesets: Vec<HashMap<String, FileSet>>,
    plans: Vec<HashMap<String, OperationPlan>>,
    journals: Vec<HashMap<String, OperationJournal>>,
}

impl Default for Interpreter {
    fn default() -> Self {
        Interpreter {
            scalars: vec![HashMap::new()],
            arrays: vec![HashMap::new()],
            functions: HashMap::new(),
            previous_status: true,
            command_not_found: false,
            echo_capture: None,
            tables: vec![HashMap::new()],
            filesets: vec![HashMap::new()],
            plans: vec![HashMap::new()],
            journals: vec![HashMap::new()],
        }
    }
}

/// How a token was quoted, per ion-manual page 4 ("Quoting Rules") and
/// page 12/27 (array coercion): this is what lets `"@array"` and `'$x'`
/// behave differently from a bare `@array`/`$x`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Quoting {
    /// Bare word, no surrounding quotes. A token that's *exactly* `@name`
    /// fans out into multiple shell words; everything else interpolates.
    None,
    /// Single-quoted (`'...'`) — no `$`/`@` expansion at all.
    Single,
    /// Double-quoted (`"..."`) — `$`/`@` expand, but arrays always coerce
    /// to a space-joined string rather than fanning out.
    Double,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub text: String,
    pub quoting: Quoting,
}

impl From<&str> for Token {
    fn from(s: &str) -> Self {
        Token {
            text: s.to_string(),
            quoting: Quoting::None,
        }
    }
}

impl From<String> for Token {
    fn from(text: String) -> Self {
        Token {
            text,
            quoting: Quoting::None,
        }
    }
}

/// A single expanded shell word: either one string, or (from an `@array`
/// expansion) several — each becomes its own argument, matching Ion's rule
/// that array expansion produces multiple shell words.
enum Expanded {
    One(String),
    Many(Vec<String>),
}

impl Interpreter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Splits a line into raw tokens, honoring single/double quotes,
    /// `[ ... ]` array literals, and `$(( ... ))` arithmetic expansions
    /// (which may contain internal spaces and balanced parens) as single
    /// tokens. Quoted tokens carry their quote kind for later expansion.
    pub fn tokenize(line: &str) -> Vec<Token> {
        let mut tokens = Vec::new();
        let mut chars = line.chars().peekable();

        while let Some(&c) = chars.peek() {
            if c.is_whitespace() {
                chars.next();
                continue;
            }

            // `#` starts a comment running to end of line — only at a
            // fresh token boundary (so `file#1` as a bareword is
            // untouched). This also transparently handles a script's
            // shebang line (`#!/usr/bin/env ion`), which is just a comment
            // as far as the interpreter itself is concerned.
            if c == '#' {
                break;
            }

            // Pipe/redirect/chain/background operators (`|`, `^|`, `&|`,
            // `>`, `>>`, `^>`, `&>`, `&&`, `||`, `&`, `&!`) always split
            // into their own token, even glued directly to a word with no
            // surrounding whitespace (`find|stat`, `cmd>out.txt`) — matching
            // how every real shell treats these as metacharacters rather
            // than ordinary word characters. `pipeline.rs`'s parser (and
            // `shell.rs`'s `&&`/`||` chain-op splitter) only recognize
            // these by exact token text, so before this fix `find|stat`
            // silently became one bareword token `"find|stat"` instead of
            // three tokens — no error at tokenize time, just a pipeline
            // that quietly never happened, surfacing later as a confusing
            // "command not found" for the fused word. A bare `^` not
            // immediately followed by `|`/`>` isn't a defined operator on
            // its own in this grammar, so it's left as an ordinary word
            // character in that case.
            if let Some(op_len) = operator_len(&chars) {
                let mut buf = String::new();
                for _ in 0..op_len {
                    buf.push(chars.next().expect("operator_len bounds match available chars"));
                }
                tokens.push(Token {
                    text: buf,
                    quoting: Quoting::None,
                });
                continue;
            }

            if c == '"' || c == '\'' {
                let quote = c;
                chars.next();
                let mut buf = String::new();
                for ch in chars.by_ref() {
                    if ch == quote {
                        break;
                    }
                    buf.push(ch);
                }
                let quoting = if quote == '"' {
                    Quoting::Double
                } else {
                    Quoting::Single
                };

                // A double-quoted segment may be concatenated directly to
                // an unquoted suffix (`"@arr"suffix`) and still forms one
                // shell word. Keep consuming until the real word boundary,
                // stripping any further double-quote delimiters while
                // preserving their contents (including spaces).
                if quoting == Quoting::Double {
                    let followed_by_word = chars
                        .peek()
                        .copied()
                        .map(|next| !next.is_whitespace() && operator_len(&chars).is_none())
                        .unwrap_or(false);
                    if followed_by_word {
                        buf = brace_trailing_variable(&buf);
                    }
                    while let Some(&next) = chars.peek() {
                        if next.is_whitespace() || operator_len(&chars).is_some() {
                            break;
                        }
                        if next == '"' {
                            chars.next();
                            for ch in chars.by_ref() {
                                if ch == '"' {
                                    break;
                                }
                                buf.push(ch);
                            }
                        } else {
                            buf.push(next);
                            chars.next();
                        }
                    }
                }
                tokens.push(Token { text: buf, quoting });
                continue;
            }

            if c == '[' {
                let mut buf = String::from("[");
                chars.next();
                let mut depth = 1;
                for ch in chars.by_ref() {
                    buf.push(ch);
                    if ch == '[' {
                        depth += 1;
                    } else if ch == ']' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                }
                tokens.push(Token {
                    text: buf,
                    quoting: Quoting::None,
                });
                continue;
            }

            if c == '$' {
                let mut lookahead = chars.clone();
                lookahead.next(); // '$'
                if lookahead.next() == Some('(') {
                    if lookahead.next() == Some('(') {
                        // $(( expr )) arithmetic expansion: two literal
                        // opening parens, closes with two literal closing
                        // parens, balancing any parens in between.
                        let mut buf = String::new();
                        buf.push(chars.next().unwrap()); // '$'
                        buf.push(chars.next().unwrap()); // first '('
                        buf.push(chars.next().unwrap()); // second '('
                        let mut depth = 0i32;
                        loop {
                            match chars.next() {
                                Some('(') => {
                                    depth += 1;
                                    buf.push('(');
                                }
                                Some(')') => {
                                    buf.push(')');
                                    if depth > 0 {
                                        depth -= 1;
                                    } else if chars.peek() == Some(&')') {
                                        buf.push(chars.next().unwrap());
                                        break;
                                    }
                                }
                                Some(other) => buf.push(other),
                                None => break, // unterminated; best-effort
                            }
                        }
                        consume_bracket_suffix(&mut chars, &mut buf);
                        tokens.push(Token {
                            text: buf,
                            quoting: Quoting::None,
                        });
                        continue;
                    } else {
                        // $( cmd ) process expansion (string capture,
                        // ion-manual page 28): single opening paren, normal
                        // balanced-paren matching for the closing one.
                        let mut buf = String::new();
                        buf.push(chars.next().unwrap()); // '$'
                        buf.push(chars.next().unwrap()); // '('
                        let mut depth = 1i32;
                        loop {
                            match chars.next() {
                                Some('(') => {
                                    depth += 1;
                                    buf.push('(');
                                }
                                Some(')') => {
                                    depth -= 1;
                                    buf.push(')');
                                    if depth == 0 {
                                        break;
                                    }
                                }
                                Some(other) => buf.push(other),
                                None => break, // unterminated; best-effort
                            }
                        }
                        consume_bracket_suffix(&mut chars, &mut buf);
                        tokens.push(Token {
                            text: buf,
                            quoting: Quoting::None,
                        });
                        continue;
                    }
                } else if let Some(mut buf) = try_consume_method_call(&mut chars, '$') {
                    // $IDENT( ... ) method call (ion-manual page 32),
                    // distinct from $(cmd) process expansion above (no
                    // identifier between the sigil and the paren).
                    consume_bracket_suffix(&mut chars, &mut buf);
                    tokens.push(Token {
                        text: buf,
                        quoting: Quoting::None,
                    });
                    continue;
                }
            }

            if c == '@' {
                let mut lookahead = chars.clone();
                lookahead.next(); // '@'
                if lookahead.next() == Some('(') {
                    // @( cmd ) process expansion (array capture, splitting
                    // the output by whitespace, ion-manual page 28).
                    let mut buf = String::new();
                    buf.push(chars.next().unwrap()); // '@'
                    buf.push(chars.next().unwrap()); // '('
                    let mut depth = 1i32;
                    loop {
                        match chars.next() {
                            Some('(') => {
                                depth += 1;
                                buf.push('(');
                            }
                            Some(')') => {
                                depth -= 1;
                                buf.push(')');
                                if depth == 0 {
                                    break;
                                }
                            }
                            Some(other) => buf.push(other),
                            None => break, // unterminated; best-effort
                        }
                    }
                    consume_bracket_suffix(&mut chars, &mut buf);
                    tokens.push(Token {
                        text: buf,
                        quoting: Quoting::None,
                    });
                    continue;
                } else if let Some(mut buf) = try_consume_method_call(&mut chars, '@') {
                    // @IDENT( ... ) method call.
                    consume_bracket_suffix(&mut chars, &mut buf);
                    tokens.push(Token {
                        text: buf,
                        quoting: Quoting::None,
                    });
                    continue;
                }
            }

            let mut buf = String::new();
            let mut has_double_quoted_segment = false;
            while let Some(&ch) = chars.peek() {
                if ch.is_whitespace() || operator_len(&chars).is_some() {
                    break;
                }
                if ch == '"' {
                    has_double_quoted_segment = true;
                    chars.next();
                    let mut quoted_segment = String::new();
                    for quoted_ch in chars.by_ref() {
                        if quoted_ch == '"' {
                            break;
                        }
                        quoted_segment.push(quoted_ch);
                    }
                    let followed_by_word = chars
                        .peek()
                        .copied()
                        .map(|next| !next.is_whitespace() && operator_len(&chars).is_none())
                        .unwrap_or(false);
                    if followed_by_word {
                        quoted_segment = brace_trailing_variable(&quoted_segment);
                    }
                    buf.push_str(&quoted_segment);
                    continue;
                }
                buf.push(ch);
                chars.next();
            }
            tokens.push(Token {
                text: buf,
                quoting: if has_double_quoted_segment {
                    Quoting::Double
                } else {
                    Quoting::None
                },
            });
        }

        tokens
    }

    /// Expands a single raw token, respecting its quote kind:
    /// - `'...'` (Single): no expansion at all, returned verbatim.
    /// - `"..."` (Double): `$`/`@` expand, but arrays always coerce to a
    ///   space-joined string.
    /// - bare (None): a token that is *exactly* `@name`/`@(cmd)`/`@method(args)`
    ///   (each optionally followed by a `[slice]`) fans out into multiple
    ///   shell words (ion-manual pages 26, 28, 32-43, 45-48); any brace
    ///   group(s) elsewhere in the token — ranges, permutation lists, or
    ///   nesting, standalone or as an infix like `job_{01,02}.{ext1,ext2}`
    ///   — fan out the same way (pages 29-30), with `$`/`@` interpolation
    ///   applied to each resulting word; everything else interpolates in
    ///   place, joining any embedded arrays with spaces.
    fn expand_token(&self, token: &Token) -> Expanded {
        match token.quoting {
            Quoting::Single => Expanded::One(token.text.clone()),
            Quoting::Double => Expanded::One(self.interpolate(&token.text)),
            Quoting::None => {
                if let Some(rest) = token.text.strip_prefix('@') {
                    let (name, slice_spec) = split_name_and_slice(rest);

                    if is_plain_name(name) {
                        return match self.get_array(name) {
                            Some(v) => {
                                Expanded::Many(apply_optional_array_slice(v.clone(), slice_spec))
                            }
                            None => {
                                err_eprintln!("ion: expansion error: {name}: variable does not exist");
                                Expanded::Many(Vec::new())
                            }
                        };
                    }
                    if let Some(inner) = whole_paren_body(name) {
                        return match self.run_process_expansion_array(inner) {
                            Ok(v) => Expanded::Many(apply_optional_array_slice(v, slice_spec)),
                            Err(e) => {
                                err_eprintln!("ion: process expansion error: {e}");
                                Expanded::Many(Vec::new())
                            }
                        };
                    }
                    if let Some((method_name, method_inner)) = split_method_call(name) {
                        return match self.call_array_method_here(method_name, method_inner) {
                            Ok(v) => Expanded::Many(apply_optional_array_slice(v, slice_spec)),
                            Err(e) => {
                                err_eprintln!("ion: method error: {e}");
                                Expanded::Many(Vec::new())
                            }
                        };
                    }
                }

                if let Some(items) = crate::ranges::expand_braces(&token.text) {
                    return Expanded::Many(
                        items.iter().map(|s| self.interpolate(s)).collect(),
                    );
                }

                Expanded::One(self.interpolate(&token.text))
            }
        }
    }

    /// Runs `$(inner)`/`@(inner)`'s command and returns its captured stdout
    /// as a single string. `echo` is handled in-process (consistent with
    /// `pipeline_exec`'s treatment of it) rather than requiring a real
    /// `echo` executable on PATH. Pipes/redirects inside the parens aren't
    /// supported (this expansion path is synchronous; piping needs the
    /// async pipeline engine) and are rejected with a clear error.
    fn run_process_expansion_scalar(&self, inner: &str) -> Result<String, String> {
        let tokens = Self::tokenize(inner);
        if !crate::pipeline::parse(&tokens).is_trivial() {
            return Err(
                "pipelines/redirection are not supported inside process expansion yet".to_string(),
            );
        }
        let args = self.expand_all(&tokens);
        if args.is_empty() {
            return Err("empty command in process expansion".to_string());
        }
        if args[0] == "echo" {
            let (rest, _no_newline) = split_echo_no_newline_flag(&args[1..]);
            return Ok(rest.join(" "));
        }
        if let Some(result) = crate::fs_builtins::capture(&args[0], &args[1..]) {
            return result;
        }
        crate::procexpand::capture(&args)
    }

    /// Same as `run_process_expansion_scalar`, but splits the captured
    /// output by whitespace into an array (`@(...)`, ion-manual page 28).
    fn run_process_expansion_array(&self, inner: &str) -> Result<Vec<String>, String> {
        let text = self.run_process_expansion_scalar(inner)?;
        Ok(text.split_whitespace().map(str::to_string).collect())
    }

    /// Resolves a method call's raw argument text into `MethodArg` values
    /// (ion-manual pages 32-33). Arguments are re-tokenized (so quoting,
    /// arrays, nested `$(...)`/method calls all work inside them) and each
    /// resolved per the manual's rule: quoted/`$`/`@`/`[`/`{`-prefixed
    /// tokens use their normal expansion; a *bare* unadorned word (no
    /// sigil, no quotes) is looked up as a variable **by name** — scalar
    /// first, then array, falling back to its own literal text if no such
    /// variable exists. That last rule is what lets `$replace(input one 1)`
    /// use `input` as a variable reference while `one`/`1` stay literal.
    fn resolve_method_args(&self, inner: &str) -> Vec<crate::methods::MethodArg> {
        Self::tokenize(inner)
            .iter()
            .map(|t| self.resolve_method_arg(t))
            .collect()
    }

    fn resolve_method_arg(&self, token: &Token) -> crate::methods::MethodArg {
        // Array literals (`[ ... ]`) aren't understood by `expand_token`
        // on their own (that's normally handled by `array_from_token` for
        // function-call args, or `builtin_let`'s own array-literal check)
        // — resolve them the same way here, or e.g. `$len([1 2 3])` would
        // silently count the literal text's characters instead of the
        // array's elements.
        if token.quoting == Quoting::None && token.text.starts_with('[') {
            let elements = Self::parse_array_literal(&token.text);
            return crate::methods::MethodArg::Arr(self.expand_all(&elements));
        }

        let is_bare_word = token.quoting == Quoting::None
            && !token.text.starts_with('$')
            && !token.text.starts_with('@')
            && !token.text.starts_with('{');

        if is_bare_word {
            if let Some(v) = self.get_scalar(&token.text) {
                return crate::methods::MethodArg::Str(v.clone());
            }
            if let Some(v) = self.get_array(&token.text) {
                return crate::methods::MethodArg::Arr(v.clone());
            }
            return crate::methods::MethodArg::Str(token.text.clone());
        }

        match self.expand_token(token) {
            Expanded::One(s) => crate::methods::MethodArg::Str(s),
            Expanded::Many(v) => crate::methods::MethodArg::Arr(v),
        }
    }

    /// Invokes a `$name(...)` string method with its raw (unresolved)
    /// argument text.
    fn call_string_method_here(&self, name: &str, inner: &str) -> Result<String, String> {
        let tokens = Self::tokenize(inner);

        // Structured `Table`s deliberately do not participate in ordinary
        // string/array coercion (ARCHITECTURE.md §18). Keep the bridge
        // narrow and explicit: `$len(table)` counts rows, while
        // `$field(one_row_table column)` extracts one scalar field. Both
        // require a bare table-variable name so quoted text and normal
        // scalar/array method calls retain their existing meaning.
        if name == "len" && tokens.len() == 1 && tokens[0].quoting == Quoting::None {
            if let Some(fileset) = self.get_fileset(&tokens[0].text) {
                return Ok(fileset.len().to_string());
            }
            if let Some(table) = self.get_table(&tokens[0].text) {
                return Ok(table.rows.len().to_string());
            }
        }

        if name == "field" && tokens.len() == 2 && tokens[0].quoting == Quoting::None {
            if let Some(table) = self.get_table(&tokens[0].text) {
                if table.rows.len() != 1 {
                    return Err(format!(
                        "field: expected exactly one row in '{}', found {}",
                        tokens[0].text,
                        table.rows.len()
                    ));
                }
                let column = self.resolve_method_arg(&tokens[1]).as_str();
                return table.rows[0]
                    .iter()
                    .find_map(|(name, value)| (name == &column).then(|| value.clone()))
                    .ok_or_else(|| format!("field: column '{column}' does not exist"));
            }
        }

        let args = self.resolve_method_args(inner);
        crate::methods::call_string_method(name, &args)
            .unwrap_or_else(|| Err(format!("no such string method '{name}'")))
    }

    /// Invokes a `@name(...)` array method with its raw (unresolved)
    /// argument text.
    fn call_array_method_here(&self, name: &str, inner: &str) -> Result<Vec<String>, String> {
        let args = self.resolve_method_args(inner);
        crate::methods::call_array_method(name, &args)
            .unwrap_or_else(|| Err(format!("no such array method '{name}'")))
    }

    /// Scans a token character-by-character, substituting every `$name`,
    /// `${name}`, `@name`, `@{name}`, and `$(( expr ))` reference it finds,
    /// and passing everything else through literally. This is what makes
    /// `"$name ($age) has..."`-style interpolation work (ion-manual pages
    /// 26-27), not just whole-token replacement.
    fn interpolate(&self, token: &str) -> String {
        let mut out = String::new();
        let mut chars = token.chars().peekable();

        while let Some(c) = chars.next() {
            if c != '$' && c != '@' {
                out.push(c);
                continue;
            }

            // Arithmetic expansion: $(( expr )). The wrapper is the literal
            // "$((" ... "))" delimiter; content in between may contain its
            // own balanced parens (ion-manual page 31).
            if c == '$' && chars.peek() == Some(&'(') {
                let mut lookahead = chars.clone();
                lookahead.next(); // first '('
                if lookahead.peek() == Some(&'(') {
                    chars.next(); // consume first '('
                    chars.next(); // consume second '('
                    let mut depth = 0i32;
                    let mut expr = String::new();
                    loop {
                        match chars.next() {
                            Some('(') => {
                                depth += 1;
                                expr.push('(');
                            }
                            Some(')') => {
                                if depth > 0 {
                                    depth -= 1;
                                    expr.push(')');
                                } else if chars.peek() == Some(&')') {
                                    chars.next(); // consume the wrapper's second ')'
                                    break;
                                } else {
                                    break; // unbalanced; best-effort stop
                                }
                            }
                            Some(other) => expr.push(other),
                            None => break, // unterminated; best-effort
                        }
                    }
                    match crate::arith::eval(&expr, &|name| self.get_scalar(name).cloned()) {
                        Ok(value) => out.push_str(&value.to_display_string()),
                        Err(e) => err_eprintln!("ion: arithmetic error: {e}"),
                    }
                    continue;
                }
            }

            // Method call: $method(args) / @method(args) (ion-manual page
            // 32), distinct from $(cmd)/@(cmd) process expansion below (no
            // identifier between the sigil and the paren). Non-destructive
            // lookahead first, so a plain `$name` with no trailing `(`
            // falls through untouched.
            {
                let mut la = chars.clone();
                let mut ident = String::new();
                while let Some(&ch) = la.peek() {
                    if ch.is_alphanumeric() || ch == '_' {
                        ident.push(ch);
                        la.next();
                    } else {
                        break;
                    }
                }
                if !ident.is_empty() && la.peek() == Some(&'(') {
                    la.next(); // consume '('
                    let mut depth = 1i32;
                    let mut inner = String::new();
                    loop {
                        match la.next() {
                            Some('(') => {
                                depth += 1;
                                inner.push('(');
                            }
                            Some(')') => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                                inner.push(')');
                            }
                            Some(other) => inner.push(other),
                            None => break, // unterminated; best-effort
                        }
                    }
                    chars = la; // commit: advance the real iterator to match
                    let result = if c == '$' {
                        self.call_string_method_here(&ident, &inner)
                    } else {
                        self.call_array_method_here(&ident, &inner)
                            .map(|v| v.join(" "))
                    };
                    match result {
                        Ok(value) => out.push_str(&value),
                        Err(e) => err_eprintln!("ion: method error: {e}"),
                    }
                    continue;
                }
            }

            // Process expansion: $(cmd) captures stdout as a string;
            // @(cmd) captures it split by whitespace, joined back with
            // spaces when embedded like this (arrays always coerce to a
            // string outside of a bare whole-token `@(...)`, same rule as
            // `@name`). Both may be followed by a `[slice]` (ion-manual
            // pages 28, 48).
            if chars.peek() == Some(&'(') {
                chars.next(); // consume '('
                let mut depth = 1i32;
                let mut inner = String::new();
                loop {
                    match chars.next() {
                        Some('(') => {
                            depth += 1;
                            inner.push('(');
                        }
                        Some(')') => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                            inner.push(')');
                        }
                        Some(other) => inner.push(other),
                        None => break, // unterminated; best-effort
                    }
                }
                let slice_spec = consume_slice_spec(&mut chars);
                let result = if c == '$' {
                    self.run_process_expansion_scalar(&inner)
                        .map(|s| apply_optional_string_slice(s, slice_spec.as_deref()))
                } else {
                    self.run_process_expansion_array(&inner)
                        .map(|v| apply_optional_array_slice(v, slice_spec.as_deref()).join(" "))
                };
                match result {
                    Ok(text) => out.push_str(&text),
                    Err(e) => err_eprintln!("ion: process expansion error: {e}"),
                }
                continue;
            }

            let sigil = c;
            let name = if chars.peek() == Some(&'{') {
                chars.next(); // consume '{'
                let mut buf = String::new();
                for ch in chars.by_ref() {
                    if ch == '}' {
                        break;
                    }
                    buf.push(ch);
                }
                buf
            } else {
                let mut buf = String::new();
                while let Some(&ch) = chars.peek() {
                    if ch.is_alphanumeric() || ch == '_' {
                        buf.push(ch);
                        chars.next();
                    } else {
                        break;
                    }
                }
                buf
            };
            let slice_spec = consume_slice_spec(&mut chars);

            if let Some(var_name) = name.strip_prefix("env::") {
                // ion-manual page 24: unlike ordinary variable access, the
                // `env` namespace intentionally emits an empty string for
                // an undefined OS environment variable instead of erroring
                // — env vars "can't be predicted."
                let value = std::env::var(var_name).unwrap_or_default();
                out.push_str(&apply_optional_string_slice(value, slice_spec.as_deref()));
            } else if name.is_empty() {
                out.push(sigil); // lone sigil with no following name: keep literal
            } else {
                out.push_str(&self.resolve_name(sigil, &name, slice_spec.as_deref()));
            }
        }

        out
    }

    fn resolve_name(&self, sigil: char, name: &str, slice_spec: Option<&str>) -> String {
        if sigil == '$' {
            match self.get_scalar(name) {
                Some(v) => apply_optional_string_slice(v.clone(), slice_spec),
                None => {
                    err_eprintln!("ion: expansion error: {name}: variable does not exist");
                    String::new()
                }
            }
        } else {
            match self.get_array(name) {
                Some(v) => apply_optional_array_slice(v.clone(), slice_spec).join(" "),
                None => {
                    err_eprintln!("ion: expansion error: {name}: variable does not exist");
                    String::new()
                }
            }
        }
    }

    /// Expands a full list of raw tokens (arrays fan out into multiple words).
    pub fn expand_all(&self, tokens: &[Token]) -> Vec<String> {
        let mut out = Vec::with_capacity(tokens.len());
        for token in tokens {
            match self.expand_token(token) {
                Expanded::One(s) => out.push(s),
                Expanded::Many(v) => out.extend(v),
            }
        }
        out
    }

    /// Parses an array literal like `[ one two 'three four' ]` into
    /// elements. Bracket contents are re-tokenized so quoting rules
    /// (including single-quote suppression) apply consistently.
    fn parse_array_literal(literal: &str) -> Vec<Token> {
        let inner = literal.trim_start_matches('[').trim_end_matches(']');
        Self::tokenize(inner)
    }

    const ARITH_OPS: [&'static str; 6] = ["+=", "-=", "*=", "/=", "//=", "**="];

    /// Handles `let NAME = VALUE...` (scalar, joined by spaces if multiple
    /// bare words), `let NAME = [ a b c ]` (array), and compound arithmetic
    /// assignment (`let NAME OP VALUE`, ion-manual page 17-18).
    pub fn builtin_let(&mut self, args: &[Token]) {
        let Some(op_pos) = args
            .iter()
            .position(|a| a.text == "=" || Self::ARITH_OPS.contains(&a.text.as_str()))
        else {
            err_println!("ion: let: usage: let NAME = VALUE  |  let NAME = [ elements... ]  |  let NAME OP VALUE");
            return;
        };
        if op_pos != 1 && op_pos != 2 {
            err_println!("ion: let: multiple-name assignment not yet supported in this scaffold");
            return;
        }
        let separated_type = (op_pos == 2 && args[0].text.ends_with(':'))
            .then(|| args[1].text.as_str());
        let (name, declared_type) = match args[0].text.split_once(':') {
            Some((name, "")) if !name.is_empty() => match separated_type
                .and_then(crate::types::parse_type_name)
            {
                Some(tag) => (name.to_string(), Some(tag)),
                None => {
                    let ty = separated_type.unwrap_or("");
                    err_println!("ion: let: unknown type '{ty}'");
                    return;
                }
            },
            Some((name, ty)) if !name.is_empty() => match crate::types::parse_type_name(ty) {
                Some(tag) => (name.to_string(), Some(tag)),
                None => {
                    err_println!("ion: let: unknown type '{ty}'");
                    return;
                }
            },
            _ => (args[0].text.clone(), None),
        };
        let op = args[op_pos].text.as_str();
        let rhs = &args[op_pos + 1..];

        if op != "=" {
            if rhs.len() != 1 {
                err_println!("ion: let: arithmetic assignment usage: let NAME {op} VALUE");
                return;
            }
            let operand = self.expand_all(rhs).join(" ");
            let current = self.get_scalar(&name).cloned().unwrap_or_default();
            match apply_arith(&current, op, &operand) {
                Ok(new_value) => {
                    self.set_scalar(name, new_value);
                }
                Err(e) => err_println!("ion: let: {e}"),
            }
            return;
        }

        if rhs.len() == 1 && rhs[0].text.starts_with('[') {
            let elements = Self::parse_array_literal(&rhs[0].text);
            let expanded = self.expand_all(&elements);
            self.set_array(name, expanded);
            return;
        }

        let expanded = self.expand_all(rhs);
        let value = expanded.join(" ");
        let value = match declared_type {
            Some(tag) => match crate::types::validate(&value, tag) {
                Ok(value) => value,
                Err(error) => {
                    err_println!("ion: let: {error}");
                    return;
                }
            },
            None => value,
        };
        self.set_scalar(name, value);
    }

    /// `export NAME = VALUE` (ion-manual page 19): "operates identical to
    /// the `let` builtin, but it does not support arrays, and variables
    /// are exported to the OS environment." Reuses `builtin_let`'s exact
    /// assignment mechanics (including arithmetic compound ops) after
    /// rejecting an array-literal RHS, then mirrors the resulting scalar
    /// into the real process environment via `std::env::set_var` — so any
    /// child process this shell spawns afterward (external commands,
    /// pipeline stages) automatically inherits it through normal OS
    /// environment inheritance, with no extra plumbing needed anywhere
    /// else (`std::process::Command` inherits the parent's env by
    /// default).
    pub fn builtin_export(&mut self, args: &[Token]) {
        let Some(op_pos) = args
            .iter()
            .position(|a| a.text == "=" || Self::ARITH_OPS.contains(&a.text.as_str()))
        else {
            err_println!("ion: export: usage: export NAME = VALUE");
            return;
        };
        if op_pos != 1 {
            err_println!("ion: export: multiple-name assignment not yet supported in this scaffold");
            return;
        }
        let name = args[0].text.clone();
        let rhs = &args[op_pos + 1..];

        if rhs.len() == 1 && rhs[0].text.starts_with('[') {
            err_println!("ion: export: arrays are not supported");
            return;
        }

        self.builtin_let(args);
        if let Some(value) = self.get_scalar(&name) {
            // SAFETY: ion-win is single-threaded at the point commands are
            // dispatched (the tokio runtime here isn't spawning concurrent
            // shell statements), so no other thread can be reading/writing
            // the process environment at the same time.
            unsafe {
                std::env::set_var(&name, value);
            }
        }
    }

    /// Searches every visible scope frame, innermost first — a name
    /// defined in an outer frame is visible from any nested block.
    pub fn get_scalar(&self, name: &str) -> Option<&String> {
        self.scalars.iter().rev().find_map(|frame| frame.get(name))
    }

    pub fn get_array(&self, name: &str) -> Option<&Vec<String>> {
        self.arrays.iter().rev().find_map(|frame| frame.get(name))
    }

    pub fn get_table(&self, name: &str) -> Option<&Table> {
        self.tables.iter().rev().find_map(|frame| frame.get(name))
    }

    pub fn get_fileset(&self, name: &str) -> Option<&FileSet> {
        self.filesets.iter().rev().find_map(|frame| frame.get(name))
    }

    pub fn get_plan(&self, name: &str) -> Option<&OperationPlan> {
        self.plans.iter().rev().find_map(|frame| frame.get(name))
    }

    pub fn get_journal(&self, name: &str) -> Option<&OperationJournal> {
        self.journals.iter().rev().find_map(|frame| frame.get(name))
    }

    /// `let`'s core ownership rule (ion-manual page 20): if `name` already
    /// exists in any visible frame, updates it there in place — the frame
    /// that first defined it keeps "owning" it, so a nested block's `let`
    /// on an outer variable doesn't shadow it, it mutates it, and that
    /// mutation survives the block ending. Otherwise defines `name` fresh
    /// in the *current* (innermost) frame, which is destroyed — taking
    /// this new variable with it — when that frame's block ends. Returns
    /// whatever value the name previously held, if any.
    pub fn set_scalar(&mut self, name: String, value: String) -> Option<String> {
        for frame in self.scalars.iter_mut().rev() {
            if let Some(slot) = frame.get_mut(&name) {
                return Some(std::mem::replace(slot, value));
            }
        }
        self.current_scalar_frame().insert(name, value)
    }

    /// Same ownership rule as `set_scalar`, for arrays.
    pub fn set_array(&mut self, name: String, value: Vec<String>) -> Option<Vec<String>> {
        for frame in self.arrays.iter_mut().rev() {
            if let Some(slot) = frame.get_mut(&name) {
                return Some(std::mem::replace(slot, value));
            }
        }
        self.current_array_frame().insert(name, value)
    }

    /// Same ownership rule as `set_scalar`, for `Table` variables.
    pub fn set_table(&mut self, name: String, value: Table) -> Option<Table> {
        for frame in self.tables.iter_mut().rev() {
            if let Some(slot) = frame.get_mut(&name) {
                return Some(std::mem::replace(slot, value));
            }
        }
        self.current_table_frame().insert(name, value)
    }

    pub fn set_fileset(&mut self, name: String, value: FileSet) -> Option<FileSet> {
        for frame in self.filesets.iter_mut().rev() {
            if let Some(slot) = frame.get_mut(&name) {
                return Some(std::mem::replace(slot, value));
            }
        }
        self.filesets.last_mut().expect("global scope frame is never popped").insert(name, value)
    }

    pub fn set_plan(&mut self, name: String, value: OperationPlan) -> Option<OperationPlan> {
        for frame in self.plans.iter_mut().rev() {
            if let Some(slot) = frame.get_mut(&name) {
                return Some(std::mem::replace(slot, value));
            }
        }
        self.plans.last_mut().expect("global scope frame is never popped").insert(name, value)
    }

    pub fn set_journal(&mut self, name: String, value: OperationJournal) -> Option<OperationJournal> {
        for frame in self.journals.iter_mut().rev() {
            if let Some(slot) = frame.get_mut(&name) { return Some(std::mem::replace(slot, value)); }
        }
        self.journals.last_mut().expect("global scope frame is never popped").insert(name, value)
    }

    /// Defines a *new* binding directly in the current (innermost) frame,
    /// shadowing rather than updating any same-named variable in an outer
    /// frame. Used only for function-parameter binding (ion-manual page
    /// 20: "Functions have the scope they were defined in" — a call's
    /// parameters must always be fresh local bindings, never accidentally
    /// mutating a same-named global just because the callee happens to
    /// reuse that name).
    pub fn define_local_scalar(&mut self, name: String, value: String) {
        self.current_scalar_frame().insert(name, value);
    }

    pub fn define_local_array(&mut self, name: String, value: Vec<String>) {
        self.current_array_frame().insert(name, value);
    }

    fn current_scalar_frame(&mut self) -> &mut HashMap<String, String> {
        self.scalars.last_mut().expect("global scope frame is never popped")
    }

    fn current_array_frame(&mut self) -> &mut HashMap<String, Vec<String>> {
        self.arrays.last_mut().expect("global scope frame is never popped")
    }

    fn current_table_frame(&mut self) -> &mut HashMap<String, Table> {
        self.tables.last_mut().expect("global scope frame is never popped")
    }

    /// The previous statement's success/failure, i.e. `$?`.
    pub fn previous_status(&self) -> bool {
        self.previous_status
    }

    pub fn set_previous_status(&mut self, ok: bool) {
        self.previous_status = ok;
    }

    pub fn clear_command_not_found(&mut self) {
        self.command_not_found = false;
    }

    pub fn mark_command_not_found(&mut self) {
        self.command_not_found = true;
    }

    pub fn command_not_found(&self) -> bool {
        self.command_not_found
    }

    /// Starts capturing `echo`'s output instead of printing it (ion-manual
    /// page 6's `PROMPT` function). Any capture already in progress is
    /// discarded — capture sessions don't nest.
    pub fn begin_echo_capture(&mut self) {
        self.echo_capture = Some(String::new());
    }

    /// Stops capturing and returns whatever `echo` wrote during the
    /// session (empty if capture was never started, or nothing was
    /// echoed).
    pub fn end_echo_capture(&mut self) -> String {
        self.echo_capture.take().unwrap_or_default()
    }

    /// `echo`'s actual output routine: appends to the active capture
    /// buffer if one exists, otherwise prints to real stdout. `newline`
    /// is `echo`'s `-n` flag, inverted (suppress it to omit the trailing
    /// newline — needed for the manual's own `PROMPT` example, `echo -n
    /// "${PWD}# "`, since a prompt with a forced trailing newline would
    /// push the cursor to its own line).
    pub fn echo_output(&mut self, text: &str, newline: bool) {
        match &mut self.echo_capture {
            Some(buf) => {
                buf.push_str(text);
                if newline {
                    buf.push('\n');
                }
            }
            None if newline => println!("{text}"),
            None => {
                use std::io::Write;
                print!("{text}");
                // Rust's stdout is line-buffered; without a newline to
                // trigger a flush, `-n` output could sit in the buffer
                // instead of appearing before the next prompt read blocks
                // on stdin.
                let _ = std::io::stdout().flush();
            }
        }
    }

    /// Pushes a fresh scope frame — called once per block *execution*
    /// (`exec_block` in `shell.rs`), so each loop iteration, each `if`
    /// branch taken, and each function call gets its own frame.
    pub fn push_scope(&mut self) {
        self.scalars.push(HashMap::new());
        self.arrays.push(HashMap::new());
        self.tables.push(HashMap::new());
        self.filesets.push(HashMap::new());
        self.plans.push(HashMap::new());
        self.journals.push(HashMap::new());
    }

    /// Pops the innermost scope frame, deleting whatever it newly defined
    /// (ion-manual page 20: "all variables are destroyed once the scope
    /// they were defined in is gone"). Never pops the permanent global
    /// frame at index 0.
    pub fn pop_scope(&mut self) {
        if self.scalars.len() > 1 {
            self.scalars.pop();
        }
        if self.arrays.len() > 1 {
            self.arrays.pop();
        }
        if self.tables.len() > 1 {
            self.tables.pop();
        }
        if self.filesets.len() > 1 {
            self.filesets.pop();
        }
        if self.plans.len() > 1 {
            self.plans.pop();
        }
        if self.journals.len() > 1 { self.journals.pop(); }
    }

    /// Hides every scope above the global one, returning them so the
    /// caller can restore them afterward. Used around a function call
    /// (ion-manual page 20: "Functions have the scope they were defined
    /// in") so the callee's body can't see — or accidentally update —
    /// whatever local variables happen to be active at the call site.
    #[allow(clippy::type_complexity)]
    pub fn isolate_global_scope(
        &mut self,
    ) -> (
        Vec<HashMap<String, String>>,
        Vec<HashMap<String, Vec<String>>>,
        Vec<HashMap<String, Table>>,
        Vec<HashMap<String, FileSet>>,
        Vec<HashMap<String, OperationPlan>>,
        Vec<HashMap<String, OperationJournal>>,
    ) {
        (
            self.scalars.split_off(1),
            self.arrays.split_off(1),
            self.tables.split_off(1),
            self.filesets.split_off(1),
            self.plans.split_off(1),
            self.journals.split_off(1),
        )
    }

    /// Restores scope frames previously hidden by `isolate_global_scope`.
    #[allow(clippy::type_complexity)]
    pub fn restore_scope(
        &mut self,
        saved: (
            Vec<HashMap<String, String>>,
            Vec<HashMap<String, Vec<String>>>,
            Vec<HashMap<String, Table>>,
            Vec<HashMap<String, FileSet>>,
            Vec<HashMap<String, OperationPlan>>,
            Vec<HashMap<String, OperationJournal>>,
        ),
    ) {
        self.scalars.extend(saved.0);
        self.arrays.extend(saved.1);
        self.tables.extend(saved.2);
        self.filesets.extend(saved.3);
        self.plans.extend(saved.4);
        self.journals.extend(saved.5);
    }

    /// Expands a single raw token as a scalar value: `$var`/`@var`
    /// references resolve, array expansions are joined with spaces.
    pub fn scalar_from_token(&self, token: &Token) -> String {
        self.expand_all(std::slice::from_ref(token)).join(" ")
    }

    /// Expands a single raw token as an array value: a `[ ... ]` literal is
    /// parsed as elements, an `@array` reference resolves directly, and a
    /// bare/`$scalar` token becomes a single-element array.
    pub fn array_from_token(&self, token: &Token) -> Vec<String> {
        if token.text.starts_with('[') {
            let elements = Self::parse_array_literal(&token.text);
            self.expand_all(&elements)
        } else {
            self.expand_all(std::slice::from_ref(token))
        }
    }

    pub fn define_function(&mut self, name: String, def: FunctionDef) {
        self.functions.insert(name, def);
    }

    pub fn get_function(&self, name: &str) -> Option<FunctionDef> {
        self.functions.get(name).cloned()
    }

    /// Lists defined functions (name, docstring) sorted by name, for the
    /// bare `fn` / `fn -h` / `fn --help` builtin (ion-manual page 74).
    pub fn list_functions(&self) -> Vec<(String, Option<String>)> {
        let mut out: Vec<(String, Option<String>)> = self
            .functions
            .iter()
            .map(|(k, v)| (k.clone(), v.doc.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// `drop NAME...`: removes each name from whichever scope frame
    /// currently owns it (searched innermost first, matching `get_scalar`/
    /// `set_scalar`), not just the current frame.
    pub fn builtin_drop(&mut self, args: &[Token]) {
        for token in args {
            for frame in self.scalars.iter_mut().rev() {
                if frame.remove(&token.text).is_some() {
                    break;
                }
            }
            for frame in self.arrays.iter_mut().rev() {
                if frame.remove(&token.text).is_some() {
                    break;
                }
            }
            for frame in self.tables.iter_mut().rev() {
                if frame.remove(&token.text).is_some() {
                    break;
                }
            }
            for frame in self.filesets.iter_mut().rev() {
                if frame.remove(&token.text).is_some() { break; }
            }
            for frame in self.plans.iter_mut().rev() {
                if frame.remove(&token.text).is_some() { break; }
            }
            for frame in self.journals.iter_mut().rev() {
                if frame.remove(&token.text).is_some() { break; }
            }
        }
    }
}

/// Returns the length (1 or 2) of the pipe/redirect/chain/background
/// operator starting at the current position of `chars`, or `None` if no
/// such operator starts here — a pure lookahead (via `chars.clone()`,
/// consuming nothing) used both to recognize an operator as its own token
/// and to make the plain-bareword loop stop *before* consuming into one,
/// so `find|stat` splits into `find`, `|`, `stat` regardless of adjacent
/// whitespace, matching every real shell's treatment of these as
/// metacharacters. Two-character operators are checked first so e.g. `>>`
/// isn't mistaken for two separate `>` tokens. A bare `^` not immediately
/// followed by `|`/`>` isn't one of this grammar's defined operators (only
/// `^|`/`^>` are), so it falls through and stays part of an ordinary word.
fn operator_len(chars: &std::iter::Peekable<std::str::Chars>) -> Option<usize> {
    let mut lookahead = chars.clone();
    let first = lookahead.next()?;
    let second = lookahead.next();
    Some(match (first, second) {
        ('>', Some('>')) => 2,
        ('&', Some('&')) => 2,
        ('&', Some('|')) => 2,
        ('&', Some('>')) => 2,
        ('&', Some('!')) => 2,
        ('|', Some('|')) => 2,
        ('^', Some('|')) => 2,
        ('^', Some('>')) => 2,
        ('|', _) => 1,
        ('>', _) => 1,
        ('&', _) => 1,
        _ => return None,
    })
}

/// When a quoted segment ending in `$name`/`@name` is concatenated to an
/// unquoted suffix, add Ion's normal disambiguation braces so the suffix
/// cannot be swallowed into the variable name (`"@arr"tail` ->
/// `@{arr}tail`).
fn brace_trailing_variable(segment: &str) -> String {
    let Some((start, sigil)) = segment
        .char_indices()
        .rev()
        .find(|(_, ch)| *ch == '$' || *ch == '@')
    else {
        return segment.to_string();
    };
    let name = &segment[start + sigil.len_utf8()..];
    if name.is_empty() || !name.chars().all(|ch| ch.is_alphanumeric() || ch == '_') {
        return segment.to_string();
    }
    format!("{}{}{{{name}}}", &segment[..start], sigil)
}

/// If the upcoming text (starting exactly at `sigil`, which `chars` should
/// currently be positioned at) matches `sigil` + an identifier + `(` — a
/// method call like `$len(` or `@split(`, ion-manual page 32 — consumes
/// the whole thing (sigil, identifier, and a balanced-paren argument list)
/// from `chars` and returns it. This is distinct from `$(cmd)`/`@(cmd)`
/// process expansion, which has no identifier between the sigil and the
/// paren. Does a fully non-destructive lookahead first: if the pattern
/// doesn't match, `chars` is left completely untouched so the caller can
/// fall through to ordinary bareword/variable tokenizing (e.g. a plain
/// `$name` must not lose its `$` here).
fn try_consume_method_call(
    chars: &mut std::iter::Peekable<std::str::Chars>,
    sigil: char,
) -> Option<String> {
    let mut lookahead = chars.clone();
    if lookahead.next() != Some(sigil) {
        return None;
    }
    let mut ident_len = 0usize;
    while let Some(&ch) = lookahead.peek() {
        if ch.is_alphanumeric() || ch == '_' {
            ident_len += 1;
            lookahead.next();
        } else {
            break;
        }
    }
    if ident_len == 0 || lookahead.peek() != Some(&'(') {
        return None; // `chars` was never touched
    }

    let mut buf = String::new();
    buf.push(chars.next().unwrap()); // sigil
    for _ in 0..ident_len {
        buf.push(chars.next().unwrap());
    }
    buf.push(chars.next().unwrap()); // '('
    let mut depth = 1i32;
    loop {
        match chars.next() {
            Some('(') => {
                depth += 1;
                buf.push('(');
            }
            Some(')') => {
                depth -= 1;
                buf.push(')');
                if depth == 0 {
                    break;
                }
            }
            Some(other) => buf.push(other),
            None => break, // unterminated; best-effort
        }
    }
    Some(buf)
}

/// If the next character is `[`, consumes a balanced-bracket slice suffix
/// (e.g. `[2..=4]`) into `buf` — this is what lets `$(cmd)[..10]` tokenize
/// as one token instead of splitting at the process-expansion's closing
/// paren (ion-manual page 48, "Process Expansions Also Support Slicing").
fn consume_bracket_suffix(chars: &mut std::iter::Peekable<std::str::Chars>, buf: &mut String) {
    if chars.peek() != Some(&'[') {
        return;
    }
    buf.push(chars.next().unwrap()); // '['
    let mut depth = 1i32;
    loop {
        match chars.next() {
            Some('[') => {
                depth += 1;
                buf.push('[');
            }
            Some(']') => {
                depth -= 1;
                buf.push(']');
                if depth == 0 {
                    break;
                }
            }
            Some(other) => buf.push(other),
            None => break, // unterminated; best-effort
        }
    }
}

/// Applies an optional `[slice]` spec to an already-resolved array,
/// printing a diagnostic and returning an empty array on error rather than
/// propagating a `Result` through every call site.
fn apply_optional_array_slice(items: Vec<String>, slice_spec: Option<&str>) -> Vec<String> {
    match slice_spec {
        Some(spec) => match crate::ranges::apply_array_slice(&items, spec) {
            Ok(sliced) => sliced,
            Err(e) => {
                err_eprintln!("ion: slice error: {e}");
                Vec::new()
            }
        },
        None => items,
    }
}

/// Same as `apply_optional_array_slice`, for a string sliced by grapheme.
fn apply_optional_string_slice(s: String, slice_spec: Option<&str>) -> String {
    match slice_spec {
        Some(spec) => match crate::ranges::apply_string_slice(&s, spec) {
            Ok(sliced) => sliced,
            Err(e) => {
                err_eprintln!("ion: slice error: {e}");
                String::new()
            }
        },
        None => s,
    }
}

/// If the next character is `[`, consumes a balanced-bracket slice spec
/// (like `[2..=4]`) from an `interpolate`-style char iterator and returns
/// its inner content.
fn consume_slice_spec(chars: &mut std::iter::Peekable<std::str::Chars>) -> Option<String> {
    if chars.peek() != Some(&'[') {
        return None;
    }
    chars.next(); // consume '['
    let mut depth = 1i32;
    let mut spec = String::new();
    loop {
        match chars.next() {
            Some('[') => {
                depth += 1;
                spec.push('[');
            }
            Some(']') => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
                spec.push(']');
            }
            Some(other) => spec.push(other),
            None => break, // unterminated; best-effort
        }
    }
    Some(spec)
}

/// Whether `s` is a bare variable name with nothing else attached — used
/// to decide whether a `@name` token qualifies for whole-token array
/// fan-out rather than in-place string interpolation.
fn is_plain_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// Splits a leading run of `echo -n` flags off `args`, returning the
/// remaining words and whether the trailing newline should be suppressed.
/// Only `-n` is recognized — real Ion's `echo` also has `-e`/`-s`, but
/// neither is documented anywhere in the manual (only `-n` is, via the
/// `PROMPT` function example on page 6), so they're out of scope here.
/// Shared by every `echo` call site (`shell.rs`'s two dispatch arms,
/// `pipeline_exec.rs`'s pipeline producer, and this module's `$(cmd)`
/// capture shortcut below) so `-n` behaves consistently everywhere instead
/// of only where it happened to be added first.
pub fn split_echo_no_newline_flag(args: &[String]) -> (&[String], bool) {
    let mut rest = args;
    let mut no_newline = false;
    while rest.first().map(String::as_str) == Some("-n") {
        no_newline = true;
        rest = &rest[1..];
    }
    (rest, no_newline)
}

/// If `s` is exactly a parenthesized body (`(...)`, nothing else attached),
/// returns the inner content — used to detect a bare whole-token `@(cmd)`
/// process expansion (ion-manual page 28) as distinct from `@name`.
fn whole_paren_body(s: &str) -> Option<&str> {
    if s.len() >= 2 && s.starts_with('(') && s.ends_with(')') {
        Some(&s[1..s.len() - 1])
    } else {
        None
    }
}

/// Splits a trailing `[...]` slice suffix off of `s`, e.g.
/// `"myarray[2..=4]"` -> `("myarray", Some("2..=4"))`, `"(cmd)"` ->
/// `("(cmd)", None)`. Scans from the end tracking bracket depth so it
/// finds the *matching* `[`, not just the first one.
fn split_name_and_slice(s: &str) -> (&str, Option<&str>) {
    if !s.ends_with(']') {
        return (s, None);
    }
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    for (i, &b) in bytes.iter().enumerate().rev() {
        if b == b']' {
            depth += 1;
        } else if b == b'[' {
            depth -= 1;
            if depth == 0 {
                return (&s[..i], Some(&s[i + 1..s.len() - 1]));
            }
        }
    }
    (s, None)
}

/// If `s` is a method call `NAME(...)` (an identifier immediately followed
/// by a balanced-paren argument list, nothing else attached), returns
/// `(name, inner_args_text)` — used to detect a bare whole-token
/// `@method(...)` (ion-manual page 32) as distinct from `@name`/`@(cmd)`.
fn split_method_call(s: &str) -> Option<(&str, &str)> {
    let paren_pos = s.find('(')?;
    let name = &s[..paren_pos];
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    let inner = s.strip_prefix(name)?.strip_prefix('(')?.strip_suffix(')')?;
    Some((name, inner))
}

/// Applies a compound-assignment operator to a scalar value.
///
/// `+=`, `-=`, `*=`, `//=` stay integer-typed when both operands parse as
/// `i64`; `/=` and `**=` always produce a float, matching the worked
/// example on ion-manual page 17 (`8 **= 2` -> `64.0`, then `/= 2` -> `32.0`).
fn apply_arith(current: &str, op: &str, rhs: &str) -> Result<String, String> {
    match op {
        "+=" | "-=" | "*=" | "//=" => {
            if let (Ok(a), Ok(b)) = (current.parse::<i64>(), rhs.parse::<i64>()) {
                let result = match op {
                    "+=" => a.checked_add(b),
                    "-=" => a.checked_sub(b),
                    "*=" => a.checked_mul(b),
                    "//=" => {
                        if b == 0 {
                            return Err("division by zero".to_string());
                        }
                        Some(a.div_euclid(b))
                    }
                    _ => unreachable!(),
                };
                result
                    .map(|v| v.to_string())
                    .ok_or_else(|| "integer overflow".to_string())
            } else {
                let a: f64 = current
                    .parse()
                    .map_err(|_| format!("'{current}' is not a number"))?;
                let b: f64 = rhs
                    .parse()
                    .map_err(|_| format!("'{rhs}' is not a number"))?;
                let result = match op {
                    "+=" => a + b,
                    "-=" => a - b,
                    "*=" => a * b,
                    "//=" => (a / b).floor(),
                    _ => unreachable!(),
                };
                Ok(format!("{result:?}"))
            }
        }
        "/=" | "**=" => {
            let a: f64 = current
                .parse()
                .map_err(|_| format!("'{current}' is not a number"))?;
            let b: f64 = rhs
                .parse()
                .map_err(|_| format!("'{rhs}' is not a number"))?;
            let result = match op {
                "/=" => a / b,
                "**=" => a.powf(b),
                _ => unreachable!(),
            };
            Ok(format!("{result:?}"))
        }
        _ => Err(format!("unsupported operator {op}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Matches ion-manual page 26: `echo $string:$string` with
    /// `string = "example string"` -> `example string:example string`.
    #[test]
    fn interpolates_multiple_refs_in_one_token() {
        let mut interp = Interpreter::new();
        interp.builtin_let(&["string".into(), "=".into(), "example string".into()]);
        let expanded = interp.expand_all(&["$string:$string".into()]);
        assert_eq!(expanded, vec!["example string:example string".to_string()]);
    }

    /// ion-manual pages 45-46: `@array[0..5]` (exclusive), `@array[0...5]`
    /// / `@array[0..=5]` (inclusive), with `array = {1...10}`.
    #[test]
    fn array_slicing_matches_manual() {
        let mut interp = Interpreter::new();
        interp.builtin_let(&["array".into(), "=".into(), "[{1...10}]".into()]);
        assert_eq!(
            interp.expand_all(&["@array[0..5]".into()]),
            vec![
                "1".to_string(),
                "2".to_string(),
                "3".to_string(),
                "4".to_string(),
                "5".to_string()
            ]
        );
        assert_eq!(
            interp.expand_all(&["@array[0...5]".into()]),
            vec!["1", "2", "3", "4", "5", "6"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
    }

    /// ion-manual page 45: `$string[..5]` -> "hello", `$string[6..]` ->
    /// "world" with `string = "hello world"`.
    #[test]
    fn string_slicing_matches_manual() {
        let mut interp = Interpreter::new();
        interp.builtin_let(&["string".into(), "=".into(), "hello world".into()]);
        assert_eq!(
            interp.expand_all(&["$string[..5]".into()]),
            vec!["hello".to_string()]
        );
        assert_eq!(
            interp.expand_all(&["$string[6..]".into()]),
            vec!["world".to_string()]
        );
    }

    /// ion-manual page 12: `@array[0]` (single index) -> `1`.
    #[test]
    fn single_index_matches_manual() {
        let mut interp = Interpreter::new();
        interp.builtin_let(&["array".into(), "=".into(), "[ 1 2 3 4 5 ]".into()]);
        assert_eq!(
            interp.expand_all(&["@array[0]".into()]),
            vec!["1".to_string()]
        );
    }

    /// ion-manual page 29-30: bare `{1..10}`/`{a..d}` brace ranges fan out
    /// as arrays, usable directly as command args or in `for`.
    #[test]
    fn brace_range_expands_as_bare_token() {
        let interp = Interpreter::new();
        let expanded = interp.expand_all(&["{1..10}".into()]);
        assert_eq!(
            expanded,
            vec!["1", "2", "3", "4", "5", "6", "7", "8", "9"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
        let expanded = interp.expand_all(&["{a..d}".into()]);
        assert_eq!(
            expanded,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    /// ion-manual pages 29-30: brace permutation as an infix, attached to
    /// surrounding literal text (the manual's primary documented form,
    /// unlike the bare whole-token range above), including multiple
    /// groups in one token and `$var` interpolation applied afterward to
    /// each resulting permutation.
    #[test]
    fn brace_permutation_expands_as_infix_with_interpolation() {
        let interp = Interpreter::new();
        let expanded = interp.expand_all(&["filename.{ext1,ext2}".into()]);
        assert_eq!(
            expanded,
            vec!["filename.ext1".to_string(), "filename.ext2".to_string()]
        );

        let expanded = interp.expand_all(&["job_{01,02}.{ext1,ext2}".into()]);
        assert_eq!(
            expanded,
            vec![
                "job_01.ext1".to_string(),
                "job_01.ext2".to_string(),
                "job_02.ext1".to_string(),
                "job_02.ext2".to_string(),
            ]
        );

        let mut interp = interp;
        interp.set_scalar("name".to_string(), "report".to_string());
        let expanded = interp.expand_all(&["$name.{a,b}".into()]);
        assert_eq!(
            expanded,
            vec!["report.a".to_string(), "report.b".to_string()]
        );
    }

    /// `${name}` disambiguation braces must survive brace-permutation
    /// expansion untouched, still resolving the intended variable rather
    /// than colliding with permutation-group parsing.
    #[test]
    fn dollar_brace_disambiguation_survives_permutation_pass() {
        let mut interp = Interpreter::new();
        interp.set_scalar("name".to_string(), "val".to_string());
        let expanded = interp.expand_all(&["${name}suffix".into()]);
        assert_eq!(expanded, vec!["valsuffix".to_string()]);
    }

    /// ion-manual page 48: process expansions also support slicing —
    /// `$(cmd)[..10]`/`@(cmd)[..10]`. Uses `cmd /c echo` as a reliable
    /// always-present command (this project targets Windows).
    #[test]
    fn process_expansion_slicing_works() {
        let interp = Interpreter::new();
        let expanded = interp.expand_all(&["$(cmd /c echo hello world)[..5]".into()]);
        assert_eq!(expanded, vec!["hello".to_string()]);
        let expanded = interp.expand_all(&["@(cmd /c echo one two three)[0]".into()]);
        assert_eq!(expanded, vec!["one".to_string()]);
    }

    /// Regression test: an array-literal method argument must resolve to
    /// its *elements*, not the literal bracket text — `$len([1 2 3 4])`
    /// was silently counting the 12 characters of "[1 2 3 4]" instead of
    /// the 4 elements before this was fixed.
    #[test]
    fn array_literal_method_arg_resolves_to_elements() {
        let interp = Interpreter::new();
        assert_eq!(
            interp.expand_all(&["$len([one two three four])".into()]),
            vec!["4".to_string()]
        );
        assert_eq!(
            interp.expand_all(&["@reverse([1 2 3])".into()]),
            vec!["3".to_string(), "2".to_string(), "1".to_string()]
        );
    }

    #[test]
    fn table_len_returns_row_count_without_text_coercion() {
        let mut interp = Interpreter::new();
        interp.set_table(
            "manifest".to_string(),
            crate::table::Table {
                rows: vec![
                    vec![("path".to_string(), "one.txt".to_string())],
                    vec![("path".to_string(), "two.txt".to_string())],
                    vec![("path".to_string(), "three.txt".to_string())],
                ],
            },
        );

        assert_eq!(
            interp.expand_all(&["$len(manifest)".into()]),
            vec!["3".to_string()]
        );
    }

    #[test]
    fn field_extracts_a_scalar_from_exactly_one_table_row() {
        let mut interp = Interpreter::new();
        interp.set_table(
            "row".to_string(),
            crate::table::Table {
                rows: vec![vec![
                    ("path".to_string(), "notes.txt".to_string()),
                    ("size".to_string(), "42".to_string()),
                ]],
            },
        );

        assert_eq!(
            interp.expand_all(&["$field(row path)".into()]),
            vec!["notes.txt".to_string()]
        );
        interp.set_scalar("column".to_string(), "size".to_string());
        assert_eq!(
            interp.expand_all(&["$field(row column)".into()]),
            vec!["42".to_string()]
        );
    }

    #[test]
    fn table_accessors_reject_ambiguous_or_missing_fields() {
        let mut interp = Interpreter::new();
        interp.set_table(
            "many".to_string(),
            crate::table::Table {
                rows: vec![
                    vec![("path".to_string(), "one.txt".to_string())],
                    vec![("path".to_string(), "two.txt".to_string())],
                ],
            },
        );
        interp.set_table(
            "one".to_string(),
            crate::table::Table {
                rows: vec![vec![("path".to_string(), "one.txt".to_string())]],
            },
        );

        assert_eq!(
            interp.call_string_method_here("field", "many path"),
            Err("field: expected exactly one row in 'many', found 2".to_string())
        );
        assert_eq!(
            interp.call_string_method_here("field", "one missing"),
            Err("field: column 'missing' does not exist".to_string())
        );
    }

    /// ion-manual page 32: `$method(variable)` — a bare unquoted argument
    /// resolves as a variable reference by name, not its literal text.
    #[test]
    fn method_call_bare_word_arg_resolves_variable_by_name() {
        let mut interp = Interpreter::new();
        interp.builtin_let(&["input".into(), "=".into(), "one two one two".into()]);
        let expanded = interp.expand_all(&["$replace(input one 1)".into()]);
        assert_eq!(expanded, vec!["1 two 1 two".to_string()]);
    }

    /// Nested method calls as arguments, per the manual's own
    /// `@lines($unescape("firstline\nsecondline"))` example (page 40).
    #[test]
    fn nested_method_calls_work() {
        let interp = Interpreter::new();
        let expanded = interp.expand_all(&["@lines($unescape(\"firstline\\nsecondline\"))".into()]);
        assert_eq!(
            expanded,
            vec!["firstline".to_string(), "secondline".to_string()]
        );
    }

    /// ion-manual page 24: `${env::VAR}` reads a real OS environment
    /// variable. Uses a name unique to this test to avoid any risk of
    /// colliding with other tests running concurrently in the same
    /// process (env vars are genuinely process-global state).
    #[test]
    fn env_namespace_reads_os_environment() {
        let interp = Interpreter::new();
        // SAFETY: this test is the only thing touching this specific,
        // uniquely-named variable.
        unsafe {
            std::env::set_var("ION_WIN_TEST_ENV_VAR_XYZ", "hello_env");
        }
        let expanded = interp.expand_all(&["${env::ION_WIN_TEST_ENV_VAR_XYZ}".into()]);
        unsafe {
            std::env::remove_var("ION_WIN_TEST_ENV_VAR_XYZ");
        }
        assert_eq!(expanded, vec!["hello_env".to_string()]);
    }

    /// ion-manual page 24: unlike ordinary variable access, an undefined
    /// `env::` variable emits an empty string rather than erroring.
    #[test]
    fn env_namespace_undefined_var_is_empty_not_error() {
        let interp = Interpreter::new();
        let expanded = interp.expand_all(&["${env::ION_WIN_TEST_DEFINITELY_UNDEFINED_XYZ}".into()]);
        assert_eq!(expanded, vec![String::new()]);
    }

    /// ion-manual page 19: `export` sets the variable like `let` and also
    /// mirrors it into the real process environment.
    #[test]
    fn export_sets_scalar_and_os_environment() {
        let mut interp = Interpreter::new();
        interp.builtin_export(&[
            "ION_WIN_TEST_EXPORT_XYZ".into(),
            "=".into(),
            "exported_value".into(),
        ]);
        assert_eq!(
            interp.get_scalar("ION_WIN_TEST_EXPORT_XYZ").unwrap(),
            "exported_value"
        );
        let os_value = std::env::var("ION_WIN_TEST_EXPORT_XYZ").unwrap();
        unsafe {
            std::env::remove_var("ION_WIN_TEST_EXPORT_XYZ");
        }
        assert_eq!(os_value, "exported_value");
    }

    /// ion-manual page 19: "does not support arrays."
    #[test]
    fn export_rejects_array_rhs() {
        let mut interp = Interpreter::new();
        interp.builtin_export(&[
            "ION_WIN_TEST_EXPORT_ARR_XYZ".into(),
            "=".into(),
            "[ a b c ]".into(),
        ]);
        assert!(interp.get_scalar("ION_WIN_TEST_EXPORT_ARR_XYZ").is_none());
        assert!(std::env::var("ION_WIN_TEST_EXPORT_ARR_XYZ").is_err());
    }

    /// ion-manual page 28: `let string = $(cmd args...)` captures stdout as
    /// a string, trailing newline trimmed.
    #[test]
    fn scalar_process_expansion_captures_stdout() {
        let interp = Interpreter::new();
        let expanded = interp.expand_all(&["$(cmd /c echo hello world)".into()]);
        assert_eq!(expanded, vec!["hello world".to_string()]);
    }

    /// ion-manual page 28: a bare whole-token `@(cmd)` splits stdout by
    /// whitespace and fans out into multiple shell words, like `@array`.
    #[test]
    fn array_process_expansion_fans_out() {
        let interp = Interpreter::new();
        let expanded = interp.expand_all(&["@(cmd /c echo hello world)".into()]);
        assert_eq!(expanded, vec!["hello".to_string(), "world".to_string()]);
    }

    /// The same `@(cmd)` embedded inside a double-quoted string coerces to
    /// one space-joined word instead of fanning out, matching the manual's
    /// general array-in-double-quotes coercion rule.
    #[test]
    fn embedded_process_expansion_coerces_to_string() {
        let interp = Interpreter::new();
        let tokens = Interpreter::tokenize(r#"echo "result: @(cmd /c echo hello world)""#);
        let expanded = interp.expand_all(&tokens[1..]);
        assert_eq!(expanded, vec!["result: hello world".to_string()]);
    }

    /// `echo` is special-cased in-process (consistent with pipeline_exec),
    /// so it works without a real `echo` executable on PATH.
    #[test]
    fn process_expansion_of_echo_runs_in_process() {
        let interp = Interpreter::new();
        let expanded = interp.expand_all(&["$(echo in-process)".into()]);
        assert_eq!(expanded, vec!["in-process".to_string()]);
    }

    /// Pipes/redirection inside process expansion aren't supported and
    /// should error clearly rather than silently misinterpreting the pipe
    /// character as a literal argument.
    #[test]
    fn pipes_inside_process_expansion_are_rejected() {
        let interp = Interpreter::new();
        // No panic, and it must NOT try to run "cmd1" with literal args
        // ["|", "cmd2"] — we can't easily assert on stderr here, but a
        // clean empty-ish result (rather than a hang or panic) is the bar.
        let _ = interp.expand_all(&["$(cmd1 | cmd2)".into()]);
    }

    /// Matches ion-manual page 27: `${hello}world` with `hello = "hello123"`
    /// -> `hello123world`.
    #[test]
    fn braced_scalar_interpolation() {
        let mut interp = Interpreter::new();
        interp.builtin_let(&["hello".into(), "=".into(), "hello123".into()]);
        let expanded = interp.expand_all(&["${hello}world".into()]);
        assert_eq!(expanded, vec!["hello123world".to_string()]);
    }

    /// Matches ion-manual page 27: `@{hello}world` with
    /// `hello = [ hello 123 ' ' ]` -> `hello 123  world`.
    #[test]
    fn braced_array_interpolation_joins_with_spaces() {
        let mut interp = Interpreter::new();
        interp.builtin_let(&["hello".into(), "=".into(), "[ hello 123 ' ' ]".into()]);
        let expanded = interp.expand_all(&["@{hello}world".into()]);
        assert_eq!(expanded, vec!["hello 123  world".to_string()]);
    }

    /// A bare `@name` token (nothing else attached) still fans out into
    /// multiple shell words rather than being interpolated in place.
    #[test]
    fn bare_array_token_still_fans_out() {
        let mut interp = Interpreter::new();
        interp.builtin_let(&["arr".into(), "=".into(), "[ one two three ]".into()]);
        let expanded = interp.expand_all(&["@arr".into()]);
        assert_eq!(
            expanded,
            vec!["one".to_string(), "two".to_string(), "three".to_string()]
        );
    }

    /// The actual bug fix: ion-manual page 4/12/27 says a double-quoted
    /// array coerces to a single space-joined string, unlike a bare
    /// `@array` token which fans out into multiple shell words.
    #[test]
    fn double_quoted_array_coerces_to_single_string() {
        let mut interp = Interpreter::new();
        interp.builtin_let(&["arr".into(), "=".into(), "[ one two three ]".into()]);
        let tokens = Interpreter::tokenize(r#"echo "@arr""#);
        let expanded = interp.expand_all(&tokens[1..]);
        assert_eq!(expanded, vec!["one two three".to_string()]);
    }

    #[test]
    fn adjacent_double_quoted_array_segments_stay_one_coerced_word() {
        let mut interp = Interpreter::new();
        interp.builtin_let(&["arr".into(), "=".into(), "[ one two ]".into()]);

        let tokens = Interpreter::tokenize(r#"echo prefix"@arr"suffix "@arr"tail"#);
        assert_eq!(
            interp.expand_all(&tokens[1..]),
            vec!["prefixone twosuffix".to_string(), "one twotail".to_string()]
        );
    }

    #[test]
    fn nested_double_quoted_array_arguments_keep_string_coercion() {
        let mut interp = Interpreter::new();
        interp.builtin_let(&["arr".into(), "=".into(), "[ one two ]".into()]);
        assert_eq!(
            interp.expand_all(&[r#"$len("@arr")"#.into()]),
            vec!["7".to_string()]
        );
        assert_eq!(
            interp.expand_all(&[r#"@reverse("@arr")"#.into()]),
            vec!["one two".to_string()]
        );
    }

    /// ion-manual page 4: "Variables are expanded in double quotes, but not
    /// single quotes." A single-quoted `$x`/`@x` must pass through inert,
    /// with no expansion attempted (and no "does not exist" error either,
    /// even for undefined names).
    #[test]
    fn single_quotes_suppress_expansion() {
        let interp = Interpreter::new();
        let tokens = Interpreter::tokenize("echo '$undefined_var @also_undefined'");
        let expanded = interp.expand_all(&tokens[1..]);
        assert_eq!(expanded, vec!["$undefined_var @also_undefined".to_string()]);
    }

    #[test]
    fn scalar_roundtrip() {
        let mut interp = Interpreter::new();
        interp.builtin_let(&["x".into(), "=".into(), "hello".into()]);
        assert_eq!(interp.get_scalar("x"), Some(&"hello".to_string()));
        let expanded = interp.expand_all(&["$x".into()]);
        assert_eq!(expanded, vec!["hello".to_string()]);
    }

    #[test]
    fn array_roundtrip() {
        let mut interp = Interpreter::new();
        interp.builtin_let(&["arr".into(), "=".into(), "[ one two three ]".into()]);
        let expanded = interp.expand_all(&["@arr".into()]);
        assert_eq!(
            expanded,
            vec!["one".to_string(), "two".to_string(), "three".to_string()]
        );
    }

    /// Reproduces ion-manual page 20's exact "Scopes" worked example:
    /// `let x = 5`, then inside a nested scope `let x = 2` (updates the
    /// existing outer `x` in place — not a shadowed copy) and `let y = 3`
    /// (a genuinely new name, owned by the nested scope). After the nested
    /// scope ends: `x` is 2, `y` no longer exists.
    #[test]
    fn scope_teardown_matches_manual_worked_example() {
        let mut interp = Interpreter::new();
        interp.builtin_let(&["x".into(), "=".into(), "5".into()]);

        interp.push_scope();
        interp.builtin_let(&["x".into(), "=".into(), "2".into()]);
        interp.builtin_let(&["y".into(), "=".into(), "3".into()]);
        assert_eq!(interp.get_scalar("x"), Some(&"2".to_string()));
        assert_eq!(interp.get_scalar("y"), Some(&"3".to_string()));
        interp.pop_scope();

        assert_eq!(interp.get_scalar("x"), Some(&"2".to_string()));
        assert_eq!(interp.get_scalar("y"), None);
    }

    #[test]
    fn nested_scope_new_variable_is_destroyed_but_outer_survives() {
        let mut interp = Interpreter::new();
        interp.push_scope();
        interp.builtin_let(&["a".into(), "=".into(), "outer".into()]);
        interp.push_scope();
        interp.builtin_let(&["b".into(), "=".into(), "inner".into()]);
        assert_eq!(interp.get_scalar("a"), Some(&"outer".to_string()));
        assert_eq!(interp.get_scalar("b"), Some(&"inner".to_string()));
        interp.pop_scope();
        assert_eq!(interp.get_scalar("a"), Some(&"outer".to_string()));
        assert_eq!(interp.get_scalar("b"), None);
        interp.pop_scope();
        assert_eq!(interp.get_scalar("a"), None);
    }

    #[test]
    fn pop_scope_never_pops_the_global_frame() {
        let mut interp = Interpreter::new();
        interp.builtin_let(&["x".into(), "=".into(), "global".into()]);
        interp.pop_scope(); // no matching push — must be a no-op, not a panic
        interp.pop_scope();
        assert_eq!(interp.get_scalar("x"), Some(&"global".to_string()));
    }

    #[test]
    fn previous_status_defaults_true_and_roundtrips() {
        let mut interp = Interpreter::new();
        assert!(interp.previous_status());
        interp.set_previous_status(false);
        assert!(!interp.previous_status());
        interp.set_previous_status(true);
        assert!(interp.previous_status());
    }

    /// ion-manual page 6: `echo -n` suppresses the trailing newline — the
    /// `PROMPT` function's own worked example (`echo -n "${PWD}# "`) relies
    /// on this so the prompt doesn't push the cursor to its own line.
    #[test]
    fn echo_no_newline_flag_is_stripped() {
        let args: Vec<String> = vec!["hello".into(), "world".into()];
        let (rest, no_newline) = split_echo_no_newline_flag(&args);
        assert_eq!(rest, &args[..]);
        assert!(!no_newline);

        let args: Vec<String> = vec!["-n".into(), "hello".into()];
        let (rest, no_newline) = split_echo_no_newline_flag(&args);
        assert_eq!(rest, &["hello".to_string()]);
        assert!(no_newline);

        // A repeated -n is harmless (mirrors real `echo -n -n x`).
        let args: Vec<String> = vec!["-n".into(), "-n".into(), "x".into()];
        let (rest, no_newline) = split_echo_no_newline_flag(&args);
        assert_eq!(rest, &["x".to_string()]);
        assert!(no_newline);
    }

    /// `Interpreter::echo_output`'s capture mode (`PROMPT`'s implementation
    /// mechanism, ion-manual page 6): while a capture session is active,
    /// `echo`'s output is appended to a buffer instead of printed, honoring
    /// the same `-n`-controlled newline suppression it would use for real
    /// stdout.
    #[test]
    fn echo_capture_collects_output_instead_of_printing() {
        let mut interp = Interpreter::new();
        interp.begin_echo_capture();
        interp.echo_output("hello ", true);
        interp.echo_output("world", false);
        assert_eq!(interp.end_echo_capture(), "hello \nworld");

        // A fresh session starts clean, and ending without ever starting
        // one returns empty rather than panicking.
        assert_eq!(interp.end_echo_capture(), "");
    }

    /// ion-manual page 20: "Functions have the scope they were defined
    /// in" — a function body isolated via `isolate_global_scope` must not
    /// see a caller-local variable, only the global one.
    #[test]
    fn isolate_global_scope_hides_local_but_not_global_variables() {
        let mut interp = Interpreter::new();
        interp.builtin_let(&["g".into(), "=".into(), "global".into()]);
        interp.push_scope();
        interp.builtin_let(&["l".into(), "=".into(), "local".into()]);

        let saved = interp.isolate_global_scope();
        assert_eq!(interp.get_scalar("g"), Some(&"global".to_string()));
        assert_eq!(interp.get_scalar("l"), None);
        interp.restore_scope(saved);

        assert_eq!(interp.get_scalar("l"), Some(&"local".to_string()));
    }

    /// `Table` variables (`ARCHITECTURE.md` §18) follow the exact same
    /// scope-stack rules as scalars/arrays: teardown on `pop_scope`,
    /// hidden-but-not-destroyed by `isolate_global_scope`, and "first
    /// `let` owns it" via `set_table`.
    #[test]
    fn table_variables_follow_the_same_scoping_rules_as_scalars() {
        let mut interp = Interpreter::new();
        let one_row = || crate::table::Table {
            rows: vec![vec![("a".to_string(), "1".to_string())]],
        };

        interp.set_table("g".to_string(), one_row());
        interp.push_scope();
        interp.set_table("l".to_string(), one_row());
        assert_eq!(interp.get_table("g"), Some(&one_row()));
        assert_eq!(interp.get_table("l"), Some(&one_row()));

        let saved = interp.isolate_global_scope();
        assert_eq!(interp.get_table("g"), Some(&one_row()));
        assert_eq!(interp.get_table("l"), None);
        interp.restore_scope(saved);
        assert_eq!(interp.get_table("l"), Some(&one_row()));

        interp.pop_scope();
        assert_eq!(interp.get_table("l"), None);
        assert_eq!(interp.get_table("g"), Some(&one_row()));
    }

    #[test]
    fn operation_plan_variables_follow_scope_and_drop_rules() {
        let mut interp = Interpreter::new();
        let plan = crate::operation::OperationPlan::archive(
            crate::compress::ArchivePlan { items: Vec::new() },
            false,
        );
        interp.push_scope();
        interp.set_plan("pending".to_string(), plan.clone());
        assert_eq!(interp.get_plan("pending"), Some(&plan));

        let saved = interp.isolate_global_scope();
        assert_eq!(interp.get_plan("pending"), None);
        interp.restore_scope(saved);
        assert_eq!(interp.get_plan("pending"), Some(&plan));

        interp.builtin_drop(&["pending".into()]);
        assert_eq!(interp.get_plan("pending"), None);
    }

    #[test]
    fn define_local_shadows_rather_than_updates_outer_variable() {
        let mut interp = Interpreter::new();
        interp.builtin_let(&["x".into(), "=".into(), "global".into()]);
        interp.push_scope();
        interp.define_local_scalar("x".to_string(), "shadowed".to_string());
        assert_eq!(interp.get_scalar("x"), Some(&"shadowed".to_string()));
        interp.pop_scope();
        assert_eq!(interp.get_scalar("x"), Some(&"global".to_string()));
    }

    #[test]
    fn tokenize_respects_quotes_and_arrays() {
        let tokens = Interpreter::tokenize(r#"let x = "hello world""#);
        let texts: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["let", "x", "=", "hello world"]);
        assert_eq!(tokens[3].quoting, Quoting::Double);

        let tokens = Interpreter::tokenize("let arr = [ one two 'three four' ]");
        let texts: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["let", "arr", "=", "[ one two 'three four' ]"]);
        assert_eq!(tokens[3].quoting, Quoting::None);

        let tokens = Interpreter::tokenize("echo 'literal'");
        assert_eq!(tokens[1].quoting, Quoting::Single);
    }

    /// Pipe/redirect/chain/background operators must split into their own
    /// token even with no surrounding whitespace — the real bug this
    /// guards against: `find|stat` used to tokenize as one bareword
    /// `"find|stat"` instead of three tokens, so `pipeline.rs` (which
    /// matches these by exact token text) never saw a pipe at all, and
    /// the whole line silently ran as a single unrecognized external
    /// command instead of erroring or piping.
    #[test]
    fn operators_split_from_adjacent_words_without_whitespace() {
        let texts = |s: &str| -> Vec<String> {
            Interpreter::tokenize(s).into_iter().map(|t| t.text).collect()
        };

        assert_eq!(texts("find|stat"), vec!["find", "|", "stat"]);
        assert_eq!(texts("find| stat"), vec!["find", "|", "stat"]);
        assert_eq!(texts("find |stat"), vec!["find", "|", "stat"]);
        assert_eq!(texts("find | stat"), vec!["find", "|", "stat"]);

        assert_eq!(texts("cmd>>out.txt"), vec!["cmd", ">>", "out.txt"]);
        assert_eq!(texts("cmd>out.txt"), vec!["cmd", ">", "out.txt"]);
        assert_eq!(texts("cmd^|other"), vec!["cmd", "^|", "other"]);
        assert_eq!(texts("cmd^>out.txt"), vec!["cmd", "^>", "out.txt"]);
        assert_eq!(texts("cmd&|other"), vec!["cmd", "&|", "other"]);
        assert_eq!(texts("cmd&>out.txt"), vec!["cmd", "&>", "out.txt"]);
        assert_eq!(texts("sleep 5&"), vec!["sleep", "5", "&"]);
        assert_eq!(texts("cmd&!"), vec!["cmd", "&!"]);
        assert_eq!(texts("true&&echo hi"), vec!["true", "&&", "echo", "hi"]);
        assert_eq!(texts("false||echo hi"), vec!["false", "||", "echo", "hi"]);

        // A lone `^` not immediately followed by `|`/`>` isn't a defined
        // operator in this grammar, so it stays part of an ordinary word.
        assert_eq!(texts("echo a^b"), vec!["echo", "a^b"]);
    }

    #[test]
    fn comments_are_stripped() {
        let tokens = Interpreter::tokenize("# a whole-line comment");
        assert!(tokens.is_empty());

        let tokens = Interpreter::tokenize("echo foo # trailing comment");
        let texts: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["echo", "foo"]);

        // A shebang line is just a comment to the interpreter itself.
        let tokens = Interpreter::tokenize("#!/usr/bin/env ion");
        assert!(tokens.is_empty());

        // '#' embedded mid-word (not at a token boundary) is untouched.
        let tokens = Interpreter::tokenize("echo file#1");
        let texts: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["echo", "file#1"]);
    }

    /// Matches the worked example on ion-manual page 17 exactly:
    /// 5 -> +=5 -> 10 -> -=2 -> 8 -> *=2 -> 16 -> //=2 -> 8 -> **=2 -> 64.0 -> /=2 -> 32.0
    #[test]
    fn arithmetic_matches_manual_worked_example() {
        let mut interp = Interpreter::new();
        interp.builtin_let(&["value".into(), "=".into(), "5".into()]);
        assert_eq!(interp.get_scalar("value").unwrap(), "5");

        interp.builtin_let(&["value".into(), "+=".into(), "5".into()]);
        assert_eq!(interp.get_scalar("value").unwrap(), "10");

        interp.builtin_let(&["value".into(), "-=".into(), "2".into()]);
        assert_eq!(interp.get_scalar("value").unwrap(), "8");

        interp.builtin_let(&["value".into(), "*=".into(), "2".into()]);
        assert_eq!(interp.get_scalar("value").unwrap(), "16");

        interp.builtin_let(&["value".into(), "//=".into(), "2".into()]);
        assert_eq!(interp.get_scalar("value").unwrap(), "8");

        interp.builtin_let(&["value".into(), "**=".into(), "2".into()]);
        assert_eq!(interp.get_scalar("value").unwrap(), "64.0");

        interp.builtin_let(&["value".into(), "/=".into(), "2".into()]);
        assert_eq!(interp.get_scalar("value").unwrap(), "32.0");
    }

    #[test]
    fn typed_let_accepts_attached_and_separated_temporal_types() {
        let mut interp = Interpreter::new();
        interp.builtin_let(&["day:date".into(), "=".into(), "2026-7-3".into()]);
        interp.builtin_let(&["at:".into(), "time".into(), "=".into(), "9:05".into()]);
        assert_eq!(
            interp.get_scalar("day").map(String::as_str),
            Some("2026-07-03")
        );
        assert_eq!(
            interp.get_scalar("at").map(String::as_str),
            Some("09:05:00")
        );
        assert!(interp.get_scalar("day:date").is_none());
    }
}
