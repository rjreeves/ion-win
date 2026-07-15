//! Range/slice parsing (ion-manual pages 45-48) and brace expansion
//! (ion-manual pages 29-30): numeric/alpha *ranges* (`{1..10}`, `{a..d}`)
//! and general *permutation* lists (`{ext1,ext2}`), including multiple
//! brace groups per word (`job_{01,02}.{ext1,ext2}`) and nesting
//! (`job_{01_{out,err},02_{out,err}}.txt`). Ranges and permutations may be
//! mixed freely within one comma list, since each comma-separated element
//! is tried as a range first and falls back to a literal (or a nested
//! permutation) otherwise — matching upstream Ion's `expand_brace`
//! (`Linux/ion-master/src/lib/expansion/mod.rs`).
//!
//! Indexing/slicing here operates on Unicode scalar values (`char`), not
//! grapheme clusters — a known simplification consistent with the rest of
//! this scaffold not depending on a graphemes crate yet. Array indices
//! don't support negative absolute positions (the manual doesn't document
//! that either); only a negative *step* is supported, for reverse slicing.

/// A parsed `[start..end]`-style slice specifier.
#[derive(Debug, Clone, Copy)]
struct Slice {
    start: Option<i64>,
    step: Option<i64>,
    end: Option<i64>,
    inclusive: bool,
}

enum IndexSpec {
    Index(i64),
    Range(Slice),
}

/// Applies a `[...]` spec to an array, returning either a single-element
/// selection (bare index) or a slice.
pub fn apply_array_slice(items: &[String], spec: &str) -> Result<Vec<String>, String> {
    match parse_index_spec(spec).ok_or_else(|| format!("invalid index/range '{spec}'"))? {
        IndexSpec::Index(i) => {
            let idx = normalize_index(i, items.len())
                .ok_or_else(|| format!("index {i} out of bounds"))?;
            Ok(vec![items[idx].clone()])
        }
        IndexSpec::Range(slice) => {
            let indices = slice_indices(items.len(), &slice);
            Ok(indices.into_iter().map(|i| items[i].clone()).collect())
        }
    }
}

/// Applies a `[...]` spec to a string, indexed/sliced by `char`.
pub fn apply_string_slice(s: &str, spec: &str) -> Result<String, String> {
    let chars: Vec<char> = s.chars().collect();
    match parse_index_spec(spec).ok_or_else(|| format!("invalid index/range '{spec}'"))? {
        IndexSpec::Index(i) => {
            let idx = normalize_index(i, chars.len())
                .ok_or_else(|| format!("index {i} out of bounds"))?;
            Ok(chars[idx].to_string())
        }
        IndexSpec::Range(slice) => {
            let indices = slice_indices(chars.len(), &slice);
            Ok(indices.into_iter().map(|i| chars[i]).collect())
        }
    }
}

fn normalize_index(i: i64, len: usize) -> Option<usize> {
    if i < 0 || i as usize >= len {
        return None;
    }
    Some(i as usize)
}

/// Parses `spec` as either a bare integer index or a range (`start..end`,
/// `start...end`, `start..=end`, optionally stepped via `start,step..end`).
fn parse_index_spec(spec: &str) -> Option<IndexSpec> {
    if let Some(slice) = parse_slice(spec) {
        return Some(IndexSpec::Range(slice));
    }
    spec.trim().parse::<i64>().ok().map(IndexSpec::Index)
}

fn parse_slice(spec: &str) -> Option<Slice> {
    let first = spec.find("..")?;
    let left = &spec[..first];
    let rest = &spec[first + 2..];

    let (step_str, inclusive, end_str): (Option<&str>, bool, &str) =
        if let Some(end) = rest.strip_prefix('.') {
            (None, true, end) // "..."
        } else if let Some(end) = rest.strip_prefix('=') {
            (None, true, end) // "..="
        } else if let Some(second) = rest.find("..") {
            // Stepped: "STEP..END" or "STEP...END"
            let step = &rest[..second];
            let after = &rest[second + 2..];
            if let Some(end) = after.strip_prefix('.') {
                (Some(step), true, end)
            } else if let Some(end) = after.strip_prefix('=') {
                (Some(step), true, end)
            } else {
                (Some(step), false, after)
            }
        } else {
            (None, false, rest) // plain "start..end"
        };

    let (start_str, comma_step) = match left.split_once(',') {
        Some((s, st)) => (s, Some(st)),
        None => (left, None),
    };

    let start = if start_str.is_empty() {
        None
    } else {
        start_str.parse::<i64>().ok()
    };
    let end = if end_str.is_empty() {
        None
    } else {
        end_str.parse::<i64>().ok()
    };
    let step = step_str
        .filter(|s| !s.is_empty())
        .or(comma_step.filter(|s| !s.is_empty()))
        .and_then(|s| s.parse::<i64>().ok());

    Some(Slice {
        start,
        step,
        end,
        inclusive,
    })
}

