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
}
