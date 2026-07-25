//! Table-column temporal transformations for the `date-column` pipeline
//! stage. Source and destination columns are always explicit, so preserving
//! the original or replacing it is a visible choice in the command.

use crate::table::Table;

const USAGE: &str = "date-column: usage: date-column SOURCE DEST \
    date|time|datetime | format PATTERN | timezone ZONE [AMBIGUOUS] [GAP] | \
    add|sub INTERVAL [ZONE [AMBIGUOUS] [GAP]]";

pub fn transform(table: Table, args: &[String]) -> Result<Table, String> {
    if args.len() < 3 {
        return Err(USAGE.to_string());
    }
    let source = &args[0];
    let destination = &args[1];
    let operation = &args[2];
    let operation_args = &args[3..];
    validate_arity(operation, operation_args)?;

    let mut rows = Vec::with_capacity(table.rows.len());
    for (index, mut row) in table.rows.into_iter().enumerate() {
        let value = row
            .iter()
            .find(|(name, _)| name == source)
            .map(|(_, value)| value.clone())
            .ok_or_else(|| {
                format!(
                    "date-column: row {} has no source column '{}'",
                    index + 1,
                    source
                )
            })?;
        let converted = convert(operation, &value, operation_args)
            .map_err(|error| format!("date-column: row {}: {error}", index + 1))?;
        if let Some((_, existing)) = row.iter_mut().find(|(name, _)| name == destination) {
            *existing = converted;
        } else {
            row.push((destination.clone(), converted));
        }
        rows.push(row);
    }
    Ok(Table { rows })
}

fn validate_arity(operation: &str, args: &[String]) -> Result<(), String> {
    let valid = match operation {
        "date" | "time" | "datetime" | "timestamp" => args.is_empty(),
        "format" => args.len() == 1,
        "timezone" | "at-timezone" => (1..=3).contains(&args.len()),
        "add" | "sub" => (1..=4).contains(&args.len()),
        _ => {
            return Err(format!(
                "date-column: unsupported operation '{operation}'\n{USAGE}"
            ))
        }
    };
    if valid {
        Ok(())
    } else {
        Err(format!("date-column: invalid arguments for '{operation}'\n{USAGE}"))
    }
}

fn convert(operation: &str, value: &str, args: &[String]) -> Result<String, String> {
    match operation {
        "date" => crate::temporal::date(value),
        "time" => crate::temporal::time(value),
        "datetime" | "timestamp" => crate::temporal::datetime(value),
        "format" => crate::temporal::format(value, &args[0]),
        "timezone" | "at-timezone" => crate::temporal::timezone(
            value,
            &args[0],
            args.get(1).map(String::as_str),
            args.get(2).map(String::as_str),
        ),
        "add" | "sub" if args.len() == 1 => {
            if operation == "add" {
                crate::temporal::add(value, &args[0])
            } else {
                crate::temporal::subtract(value, &args[0])
            }
        }
        "add" | "sub" => {
            let zone = &args[1];
            let ambiguous = args.get(2).map(String::as_str);
            let gap = args.get(3).map(String::as_str);
            if operation == "add" {
                crate::temporal::add_in_timezone(
                    value,
                    &args[0],
                    zone,
                    ambiguous,
                    gap,
                )
            } else {
                crate::temporal::subtract_in_timezone(
                    value,
                    &args[0],
                    zone,
                    ambiguous,
                    gap,
                )
            }
        }
        _ => unreachable!("operation and arity validated above"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(rows: &[&[(&str, &str)]]) -> Table {
        Table {
            rows: rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|(name, value)| (name.to_string(), value.to_string()))
                        .collect()
                })
                .collect(),
        }
    }

    fn field<'a>(table: &'a Table, row: usize, column: &str) -> &'a str {
        table.rows[row]
            .iter()
            .find(|(name, _)| name == column)
            .map(|(_, value)| value.as_str())
            .unwrap()
    }

    #[test]
    fn parses_and_formats_entire_columns_without_overwriting_source() {
        let input = table(&[
            &[("created", "2026-7-3")],
            &[("created", "2024-2-29")],
        ]);
        let parsed = transform(
            input,
            &["created", "canonical", "date"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert_eq!(field(&parsed, 0, "created"), "2026-7-3");
        assert_eq!(field(&parsed, 0, "canonical"), "2026-07-03");

        let formatted = transform(
            parsed,
            &["canonical", "display", "format", "dd-MMM-yy"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert_eq!(field(&formatted, 0, "display"), "03-Jul-26");
        assert_eq!(field(&formatted, 1, "display"), "29-Feb-24");
    }

    #[test]
    fn timezone_and_calendar_add_transform_every_row() {
        let input = table(&[
            &[("at", "2026-09-04T09:00:00")],
            &[("at", "2026-09-05T09:00:00")],
        ]);
        let shifted = transform(
            input,
            &["at", "due", "add", "1 month", "Australia/Sydney"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert_eq!(field(&shifted, 0, "due"), "2026-10-04T09:00:00+11:00");
        assert_eq!(field(&shifted, 1, "due"), "2026-10-05T09:00:00+11:00");
    }

    #[test]
    fn missing_or_invalid_cells_report_the_row_and_leave_input_atomic() {
        let missing = table(&[&[("at", "2026-01-01")], &[("other", "x")]]);
        let error = transform(
            missing,
            &["at", "parsed", "date"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>(),
        )
        .unwrap_err();
        assert!(error.contains("row 2"), "{error}");
        assert!(error.contains("no source column"), "{error}");

        let invalid = table(&[&[("at", "not-a-date")]]);
        let error = transform(
            invalid,
            &["at", "parsed", "date"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>(),
        )
        .unwrap_err();
        assert!(error.contains("row 1"), "{error}");
        assert!(error.contains("expected date"), "{error}");
    }
}
