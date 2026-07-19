//! Structured "table" data flowing through pipelines — the in-process
//! object bridge staged as a post-1.0 upgrade in `ARCHITECTURE.md` §6
//! (item 2) and §7. Real Ion's pipes are flat text, matching every other
//! POSIX-style shell; this is ion-win's own extension, for JSON-emitting
//! Windows tools (`winget list --format json`, `Get-Process |
//! ConvertTo-Json`, REST API responses) that would otherwise need
//! regex-parsing flat strings.
//!
//! Deliberately scoped to *table* shape — an ordered list of flat records,
//! no arbitrary nesting — rather than full JSON. Most real-world JSON from
//! Windows CLI tools is naturally row-shaped (a list of same-shaped
//! objects, or a single object for a single-item result); a general
//! nested-value model would need a real path/query syntax (`.a.b[0].c`) to
//! reach into arbitrary depth, which is out of scope for this first slice.
//! A field whose JSON value is itself an object or array is not rejected —
//! it's kept as that value's own compact JSON text, so building a table
//! never fails just because one field happens to be non-scalar; pipe that
//! field's text through `from-json` again if you need to dig into it.
//!
//! See `ARCHITECTURE.md` §17 for the full design writeup (why
//! table-shaped, why `from-json`/`to-json` are explicit boundary adapters
//! rather than implicit auto-parsing, and why external processes only
//! ever see JSON text, never a `Table` value directly).

/// One row: column name -> string value, in the order the source JSON
/// gave them. A plain `Vec` rather than a map, since real-world JSON
/// arrays sometimes have inconsistent keys across elements — this stores
/// exactly what each row had, with no assumption that every row shares the
/// same columns.
pub type Row = Vec<(String, String)>;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Table {
    pub rows: Vec<Row>,
}

impl Table {
    /// Parses `text` as JSON into a table. Accepts a JSON array of objects
    /// (one row each) or a single bare JSON object (treated as a one-row
    /// table, matching how many CLI tools emit a bare object for a
    /// single-item result rather than wrapping it in a one-element array).
    /// Anything else — a scalar, or an array containing anything other
    /// than objects — is a clear error rather than a guessed-at shape.
    pub fn from_json(text: &str) -> Result<Table, String> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;

        let objects: Vec<serde_json::Map<String, serde_json::Value>> = match value {
            serde_json::Value::Array(items) => items
                .into_iter()
                .map(|item| match item {
                    serde_json::Value::Object(map) => Ok(map),
                    other => Err(format!(
                        "expected an array of objects, found {} inside the array",
                        json_type_name(&other)
                    )),
                })
                .collect::<Result<_, _>>()?,
            serde_json::Value::Object(map) => vec![map],
            other => {
                return Err(format!(
                    "expected a JSON array of objects or a single object, found {}",
                    json_type_name(&other)
                ));
            }
        };

