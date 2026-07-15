//! Single source of truth for interactive-shell "words" that used to be
//! duplicated across three independently hand-maintained lists: the
//! `help` builtin's summary line, Tab-completion's command list, and the
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
    /// How this entry appears in `help`'s summary line. `None` means it's
    /// folded into a sibling's display instead of getting its own (e.g.
    /// `else`/`in` are covered by `if`/`for`'s "if/else if/else"/"for/in").
    pub help_display: Option<&'static str>,
}

pub const BUILTINS: &[Builtin] = &[
    Builtin { name: "exit", is_keyword: false, help_display: Some("exit") },
    Builtin { name: "quit", is_keyword: false, help_display: None },
    Builtin { name: "help", is_keyword: false, help_display: Some("help") },
    Builtin { name: "let", is_keyword: true, help_display: Some("let") },
    Builtin { name: "export", is_keyword: true, help_display: Some("export") },
    Builtin { name: "drop", is_keyword: true, help_display: Some("drop") },
    Builtin { name: "read", is_keyword: false, help_display: Some("read") },
    Builtin { name: "echo", is_keyword: false, help_display: Some("echo") },
    Builtin { name: "cd", is_keyword: false, help_display: Some("cd") },
    Builtin { name: "pwd", is_keyword: false, help_display: Some("pwd") },
    Builtin { name: "dirs", is_keyword: false, help_display: Some("dirs") },
    Builtin { name: "folders", is_keyword: false, help_display: Some("folders") },
    Builtin { name: "files", is_keyword: false, help_display: Some("files") },
    Builtin { name: "if", is_keyword: true, help_display: Some("if/else if/else") },
    Builtin { name: "else", is_keyword: true, help_display: None },
    Builtin { name: "while", is_keyword: true, help_display: Some("while") },
    Builtin { name: "for", is_keyword: true, help_display: Some("for/in") },
    Builtin { name: "in", is_keyword: true, help_display: None },
    Builtin { name: "fn", is_keyword: true, help_display: Some("fn") },
    Builtin { name: "match", is_keyword: true, help_display: Some("match/case") },
    Builtin { name: "case", is_keyword: true, help_display: None },
    Builtin { name: "end", is_keyword: true, help_display: None },
    Builtin { name: "break", is_keyword: true, help_display: Some("break") },
    Builtin { name: "continue", is_keyword: true, help_display: Some("continue") },
    Builtin { name: "test", is_keyword: false, help_display: Some("test") },
    Builtin { name: "matches", is_keyword: false, help_display: Some("matches") },
    Builtin { name: "not", is_keyword: true, help_display: Some("not") },
    Builtin { name: "true", is_keyword: false, help_display: Some("true") },
    Builtin { name: "false", is_keyword: false, help_display: Some("false") },
    Builtin { name: "bool", is_keyword: false, help_display: Some("bool") },
    Builtin { name: "contains", is_keyword: false, help_display: Some("contains") },
    Builtin { name: "starts-with", is_keyword: false, help_display: Some("starts-with") },
    Builtin { name: "ends-with", is_keyword: false, help_display: Some("ends-with") },
    Builtin { name: "eq", is_keyword: false, help_display: Some("eq/is") },
    Builtin { name: "is", is_keyword: false, help_display: None },
    Builtin { name: "exists", is_keyword: false, help_display: Some("exists") },
    Builtin { name: "intersects", is_keyword: false, help_display: Some("intersects") },
    Builtin { name: "isatty", is_keyword: false, help_display: Some("isatty") },
    Builtin { name: "and", is_keyword: true, help_display: Some("and") },
    Builtin { name: "or", is_keyword: true, help_display: Some("or") },
    Builtin { name: "which", is_keyword: false, help_display: Some("which/type") },
    Builtin { name: "type", is_keyword: false, help_display: None },
    Builtin { name: "eval", is_keyword: false, help_display: Some("eval") },
    Builtin { name: "pvar", is_keyword: false, help_display: Some("pvar set|get|list|delete") },
    Builtin { name: "dmark", is_keyword: false, help_display: Some("dmark add|list|jump") },
    Builtin { name: "jobs", is_keyword: false, help_display: Some("jobs") },
    Builtin { name: "wait", is_keyword: false, help_display: Some("wait") },
    Builtin { name: "disown", is_keyword: false, help_display: Some("disown [-a|PID...]") },
    Builtin { name: "source", is_keyword: false, help_display: Some("source") },
    Builtin { name: "highlight", is_keyword: false, help_display: Some("highlight on|off") },
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

/// The `help` builtin's full printed text: every builtin's display form,
/// plus a few non-builtin shell-syntax notes (pipes/redirects/implicit
/// cd/namespaces) that aren't builtins themselves but belong alongside them.
pub fn help_text() -> String {
    let names: Vec<&str> = BUILTINS.iter().filter_map(|b| b.help_display).collect();
    format!(
        "builtins: {}\n\
         pipes: | ^| &|   redirect: > >> ^> &>   background: & &!\n\
         implicit cd: bare ~/path, .., .config, examples/\n\
         namespaces: ${{env::VAR}}",
        names.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_text_matches_current_builtin_list() {
        assert_eq!(
            help_text(),
            "builtins: exit, help, let, export, drop, read, echo, cd, pwd, dirs, folders, files, if/else if/else, while, for/in, fn, match/case, break, continue, test, matches, not, true, false, bool, contains, starts-with, ends-with, eq/is, exists, intersects, isatty, and, or, which/type, eval, pvar set|get|list|delete, dmark add|list|jump, jobs, wait, disown [-a|PID...], source, highlight on|off\n\
             pipes: | ^| &|   redirect: > >> ^> &>   background: & &!\n\
             implicit cd: bare ~/path, .., .config, examples/\n\
             namespaces: ${env::VAR}"
        );
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
    }
}
