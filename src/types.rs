//! Primitive type tags shared by typed `let`/`fn` parameter checking
//! (ion-manual pages 7-8, 60).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeTag {
    Str,
    Bool,
    Int,
    Float,
    Date,
    Time,
    DateTime,
    Duration,
}

pub fn parse_type_name(s: &str) -> Option<TypeTag> {
    match s {
        "str" => Some(TypeTag::Str),
        "bool" => Some(TypeTag::Bool),
        "int" => Some(TypeTag::Int),
        "float" => Some(TypeTag::Float),
        "date" => Some(TypeTag::Date),
        "time" => Some(TypeTag::Time),
        "datetime" | "timestamp" => Some(TypeTag::DateTime),
        "duration" | "interval" => Some(TypeTag::Duration),
        _ => None,
    }
}

/// Validates (and, for `bool`, normalizes) a single scalar value against a
/// type tag. Matches ion-manual page 8: `1`/`true` -> `true`;
/// `0`/`false`/`n` -> `false`; anything else is a type error.
pub fn validate(value: &str, ty: TypeTag) -> Result<String, String> {
    match ty {
        TypeTag::Str => Ok(value.to_string()),
        TypeTag::Bool => {
            if value == "1" || value.eq_ignore_ascii_case("true") {
                Ok("true".to_string())
            } else if value == "0"
                || value.eq_ignore_ascii_case("false")
                || value.eq_ignore_ascii_case("n")
            {
                Ok("false".to_string())
            } else {
                Err(format!("expected bool, found value '{value}'"))
            }
        }
        TypeTag::Int => value
            .parse::<i64>()
            .map(|_| value.to_string())
            .map_err(|_| format!("expected int, found value '{value}'")),
        TypeTag::Float => value
            .parse::<f64>()
            .map(|_| value.to_string())
            .map_err(|_| format!("expected float, found value '{value}'")),
        TypeTag::Date => crate::temporal::date(value),
        TypeTag::Time => crate::temporal::time(value),
        TypeTag::DateTime => crate::temporal::datetime(value),
        TypeTag::Duration => crate::temporal::duration(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bool_normalizes_manual_examples() {
        assert_eq!(validate("1", TypeTag::Bool), Ok("true".to_string()));
        assert_eq!(validate("true", TypeTag::Bool), Ok("true".to_string()));
        assert_eq!(validate("n", TypeTag::Bool), Ok("false".to_string()));
        assert!(validate("", TypeTag::Bool).is_err());
    }

    #[test]
    fn int_rejects_non_numeric() {
        assert!(validate("3", TypeTag::Int).is_ok());
        assert!(validate("a", TypeTag::Int).is_err());
    }

    #[test]
    fn temporal_types_validate_and_normalize() {
        assert_eq!(validate("2026-7-3", TypeTag::Date), Ok("2026-07-03".into()));
        assert_eq!(validate("9:05", TypeTag::Time), Ok("09:05:00".into()));
        assert_eq!(
            validate("2026-07-03 09:05", TypeTag::DateTime),
            Ok("2026-07-03T09:05:00".into())
        );
        assert_eq!(validate("1 day 2 hours", TypeTag::Duration), Ok("PT93600S".into()));
        assert!(validate("2026-02-30", TypeTag::Date).is_err());
    }
}