/// Resolves a `Slice` against a sequence of length `len` into the concrete
/// list of indices it selects, ascending for a positive/absent step or
/// descending for a negative one. Per the manual: descending ranges aren't
/// valid for plain array indices (no positive-step reverse), but a
/// negative step is exactly how reverse slicing is expressed, and the end
/// index defaults to 0 when omitted in that case.
fn slice_indices(len: usize, slice: &Slice) -> Vec<usize> {
    if len == 0 {
        return Vec::new();
    }
    let step = slice.step.unwrap_or(1);
    if step == 0 {
        return Vec::new();
    }

    let mut out = Vec::new();
    if step > 0 {
        let start = slice.start.unwrap_or(0).max(0);
        let end_inclusive: i64 = match slice.end {
            Some(e) if slice.inclusive => e,
            Some(e) => e - 1,
            None => len as i64 - 1,
        };
        let mut i = start;
        while i <= end_inclusive && (i as usize) < len {
            out.push(i as usize);
            i += step;
        }
    } else {
        let start = slice.start.unwrap_or(len as i64 - 1);
        let end: i64 = match slice.end {
            Some(e) if slice.inclusive => e,
            Some(e) => e + 1,
            None => 0,
        };
        let mut i = start;
        while i >= end && i >= 0 && (i as usize) < len {
            out.push(i as usize);
            i += step; // step is negative
        }
    }
    out
}

/// Expands a brace range's inner content (without the braces), e.g.
/// `"1..10"`, `"10...1"`, `"a..d"`, `"0..3...12"`, `"10..-2...-10"`.
/// Returns `None` for anything that isn't a numeric or single-letter
/// start/end range — comma-separated permutation lists like `"ext1,ext2"`
/// are handled one layer up, by `expand_braces`.
pub fn expand_brace_range(inner: &str) -> Option<Vec<String>> {
    expand_numeric_brace_range(inner).or_else(|| expand_alpha_brace_range(inner))
}

fn expand_numeric_brace_range(inner: &str) -> Option<Vec<String>> {
    let slice = parse_slice(inner)?;
    let (Some(start), Some(end)) = (slice.start, slice.end) else {
        return None;
    };
    let step_magnitude = slice.step.unwrap_or(1).unsigned_abs().max(1) as i64;
    Some(
        numeric_sequence(start, end, step_magnitude, slice.inclusive)
            .iter()
            .map(i64::to_string)
            .collect(),
    )
}

/// Expands an alphabetic brace range like `"a..d"` -> `["a","b","c"]`.
/// Kept separate from the numeric path because `parse_slice` only parses
/// integer bounds; single-letter bounds need their own detection.
fn expand_alpha_brace_range(inner: &str) -> Option<Vec<String>> {
    let first = inner.find("..")?;
    let left = &inner[..first];
    let after_dots = &inner[first + 2..];
    let end_str = after_dots
        .strip_prefix('.')
        .or_else(|| after_dots.strip_prefix('='))
        .unwrap_or(after_dots);
    let inclusive = end_str.len() != after_dots.len();

    let mut left_chars = left.chars();
    let mut right_chars = end_str.chars();
    let (Some(start), None, Some(end), None) = (
        left_chars.next(),
        left_chars.next(),
        right_chars.next(),
        right_chars.next(),
    ) else {
        return None;
    };
    if !start.is_ascii_alphabetic() || !end.is_ascii_alphabetic() {
        return None;
    }

    let seq = numeric_sequence(start as i64, end as i64, 1, inclusive);
    Some(seq.iter().map(|&n| (n as u8 as char).to_string()).collect())
}

/// A word broken into alternating literal text and brace-group content, as
/// found by `parse_top_level_segments`.
enum BraceSegment {
    Literal(String),
    Group(String),
}

