//! String and array methods (ion-manual pages 32-43): `$method(args)` /
//! `@method(args)` call syntax, dispatched here after `interp.rs` resolves
//! the argument tokens.
//!
//! Indexing/counting is by Unicode scalar value (`char`), not grapheme
//! cluster — a known simplification consistent with the rest of this
//! scaffold (no graphemes crate dependency yet), so `graphemes` is
//! currently an alias for `chars`.

use regex::Regex;

/// A resolved method argument: either a string or an array, matching
/// ion's own "quoted/`$`-prefixed are strings; `[`/`@`-prefixed are
/// arrays" rule (page 32-33).
#[derive(Clone, Debug)]
pub enum MethodArg {
    Str(String),
    Arr(Vec<String>),
}

impl MethodArg {
    /// Coerces to a string, joining an array with spaces if needed.
    pub fn as_str(&self) -> String {
        match self {
            MethodArg::Str(s) => s.clone(),
            MethodArg::Arr(v) => v.join(" "),
        }
    }

    /// Coerces to an array, wrapping a lone string as a single element.
    pub fn as_array(&self) -> Vec<String> {
        match self {
            MethodArg::Str(s) => vec![s.clone()],
            MethodArg::Arr(v) => v.clone(),
        }
    }
}

fn arg_str(args: &[MethodArg], i: usize) -> Result<String, String> {
    args.get(i)
        .map(MethodArg::as_str)
        .ok_or_else(|| format!("expected at least {} argument(s)", i + 1))
}

fn arg_arr(args: &[MethodArg], i: usize) -> Result<Vec<String>, String> {
    args.get(i)
        .map(MethodArg::as_array)
        .ok_or_else(|| format!("expected at least {} argument(s)", i + 1))
}

fn arg_usize(args: &[MethodArg], i: usize, method: &str) -> Result<usize, String> {
    arg_str(args, i)?
        .parse::<usize>()
        .map_err(|_| format!("{method}: requires a valid number as an argument"))
}

/// Dispatches a string method (`$name(...)`) by name. `None` means `name`
/// isn't a recognized string method.
pub fn call_string_method(name: &str, args: &[MethodArg]) -> Option<Result<String, String>> {
    Some(match name {
        "basename" => basename(args),
        "extension" => extension(args),
        "filename" => filename(args),
        "join" => join(args),
        "find" => find(args),
        "len" => len(args),
        "len_bytes" => len_bytes(args),
        "parent" => parent(args),
        "repeat" => repeat(args),
        "replace" => replace(args),
        "replacen" => replacen(args),
        "regex_replace" => regex_replace(args),
        "reverse" => reverse_str(args),
        "to_lowercase" => to_lowercase(args),
        "to_uppercase" => to_uppercase(args),
        "escape" => escape(args),
        "unescape" => unescape(args),
        "or" => or(args),
        _ => return None,
    })
}

/// Dispatches an array method (`@name(...)`) by name. `None` means `name`
/// isn't a recognized array method.
pub fn call_array_method(name: &str, args: &[MethodArg]) -> Option<Result<Vec<String>, String>> {
    Some(match name {
        "lines" => lines(args),
        "split" => split(args),
        "split_at" => split_at(args),
        "bytes" => bytes(args),
        "chars" => chars_method(args),
        "graphemes" => chars_method(args), // alias; see module doc
        "reverse" => reverse_arr(args),
        "subst" => subst(args),
        _ => return None,
    })
}

// ---------------------------------------------------------------------
// String methods
// ---------------------------------------------------------------------

fn basename(args: &[MethodArg]) -> Result<String, String> {
    let s = arg_str(args, 0)?;
    Ok(s.rsplit('/').next().unwrap_or(&s).to_string())
}

fn extension(args: &[MethodArg]) -> Result<String, String> {
    let s = arg_str(args, 0)?;
    let base = s.rsplit('/').next().unwrap_or(&s);
    Ok(base
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_string())
        .unwrap_or_default())
}