        let rows = objects
            .into_iter()
            .map(|map| {
                map.into_iter()
                    .map(|(k, v)| (k, json_value_to_cell(&v)))
                    .collect()
            })
            .collect();
        Ok(Table { rows })
    }

    /// Serializes the table back to a JSON array of objects, pretty-printed.
    pub fn to_json(&self) -> String {
        let rows: Vec<serde_json::Value> = self
            .rows
            .iter()
            .map(|row| {
                let map: serde_json::Map<String, serde_json::Value> = row
                    .iter()
                    .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                    .collect();
                serde_json::Value::Object(map)
            })
            .collect();
        serde_json::to_string_pretty(&serde_json::Value::Array(rows))
            .unwrap_or_else(|_| "[]".to_string())
    }

    /// Serializes the table to CSV: a header line of column names, one
    /// data line per row. Unlike JSON, CSV needs one fixed set of columns
    /// for the whole table — computed here as the first-seen union across
    /// every row (not just the first row's keys), since rows aren't
    /// assumed to share columns. A row missing a given column gets an
    /// empty cell there, matching CSV's own inability to represent
    /// "absent" separately from "empty" (a real, one-way lossiness versus
    /// JSON — see `from_csv`).
    pub fn to_csv(&self) -> String {
        let mut columns: Vec<String> = Vec::new();
        for row in &self.rows {
            for (k, _) in row {
                if !columns.contains(k) {
                    columns.push(k.clone());
                }
            }
        }

        let mut out = String::new();
        out.push_str(&columns.iter().map(|c| csv_escape(c)).collect::<Vec<_>>().join(","));
        out.push('\n');
        for row in &self.rows {
            let fields: Vec<String> = columns
                .iter()
                .map(|col| {
                    row.iter()
                        .find(|(k, _)| k == col)
                        .map(|(_, v)| csv_escape(v))
                        .unwrap_or_default()
                })
                .collect();
            out.push_str(&fields.join(","));
            out.push('\n');
        }
        out
    }

    /// Parses CSV text into a table: the first record is the header
    /// (column names), each following record becomes a row, with fields
    /// matched to header columns positionally. A row with *fewer* fields
    /// than the header just leaves those trailing columns absent from
    /// that row — matching `select`'s existing "missing column" handling,
    /// not treated as empty-string-present. A row with *more* fields than
    /// the header is a clear error (`from-csv`), since there's no column
    /// name to attribute the extra value to, rather than silently
    /// dropping data. Empty input (or a header with no data rows at all)
    /// produces an empty table, not an error.
    pub fn from_csv(text: &str) -> Result<Table, String> {
        let mut records = parse_csv_records(text).into_iter();
        let Some(header) = records.next() else {
            return Ok(Table::default());
        };

        let mut rows = Vec::new();
        for (i, record) in records.enumerate() {
            if record.len() > header.len() {
                return Err(format!(
                    "from-csv: row {} has {} field(s), expected at most {} (matching the header)",
                    i + 2,
                    record.len(),
                    header.len()
                ));
            }
            let row: Row = header
                .iter()
                .zip(record.iter())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            rows.push(row);
        }
        Ok(Table { rows })
    }

    /// Projects every row down to just the named columns, in the order
    /// requested. A column missing from a given row is simply absent from
    /// that row's projection rather than an error — rows aren't assumed
    /// to share columns (see `Row`'s doc comment).
    pub fn select(&self, columns: &[String]) -> Table {
        let rows = self
            .rows
            .iter()
            .map(|row| {
                columns
                    .iter()
                    .filter_map(|col| {
                        row.iter()
                            .find(|(k, _)| k == col)
                            .map(|(k, v)| (k.clone(), v.clone()))
                    })
                    .collect()
            })
            .collect();
        Table { rows }
    }

    /// Keeps only rows where `column`'s value satisfies `op value`, using
    /// the exact same comparison operators as `test`/`if` (`=`, `==`,
    /// `!=`, `-eq`, `-ne`, `-lt`, `-le`, `-gt`, `-ge`) — reused directly
    /// via `builtins::eval_test` rather than reimplemented, so `where pid
    /// -gt 1000` behaves exactly like `test $pid -gt 1000` would (same
    /// numeric-parse-failure handling included). A row missing `column`
    /// entirely never matches — there's no value to compare — rather than
    /// being treated as an error.
    pub fn filter(&self, column: &str, op: &str, value: &str) -> Table {
        let rows = self
            .rows
            .iter()
            .filter(|row| {
                row.iter().find(|(k, _)| k == column).is_some_and(|(_, v)| {
                    crate::builtins::eval_test(&[v.clone(), op.to_string(), value.to_string()])
                })
            })
            .cloned()
            .collect();
        Table { rows }
    }
}