/// Scans `word` for top-level (non-nested) `{...}` groups, returning the
/// alternating literal/group segments. Returns `None` if `word` has no
/// top-level group at all, or an unmatched brace — callers then fall back
/// to treating `word` as plain text.
///
/// A `{` immediately preceded by `$` or `@` is left untouched as literal
/// text instead of opened as a group: that's `${name}`/`@{name}`
/// variable-interpolation syntax (handled by `Interpreter::interpolate`),
/// not brace-permutation syntax, and the two would otherwise collide —
/// `${name}suffix` must stay `${name}` + `suffix`, not collapse into a
/// single-element "group" that drops the disambiguating braces and merges
/// the name with its suffix.
fn parse_top_level_segments(word: &str) -> Option<Vec<BraceSegment>> {
    let mut segments = Vec::new();
    let mut literal = String::new();
    let mut found_group = false;
    let mut chars = word.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '{' {
            if matches!(literal.chars().last(), Some('$') | Some('@')) {
                literal.push('{');
                for c2 in chars.by_ref() {
                    literal.push(c2);
                    if c2 == '}' {
                        break;
                    }
                }
                continue;
            }

            let mut depth = 1i32;
            let mut content = String::new();
            let mut closed = false;
            for c2 in chars.by_ref() {
                match c2 {
                    '{' => {
                        depth += 1;
                        content.push(c2);
                    }
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            closed = true;
                            break;
                        }
                        content.push(c2);
                    }
                    _ => content.push(c2),
                }
            }
            if !closed {
                return None; // unmatched opening brace
            }
            if content.is_empty() {
                // `{}` alone isn't a valid permutation — leave it literal
                // rather than silently vanishing.
                literal.push('{');
                literal.push('}');
                continue;
            }
            if !literal.is_empty() {
                segments.push(BraceSegment::Literal(std::mem::take(&mut literal)));
            }
            segments.push(BraceSegment::Group(content));
            found_group = true;
        } else if c == '}' {
            return None; // unmatched closing brace
        } else {
            literal.push(c);
        }
    }

    if !literal.is_empty() {
        segments.push(BraceSegment::Literal(literal));
    }
    found_group.then_some(segments)
}

/// Splits a brace group's inner content on top-level commas (depth-aware
/// over nested `{`/`}`, so `"A{1,2},B{1,2}"` splits into `["A{1,2}",
/// "B{1,2}"]`, not four pieces).
fn split_top_level_commas(content: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (i, c) in content.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&content[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&content[start..]);
    parts
}

/// Expands every top-level brace group in `word` (permutation lists,
/// ranges, or nested combinations of both) into the full cross product of
/// resulting words, e.g. `"job_{01,02}.{ext1,ext2}"` ->
/// `["job_01.ext1", "job_01.ext2", "job_02.ext1", "job_02.ext2"]`
/// (ion-manual pages 29-30). Each comma-separated element within a group
/// is tried as a range first (`expand_brace_range`), then recursively as
/// its own nested brace expansion, falling back to a literal — matching
/// upstream Ion's `expand_brace`
/// (`Linux/ion-master/src/lib/expansion/mod.rs`). Returns `None` when
/// `word` has no top-level brace group to expand, so callers can fall back
/// to treating `word` as ordinary text.
pub fn expand_braces(word: &str) -> Option<Vec<String>> {
    let segments = parse_top_level_segments(word)?;
    let mut combos = vec![String::new()];

    for segment in segments {
        match segment {
            BraceSegment::Literal(text) => {
                for combo in combos.iter_mut() {
                    combo.push_str(&text);
                }
            }
            BraceSegment::Group(content) => {
                let mut options = Vec::new();
                for element in split_top_level_commas(&content) {
                    if let Some(range_items) = expand_brace_range(element) {
                        options.extend(range_items);
                    } else if let Some(nested) = expand_braces(element) {
                        options.extend(nested);
                    } else {
                        options.push(element.to_string());
                    }
                }
                let mut expanded = Vec::with_capacity(combos.len() * options.len().max(1));
                for combo in &combos {
                    for opt in &options {
                        expanded.push(format!("{combo}{opt}"));
                    }
                }
                combos = expanded;
            }
        }
    }

    Some(combos)
}