fn filename(args: &[MethodArg]) -> Result<String, String> {
    let s = arg_str(args, 0)?;
    let base = s.rsplit('/').next().unwrap_or(&s);
    Ok(base
        .rsplit_once('.')
        .map(|(name, _)| name.to_string())
        .unwrap_or_else(|| base.to_string()))
}

fn join(args: &[MethodArg]) -> Result<String, String> {
    let arr = arg_arr(args, 0)?;
    let sep = if args.len() > 1 {
        arg_str(args, 1)?
    } else {
        " ".to_string()
    };
    Ok(arr.join(&sep))
}

fn find(args: &[MethodArg]) -> Result<String, String> {
    let haystack = arg_str(args, 0)?;
    let needle = arg_str(args, 1)?;
    match haystack.find(&needle) {
        Some(byte_idx) => Ok(haystack[..byte_idx].chars().count().to_string()),
        None => Ok("-1".to_string()),
    }
}

fn len(args: &[MethodArg]) -> Result<String, String> {
    match args.first() {
        Some(MethodArg::Arr(v)) => Ok(v.len().to_string()),
        Some(MethodArg::Str(s)) => Ok(s.chars().count().to_string()),
        None => Err("len: requires an argument".to_string()),
    }
}

fn len_bytes(args: &[MethodArg]) -> Result<String, String> {
    Ok(arg_str(args, 0)?.len().to_string())
}

fn parent(args: &[MethodArg]) -> Result<String, String> {
    let s = arg_str(args, 0)?;
    Ok(s.rsplit_once('/')
        .map(|(parent, _)| parent.to_string())
        .unwrap_or_default())
}

fn repeat(args: &[MethodArg]) -> Result<String, String> {
    let s = arg_str(args, 0)?;
    let n = arg_usize(args, 1, "repeat")?;
    Ok(s.repeat(n))
}

fn replace(args: &[MethodArg]) -> Result<String, String> {
    let s = arg_str(args, 0)?;
    let pattern = arg_str(args, 1)?;
    let replacement = arg_str(args, 2)?;
    Ok(s.replace(&pattern, &replacement))
}

fn replacen(args: &[MethodArg]) -> Result<String, String> {
    let s = arg_str(args, 0)?;
    let pattern = arg_str(args, 1)?;
    let replacement = arg_str(args, 2)?;
    let n = arg_usize(args, 3, "replacen")?;
    Ok(s.replacen(&pattern, &replacement, n))
}

fn regex_replace(args: &[MethodArg]) -> Result<String, String> {
    let s = arg_str(args, 0)?;
    let pattern = arg_str(args, 1)?;
    let replacement = arg_str(args, 2)?;
    let re = Regex::new(&pattern)
        .map_err(|e| format!("regex_replace: invalid pattern '{pattern}': {e}"))?;
    Ok(re.replace_all(&s, replacement.as_str()).into_owned())
}

fn reverse_str(args: &[MethodArg]) -> Result<String, String> {
    Ok(arg_str(args, 0)?.chars().rev().collect())
}

fn to_lowercase(args: &[MethodArg]) -> Result<String, String> {
    Ok(arg_str(args, 0)?.to_lowercase())
}

fn to_uppercase(args: &[MethodArg]) -> Result<String, String> {
    Ok(arg_str(args, 0)?.to_uppercase())
}

fn escape(args: &[MethodArg]) -> Result<String, String> {
    let s = arg_str(args, 0)?;
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    Ok(out)
}

fn unescape(args: &[MethodArg]) -> Result<String, String> {
    let s = arg_str(args, 0)?;
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    Ok(out)
}

/// Falls back to `args[1]` if `args[0]` is undefined or empty.
///
/// NOTE: if `args[0]` is a `$name` reference to an *undefined* variable,
/// resolving it as a method argument still triggers the ordinary
/// "variable does not exist" diagnostic on stderr (from `interp.rs`'s
/// normal expansion path) before `or` ever sees the resulting empty
/// string. The fallback value on stdout is still correct either way —
/// this is just a redundant stderr warning in that specific case, not a
/// functional bug.
fn or(args: &[MethodArg]) -> Result<String, String> {
    let value = arg_str(args, 0)?;
    let fallback = arg_str(args, 1)?;
    Ok(if value.is_empty() { fallback } else { value })
}