/// A JSON value's cell text: scalars render as their natural display form;
/// an object/array is kept as its own compact JSON text rather than
/// rejected (see the module doc comment).
fn json_value_to_cell(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn json_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// RFC4180-style CSV field escaping: a field containing a comma, a double
/// quote, or a newline gets wrapped in quotes, with any embedded quote
/// doubled. Left unescaped otherwise, so the common case (plain paths,
/// numbers, hex digests) stays perfectly readable.
fn csv_escape(field: &str) -> String {
    if field.contains(',') || field.contains('"') || field.contains('\n') || field.contains('\r') {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

/// Parses CSV text into records (rows of raw field strings, header
/// included) — a small hand-rolled state machine rather than a naive
/// `split(',')`/`split('\n')`, since a quoted field may legitimately
/// contain literal commas and newlines that aren't record/field
/// separators. An unterminated quote at the end of the input is handled
/// best-effort (whatever was accumulated becomes the final field) rather
/// than erroring, matching this project's general tokenizing philosophy
/// elsewhere (`interp.rs`'s own "unterminated; best-effort" comments).
fn parse_csv_records(text: &str) -> Vec<Vec<String>> {
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    field.push('"');
                    chars.next();
                }
                '"' => in_quotes = false,
                _ => field.push(c),
            }
        } else {
            match c {
                '"' => in_quotes = true,
                ',' => record.push(std::mem::take(&mut field)),
                '\r' => {} // swallowed; \r\n and bare \n both end a record below
                '\n' => {
                    record.push(std::mem::take(&mut field));
                    records.push(std::mem::take(&mut record));
                }
                _ => field.push(c),
            }
        }
    }
    // A final record with no trailing newline still needs flushing.
    if !field.is_empty() || !record.is_empty() {
        record.push(field);
        records.push(record);
    }
    records
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pairs: &[(&str, &str)]) -> Row {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn parses_array_of_objects() {
        let table = Table::from_json(
            r#"[{"name":"svchost.exe","pid":412},{"name":"chrome.exe","pid":8891}]"#,
        )
        .unwrap();
        assert_eq!(
            table.rows,
            vec![
                row(&[("name", "svchost.exe"), ("pid", "412")]),
                row(&[("name", "chrome.exe"), ("pid", "8891")]),
            ]
        );
    }

    #[test]
    fn parses_single_bare_object_as_one_row_table() {
        let table = Table::from_json(r#"{"name":"svchost.exe","pid":412}"#).unwrap();
        assert_eq!(table.rows, vec![row(&[("name", "svchost.exe"), ("pid", "412")])]);
    }

    #[test]
    fn rejects_non_object_array_elements() {
        let err = Table::from_json(r#"[1, 2, 3]"#).unwrap_err();
        assert!(err.contains("expected an array of objects"), "{err}");
    }

    #[test]
    fn rejects_bare_scalar() {
        let err = Table::from_json(r#""just a string""#).unwrap_err();
        assert!(err.contains("expected a JSON array of objects or a single object"), "{err}");
    }

    #[test]
    fn rejects_invalid_json() {
        let err = Table::from_json("{not json").unwrap_err();
        assert!(err.contains("invalid JSON"), "{err}");
    }

    #[test]
    fn nested_fields_are_kept_as_their_own_json_text_not_rejected() {
        let table = Table::from_json(r#"[{"user":{"name":"bob"},"tags":["a","b"]}]"#).unwrap();
        assert_eq!(table.rows.len(), 1);
        let field = |name: &str| {
            table.rows[0]
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert_eq!(field("user"), r#"{"name":"bob"}"#);
        assert_eq!(field("tags"), r#"["a","b"]"#);
    }

    #[test]
    fn select_projects_named_columns_in_requested_order() {
        let table = Table {
            rows: vec![row(&[("name", "svchost.exe"), ("pid", "412"), ("cpu", "0.3")])],
        };
        let selected = table.select(&["pid".to_string(), "name".to_string()]);
        assert_eq!(selected.rows, vec![row(&[("pid", "412"), ("name", "svchost.exe")])]);
    }

    #[test]
    fn select_omits_missing_columns_without_erroring() {
        let table = Table { rows: vec![row(&[("name", "a")])] };
        let selected = table.select(&["name".to_string(), "missing".to_string()]);
        assert_eq!(selected.rows, vec![row(&[("name", "a")])]);
    }

    #[test]
    fn round_trips_through_to_json_and_back() {
        let original = Table {
            rows: vec![
                row(&[("name", "a"), ("pid", "1")]),
                row(&[("name", "b"), ("pid", "2")]),
            ],
        };
        let reparsed = Table::from_json(&original.to_json()).unwrap();
        assert_eq!(original, reparsed);
    }

    #[test]
    fn to_csv_writes_a_header_and_one_line_per_row() {
        let table = Table {
            rows: vec![
                row(&[("name", "svchost.exe"), ("pid", "412")]),
                row(&[("name", "chrome.exe"), ("pid", "8891")]),
            ],
        };
        assert_eq!(
            table.to_csv(),
            "name,pid\nsvchost.exe,412\nchrome.exe,8891\n"
        );
    }

    #[test]
    fn to_csv_quotes_fields_containing_commas_quotes_or_newlines() {
        let table = Table {
            rows: vec![row(&[
                ("note", "has, a comma"),
                ("quote", "say \"hi\""),
                ("multiline", "line one\nline two"),
            ])],
        };
        assert_eq!(
            table.to_csv(),
            "note,quote,multiline\n\"has, a comma\",\"say \"\"hi\"\"\",\"line one\nline two\"\n"
        );
    }

    #[test]
    fn to_csv_column_order_is_the_first_seen_union_across_all_rows() {
        // Second row introduces a column ("extra") the first row doesn't
        // have — the header must still include it, since rows aren't
        // assumed to share columns (see `Row`'s own doc comment).
        let table = Table {
            rows: vec![row(&[("a", "1")]), row(&[("a", "2"), ("extra", "x")])],
        };
        assert_eq!(table.to_csv(), "a,extra\n1,\n2,x\n");
    }

    #[test]
    fn round_trips_through_to_csv_and_back_when_rows_share_columns() {
        let original = Table {
            rows: vec![
                row(&[("name", "a"), ("pid", "1")]),
                row(&[("name", "b"), ("pid", "2")]),
            ],
        };
        let reparsed = Table::from_csv(&original.to_csv()).unwrap();
        assert_eq!(original, reparsed);
    }

    #[test]
    fn from_csv_parses_quoted_fields_with_embedded_commas_and_quotes() {
        let table =
            Table::from_csv("note,quote\n\"has, a comma\",\"say \"\"hi\"\"\"\n").unwrap();
        assert_eq!(
            table.rows,
            vec![row(&[("note", "has, a comma"), ("quote", "say \"hi\"")])]
        );
    }

    #[test]
    fn from_csv_treats_a_short_row_as_missing_trailing_columns_not_empty_ones() {
        let table = Table::from_csv("a,b,c\n1\n").unwrap();
        assert_eq!(table.rows, vec![row(&[("a", "1")])]);
    }

    #[test]
    fn from_csv_rejects_a_row_with_more_fields_than_the_header() {
        let err = Table::from_csv("a,b\n1,2,3\n").unwrap_err();
        assert!(err.contains("from-csv"), "{err}");
        assert!(err.contains("3 field"), "{err}");
    }

    #[test]
    fn from_csv_on_empty_input_is_an_empty_table_not_an_error() {
        assert_eq!(Table::from_csv("").unwrap(), Table::default());
    }

    #[test]
    fn from_csv_header_only_is_an_empty_table() {
        assert_eq!(Table::from_csv("a,b,c\n").unwrap(), Table::default());
    }

    #[test]
    fn filter_keeps_only_rows_matching_numeric_comparison() {
        let table = Table {
            rows: vec![
                row(&[("name", "svchost.exe"), ("pid", "412")]),
                row(&[("name", "chrome.exe"), ("pid", "8891")]),
            ],
        };
        let filtered = table.filter("pid", "-gt", "1000");
        assert_eq!(filtered.rows, vec![row(&[("name", "chrome.exe"), ("pid", "8891")])]);
    }

    #[test]
    fn filter_supports_string_equality() {
        let table = Table {
            rows: vec![row(&[("name", "a")]), row(&[("name", "b")])],
        };
        assert_eq!(table.filter("name", "=", "a").rows, vec![row(&[("name", "a")])]);
        assert_eq!(table.filter("name", "!=", "a").rows, vec![row(&[("name", "b")])]);
    }

    #[test]
    fn filter_excludes_rows_missing_the_column_rather_than_erroring() {
        let table = Table {
            rows: vec![row(&[("name", "a"), ("pid", "1")]), row(&[("name", "b")])],
        };
        let filtered = table.filter("pid", "-eq", "1");
        assert_eq!(filtered.rows, vec![row(&[("name", "a"), ("pid", "1")])]);
    }
}