/// Generates the inclusive/exclusive sequence from `start` to `end`
/// stepping by `step` (always given as a positive magnitude; direction is
/// inferred from comparing `start`/`end`, matching the manual's own
/// examples where `{10..-2...-10}`'s "-2" is a magnitude-2 descending step,
/// not a literal negative delta).
fn numeric_sequence(start: i64, end: i64, step: i64, inclusive: bool) -> Vec<i64> {
    let step = step.max(1);
    let mut out = Vec::new();
    if start <= end {
        let mut i = start;
        while (inclusive && i <= end) || (!inclusive && i < end) {
            out.push(i);
            i += step;
        }
    } else {
        let mut i = start;
        while (inclusive && i >= end) || (!inclusive && i > end) {
            out.push(i);
            i -= step;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arr(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn exclusive_and_inclusive_array_slice_match_manual() {
        let array = arr(&["1", "2", "3", "4", "5", "6", "7", "8", "9", "10"]);
        assert_eq!(
            apply_array_slice(&array, "0..5").unwrap(),
            arr(&["1", "2", "3", "4", "5"])
        );
        assert_eq!(
            apply_array_slice(&array, "..5").unwrap(),
            arr(&["1", "2", "3", "4", "5"])
        );
        assert_eq!(
            apply_array_slice(&array, "0...5").unwrap(),
            arr(&["1", "2", "3", "4", "5", "6"])
        );
        assert_eq!(
            apply_array_slice(&array, "0..=5").unwrap(),
            arr(&["1", "2", "3", "4", "5", "6"])
        );
    }

    /// Matches ion-manual page 45 (`$string = "hello world"`) and page 10
    /// (`$foo = "Hello, World"`, `$foo[2..9]` -> `"llo, Wo"`).
    #[test]
    fn string_slice_matches_manual() {
        assert_eq!(apply_string_slice("hello world", "..5").unwrap(), "hello");
        assert_eq!(apply_string_slice("hello world", "6..").unwrap(), "world");
        assert_eq!(
            apply_string_slice("Hello, World", "2..9").unwrap(),
            "llo, Wo"
        );
    }

    #[test]
    fn single_index_matches_manual() {
        let array = arr(&["1", "2", "3", "4", "5"]);
        assert_eq!(apply_array_slice(&array, "0").unwrap(), arr(&["1"]));
        assert_eq!(
            apply_array_slice(&array, "2..=4").unwrap(),
            arr(&["3", "4", "5"])
        );
    }

    #[test]
    fn stepped_and_reverse_array_slice_match_manual() {
        let array: Vec<String> = (0..=30).map(|n: i64| n.to_string()).collect();
        assert_eq!(
            apply_array_slice(&array, "0,3..").unwrap(),
            arr(&["0", "3", "6", "9", "12", "15", "18", "21", "24", "27", "30"])
        );
        assert_eq!(
            apply_array_slice(&array, "30,-3..").unwrap(),
            arr(&["30", "27", "24", "21", "18", "15", "12", "9", "6", "3", "0"])
        );
    }

    #[test]
    fn brace_ranges_match_manual_ascending_descending() {
        assert_eq!(
            expand_brace_range("1..10").unwrap(),
            vec!["1", "2", "3", "4", "5", "6", "7", "8", "9"]
        );
        assert_eq!(
            expand_brace_range("10..1").unwrap(),
            vec!["10", "9", "8", "7", "6", "5", "4", "3", "2"]
        );
        assert_eq!(
            expand_brace_range("1...10").unwrap(),
            vec!["1", "2", "3", "4", "5", "6", "7", "8", "9", "10"]
        );
        assert_eq!(
            expand_brace_range("10...1").unwrap(),
            vec!["10", "9", "8", "7", "6", "5", "4", "3", "2", "1"]
        );
    }

    #[test]
    fn brace_ranges_support_negative_bounds() {
        let expected: Vec<String> = (-10..=10).map(|n: i64| n.to_string()).collect();
        assert_eq!(expand_brace_range("-10...10").unwrap(), expected);
    }

    #[test]
    fn brace_ranges_support_stepping() {
        assert_eq!(
            expand_brace_range("0..3...12").unwrap(),
            vec!["0", "3", "6", "9", "12"]
        );
        assert_eq!(
            expand_brace_range("0..3..12").unwrap(),
            vec!["0", "3", "6", "9"]
        );
        assert_eq!(
            expand_brace_range("10..-2...-10").unwrap(),
            vec!["10", "8", "6", "4", "2", "0", "-2", "-4", "-6", "-8", "-10"]
        );
        assert_eq!(
            expand_brace_range("10..-2..-10").unwrap(),
            vec!["10", "8", "6", "4", "2", "0", "-2", "-4", "-6", "-8"]
        );
    }

    #[test]
    fn brace_alpha_ranges_match_manual() {
        assert_eq!(expand_brace_range("a..d").unwrap(), vec!["a", "b", "c"]);
        assert_eq!(expand_brace_range("d..a").unwrap(), vec!["d", "c", "b"]);
        assert_eq!(
            expand_brace_range("a...d").unwrap(),
            vec!["a", "b", "c", "d"]
        );
        assert_eq!(
            expand_brace_range("d...a").unwrap(),
            vec!["d", "c", "b", "a"]
        );
    }

    /// ion-manual pages 29-30 (`filename.{ext1,ext2}` -> "filename.ext1
    /// filename.ext2") and upstream's `tests/brace_exp.ion` ("single_brace_expansion").
    #[test]
    fn single_brace_permutation_matches_manual() {
        assert_eq!(
            expand_braces("filename.{ext1,ext2}").unwrap(),
            arr(&["filename.ext1", "filename.ext2"])
        );
    }

    /// ion-manual pages 29-30, multiple brace groups in one word
    /// ("multi_brace_expansion" in upstream's `tests/brace_exp.ion`).
    #[test]
    fn multiple_brace_groups_cross_product() {
        assert_eq!(
            expand_braces("job_{01,02}.{ext1,ext2}").unwrap(),
            arr(&["job_01.ext1", "job_01.ext2", "job_02.ext1", "job_02.ext2"])
        );
    }

    /// ion-manual pages 29-30, brace elements containing brace elements of
    /// their own ("nested_brace_expansion" in upstream's `tests/brace_exp.ion`).
    #[test]
    fn nested_brace_groups_expand() {
        assert_eq!(
            expand_braces("job_{01_{out,err},02_{out,err}}.txt").unwrap(),
            arr(&[
                "job_01_out.txt",
                "job_01_err.txt",
                "job_02_out.txt",
                "job_02_err.txt",
            ])
        );
    }

    /// Cross-checked against upstream's `tests/braces.ion`/`braces.out`
    /// (real Ion's own brace-expansion test suite), not just the manual —
    /// deeper nesting and an empty comma-branch (`{d,}`, "d or nothing").
    #[test]
    fn deep_nesting_matches_upstream_ion_test_suite() {
        assert_eq!(
            expand_braces("1{A{1,2},B{1,2}}").unwrap(),
            arr(&["1A1", "1A2", "1B1", "1B2"])
        );
        assert_eq!(
            expand_braces("It{{em,alic}iz,erat}e{d,}").unwrap(),
            arr(&[
                "Itemized",
                "Itemize",
                "Italicized",
                "Italicize",
                "Iterated",
                "Iterate",
            ])
        );
    }

    /// A range and plain literals may sit in the same comma list; each
    /// element is tried as a range independently.
    #[test]
    fn range_and_literal_elements_mix_in_one_group() {
        assert_eq!(
            expand_braces("v{1..3,final}").unwrap(),
            arr(&["v1", "v2", "vfinal"])
        );
    }

    /// A bare `{1..10}` (whole-token) still expands exactly as before —
    /// `expand_braces` subsumes the old whole-token-only `expand_brace_range`
    /// entry point without changing its output.
    #[test]
    fn bare_range_still_works_via_expand_braces() {
        assert_eq!(
            expand_braces("{1..10}").unwrap(),
            expand_brace_range("1..10").unwrap()
        );
    }

    /// `${name}`/`@{name}` is variable-interpolation disambiguation syntax
    /// (handled by `Interpreter::interpolate`), not a permutation group —
    /// must not be touched by brace expansion at all.
    #[test]
    fn dollar_and_at_brace_are_not_permutation_groups() {
        assert_eq!(expand_braces("${name}"), None);
        assert_eq!(expand_braces("@{name}"), None);
        assert_eq!(expand_braces("${name}suffix"), None);
    }

    /// A `${name}` disambiguation group and a real permutation group can
    /// coexist in the same word; only the latter expands.
    #[test]
    fn dollar_brace_coexists_with_permutation_group() {
        assert_eq!(
            expand_braces("${name}.{a,b}").unwrap(),
            arr(&["${name}.a", "${name}.b"])
        );
    }

    /// No brace at all, or a malformed/unmatched brace, falls back to
    /// `None` so callers treat the word as ordinary text.
    #[test]
    fn no_brace_or_unmatched_brace_returns_none() {
        assert_eq!(expand_braces("hello"), None);
        assert_eq!(expand_braces("{unterminated"), None);
        assert_eq!(expand_braces("stray}"), None);
    }

    /// `{}` alone isn't a valid permutation (zero comma-separated
    /// elements) — stays literal rather than vanishing.
    #[test]
    fn empty_braces_stay_literal() {
        assert_eq!(expand_braces("{}"), None);
        assert_eq!(expand_braces("prefix{}suffix"), None);
    }
}