// ---------------------------------------------------------------------
// Array methods
// ---------------------------------------------------------------------

fn lines(args: &[MethodArg]) -> Result<Vec<String>, String> {
    Ok(arg_str(args, 0)?.split('\n').map(str::to_string).collect())
}

fn split(args: &[MethodArg]) -> Result<Vec<String>, String> {
    let s = arg_str(args, 0)?;
    match args.get(1) {
        Some(pat) => Ok(s.split(pat.as_str().as_str()).map(str::to_string).collect()),
        None => Ok(s.split_whitespace().map(str::to_string).collect()),
    }
}

fn split_at(args: &[MethodArg]) -> Result<Vec<String>, String> {
    let s = arg_str(args, 0)?;
    let idx = arg_usize(args, 1, "split_at")?;
    let chars: Vec<char> = s.chars().collect();
    if idx > chars.len() {
        return Err("split_at: value is out of bounds".to_string());
    }
    let (a, b) = chars.split_at(idx);
    Ok(vec![a.iter().collect(), b.iter().collect()])
}

fn bytes(args: &[MethodArg]) -> Result<Vec<String>, String> {
    Ok(arg_str(args, 0)?.bytes().map(|b| b.to_string()).collect())
}

fn chars_method(args: &[MethodArg]) -> Result<Vec<String>, String> {
    Ok(arg_str(args, 0)?.chars().map(|c| c.to_string()).collect())
}

fn reverse_arr(args: &[MethodArg]) -> Result<Vec<String>, String> {
    let mut arr = arg_arr(args, 0)?;
    arr.reverse();
    Ok(arr)
}

