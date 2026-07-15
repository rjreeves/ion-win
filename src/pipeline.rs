//! Pipeline and redirection parsing (ion-manual pages 58-59): `|`, `^|`,
//! `&|` connect commands' stdout/stderr/both to the next command's stdin;
//! `>`, `>>`, `^>`, `&>` redirect a single command's stdout/stderr/both to
//! a file; trailing `&`/`&!` background/disown the whole pipeline.
//!
//! This module only parses an already-tokenized line — see shell.rs for
//! execution. No tokenizer changes were needed: as long as a script writes
//! normal spacing (`cmd | cmd`, not `cmd|cmd`), every operator here is
//! already its own token under `Interpreter::tokenize`, since none of them
//! contain whitespace, quotes, brackets, or `$`.

use crate::interp::Token;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PipeKind {
    Stdout,   // |
    Stderr,   // ^|
    Combined, // &|
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stream {
    Stdout,   // >, >>
    Stderr,   // ^>
    Combined, // &>
}

#[derive(Clone, Debug)]
pub struct Redirect {
    pub stream: Stream,
    pub path: String,
    pub append: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Stage {
    pub tokens: Vec<Token>,
    pub redirects: Vec<Redirect>,
}

#[derive(Debug, Default)]
pub struct Pipeline {
    pub stages: Vec<Stage>,
    pub pipes: Vec<PipeKind>,
    pub background: bool,
    pub disown: bool,
}

impl Pipeline {
    /// Whether this is just a single plain command: no pipes, redirects, or
    /// background/disown markers. Callers should fall back to ordinary
    /// statement dispatch in this case rather than invoking the pipeline
    /// execution engine.
    pub fn is_trivial(&self) -> bool {
        self.stages.len() <= 1
            && self.pipes.is_empty()
            && !self.background
            && !self.disown
            && self
                .stages
                .first()
                .map(|s| s.redirects.is_empty())
                .unwrap_or(true)
    }
}

/// Parses a fully-tokenized line into a `Pipeline`.
pub fn parse(tokens: &[Token]) -> Pipeline {
    let mut pipeline = Pipeline::default();
    let mut current = Stage::default();
    let mut i = 0;

    while i < tokens.len() {
        let text = tokens[i].text.as_str();
        match text {
            "|" | "^|" | "&|" => {
                pipeline.stages.push(std::mem::take(&mut current));
                pipeline.pipes.push(match text {
                    "|" => PipeKind::Stdout,
                    "^|" => PipeKind::Stderr,
                    _ => PipeKind::Combined,
                });
            }
            ">" | ">>" | "^>" | "&>" => {
                let append = text == ">>";
                let stream = match text {
                    "^>" => Stream::Stderr,
                    "&>" => Stream::Combined,
                    _ => Stream::Stdout,
                };
                i += 1;
                if let Some(target) = tokens.get(i) {
                    current.redirects.push(Redirect {
                        stream,
                        path: target.text.clone(),
                        append,
                    });
                }
            }
            "&" if i == tokens.len() - 1 => pipeline.background = true,
            "&!" if i == tokens.len() - 1 => pipeline.disown = true,
            _ => current.tokens.push(tokens[i].clone()),
        }
        i += 1;
    }

    pipeline.stages.push(current);
    pipeline
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interp::Interpreter;

    fn parse_line(line: &str) -> Pipeline {
        parse(&Interpreter::tokenize(line))
    }

    #[test]
    fn trivial_single_command() {
        let p = parse_line("echo hello world");
        assert!(p.is_trivial());
        assert_eq!(p.stages.len(), 1);
        assert_eq!(p.stages[0].tokens.len(), 3);
    }

    #[test]
    fn simple_pipe() {
        let p = parse_line("command | command");
        assert!(!p.is_trivial());
        assert_eq!(p.stages.len(), 2);
        assert_eq!(p.pipes, vec![PipeKind::Stdout]);
    }

    #[test]
    fn stderr_and_combined_pipes() {
        assert_eq!(
            parse_line("command ^| command").pipes,
            vec![PipeKind::Stderr]
        );
        assert_eq!(
            parse_line("command &| command").pipes,
            vec![PipeKind::Combined]
        );
    }

    #[test]
    fn redirection_forms() {
        let p = parse_line("command > stdout");
        assert_eq!(p.stages[0].redirects.len(), 1);
        assert_eq!(p.stages[0].redirects[0].stream, Stream::Stdout);
        assert_eq!(p.stages[0].redirects[0].path, "stdout");
        assert!(!p.stages[0].redirects[0].append);

        assert!(parse_line("command >> stdout").stages[0].redirects[0].append);
        assert_eq!(
            parse_line("command ^> stderr").stages[0].redirects[0].stream,
            Stream::Stderr
        );
        assert_eq!(
            parse_line("command &> combined").stages[0].redirects[0].stream,
            Stream::Combined
        );
    }

    #[test]
    fn multiple_redirection() {
        let p = parse_line("command > stdout ^> stderr &> combined");
        assert!(!p.is_trivial());
        assert_eq!(p.stages[0].redirects.len(), 3);
        // The command itself shouldn't have swallowed any redirect tokens.
        assert_eq!(p.stages[0].tokens.len(), 1);
    }

    #[test]
    fn combined_pipe_and_redirect() {
        let p = parse_line("command | command > stdout");
        assert_eq!(p.stages.len(), 2);
        assert!(p.stages[0].redirects.is_empty());
        assert_eq!(p.stages[1].redirects.len(), 1);
    }

    #[test]
    fn background_and_disown() {
        assert!(parse_line("command &").background);
        assert!(parse_line("command &!").disown);
        // Not trivial even though it's a single command with no pipes.
        assert!(!parse_line("command &").is_trivial());
    }

    #[test]
    fn concatenating_redirect_example_from_manual() {
        let p1 = parse_line("command > stdout");
        let p2 = parse_line("command >> stdout");
        assert!(!p1.stages[0].redirects[0].append);
        assert!(p2.stages[0].redirects[0].append);
    }
}
