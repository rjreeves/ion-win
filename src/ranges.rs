//! Range/slice parsing (ion-manual pages 45-48) and brace *range* expansion
//! (the numeric/alpha-sequence subset of ion-manual pages 29-30 — general
//! brace *permutation* lists like `{ext1,ext2}` or nested braces are a
//! separate, not-yet-implemented feature).
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
/// start/end range, leaving comma-separated permutation lists like
/// `"ext1,ext2"` untouched (that's a separate, unimplemented feature).
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
}