fn subst(args: &[MethodArg]) -> Result<Vec<String>, String> {
    if args.len() != 2 {
        return Err("subst: requires 2 arguments".to_string());
    }
    let first = arg_arr(args, 0)?;
    if !first.is_empty() {
        return Ok(first);
    }
    arg_arr(args, 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(text: &str) -> MethodArg {
        MethodArg::Str(text.to_string())
    }
    fn a(items: &[&str]) -> MethodArg {
        MethodArg::Arr(items.iter().map(|i| i.to_string()).collect())
    }

    #[test]
    fn path_methods_match_manual() {
        assert_eq!(
            basename(&[s("/parent/filename.ext")]).unwrap(),
            "filename.ext"
        );
        assert_eq!(extension(&[s("/parent/filename.ext")]).unwrap(), "ext");
        assert_eq!(filename(&[s("/parent/filename.ext")]).unwrap(), "filename");
        assert_eq!(
            parent(&[s("/root/parent/filename.ext")]).unwrap(),
            "/root/parent"
        );
    }

    #[test]
    fn join_matches_manual() {
        let arr = a(&["1", "2", "3", "4", "5"]);
        assert_eq!(join(&[arr.clone()]).unwrap(), "1 2 3 4 5");
        assert_eq!(join(&[arr, s(", ")]).unwrap(), "1, 2, 3, 4, 5");
    }

    #[test]
    fn find_matches_manual() {
        assert_eq!(find(&[s("FOOBAR"), s("OB")]).unwrap(), "2");
        assert_eq!(find(&[s("FOOBAR"), s("ob")]).unwrap(), "-1");
    }

    #[test]
    fn len_and_len_bytes_match_manual() {
        assert_eq!(len(&[s("foobar")]).unwrap(), "6");
        assert_eq!(len(&[a(&["one", "two", "three", "four"])]).unwrap(), "4");
        assert_eq!(len_bytes(&[s("foobar")]).unwrap(), "6");
    }

    #[test]
    fn repeat_matches_manual() {
        assert_eq!(repeat(&[s("abc, "), s("3")]).unwrap(), "abc, abc, abc, ");
    }

    #[test]
    fn replace_and_replacen_match_manual() {
        let input = s("one two one two");
        assert_eq!(
            replace(&[input.clone(), s("one"), s("1")]).unwrap(),
            "1 two 1 two"
        );
        assert_eq!(
            replacen(&[input.clone(), s("one"), s("three"), s("1")]).unwrap(),
            "three two one two"
        );
        assert_eq!(
            replacen(&[input, s("two"), s("three"), s("2")]).unwrap(),
            "one three one three"
        );
    }

    #[test]
    fn regex_replace_matches_manual() {
        assert_eq!(regex_replace(&[s("bob"), s("^b"), s("B")]).unwrap(), "Bob");
        assert_eq!(regex_replace(&[s("bob"), s("b$"), s("B")]).unwrap(), "boB");
    }

    #[test]
    fn reverse_str_matches_manual() {
        assert_eq!(reverse_str(&[s("foobar")]).unwrap(), "raboof");
    }

    #[test]
    fn case_conversion_matches_manual() {
        assert_eq!(to_lowercase(&[s("FOOBAR")]).unwrap(), "foobar");
        assert_eq!(to_uppercase(&[s("foobar")]).unwrap(), "FOOBAR");
    }

    #[test]
    fn escape_matches_manual() {
        let line = " Mary   had\\ta little  \\n\\t lamb\\t";
        assert_eq!(
            escape(&[s(line)]).unwrap(),
            " Mary   had\\\\ta little  \\\\n\\\\t lamb\\\\t"
        );
    }

    #[test]
    fn unescape_round_trips_escape_sequences() {
        assert_eq!(unescape(&[s("a\\tb\\nc")]).unwrap(), "a\tb\nc");
    }

    #[test]
    fn or_matches_manual() {
        assert_eq!(or(&[s(""), s("Fallback")]).unwrap(), "Fallback");
        assert_eq!(or(&[s("42"), s("Not displayed")]).unwrap(), "42");
    }

    #[test]
    fn lines_matches_manual() {
        assert_eq!(
            lines(&[s("firstline\nsecondline")]).unwrap(),
            vec!["firstline", "secondline"]
        );
    }

    #[test]
    fn split_matches_manual() {
        assert_eq!(
            split(&[s("onetwoone"), s("two")]).unwrap(),
            vec!["one", "one"]
        );
        assert_eq!(
            split(&[s("person, age, some data"), s(", ")]).unwrap(),
            vec!["person", "age", "some data"]
        );
        assert_eq!(
            split(&[s("person age data")]).unwrap(),
            vec!["person", "age", "data"]
        );
    }

    #[test]
    fn split_at_matches_manual() {
        assert_eq!(
            split_at(&[s("onetwoone"), s("3")]).unwrap(),
            vec!["one", "twoone"]
        );
        assert_eq!(
            split_at(&[s("FOOBAR"), s("3")]).unwrap(),
            vec!["FOO", "BAR"]
        );
    }

    #[test]
    fn bytes_matches_manual() {
        assert_eq!(
            bytes(&[s("onetwo")]).unwrap(),
            vec!["111", "110", "101", "116", "119", "111"]
        );
        assert_eq!(bytes(&[s("abc")]).unwrap(), vec!["97", "98", "99"]);
    }

    #[test]
    fn chars_matches_manual() {
        assert_eq!(
            chars_method(&[s("onetwo")]).unwrap(),
            vec!["o", "n", "e", "t", "w", "o"]
        );
    }

    #[test]
    fn reverse_arr_matches_manual() {
        assert_eq!(
            reverse_arr(&[a(&["1", "2", "3"])]).unwrap(),
            vec!["3", "2", "1"]
        );
    }

    #[test]
    fn subst_matches_manual() {
        assert_eq!(
            subst(&[a(&[]), a(&["1", "2", "3"])]).unwrap(),
            vec!["1", "2", "3"]
        );
        assert_eq!(subst(&[a(&["x"]), a(&["1", "2", "3"])]).unwrap(), vec!["x"]);
    }

    #[test]
    fn dispatch_returns_none_for_unknown_names() {
        assert!(call_string_method("no_such_method", &[]).is_none());
        assert!(call_array_method("no_such_method", &[]).is_none());
    }
}
