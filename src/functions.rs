//! Function definitions: parameter specs (with optional type hints and
//! array markers) and docstrings, per ion-manual pages 60-61.

use crate::types::{parse_type_name, TypeTag};

#[derive(Clone, Debug)]
pub struct ParamSpec {
    pub name: String,
    pub array: bool,
    pub ty: Option<TypeTag>,
}

#[derive(Clone, Debug)]
pub struct FunctionDef {
    pub params: Vec<ParamSpec>,
    pub body: Vec<String>,
    pub doc: Option<String>,
}

/// Parses the tokens following `fn NAME` on a function header line into
/// parameter specs and an optional docstring, e.g.
/// `fn square x -- Squares a single number` or
/// `fn hello name age:int hobbies:[str]`.
pub fn parse_params(tokens: &[String]) -> (Vec<ParamSpec>, Option<String>) {
    let mut params = Vec::new();
    let mut iter = tokens.iter();
    while let Some(token) = iter.next() {
        if token == "--" {
            let doc: Vec<&str> = iter.map(|s| s.as_str()).collect();
            return (params, Some(doc.join(" ")));
        }
        params.push(parse_one_param(token));
    }
    (params, None)
}

fn parse_one_param(token: &str) -> ParamSpec {
    match token.split_once(':') {
        None => ParamSpec {
            name: token.to_string(),
            array: false,
            ty: None,
        },
        Some((name, ty_str)) => {
            if let Some(inner) = ty_str.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                ParamSpec {
                    name: name.to_string(),
                    array: true,
                    ty: parse_type_name(inner),
                }
            } else {
                ParamSpec {
                    name: name.to_string(),
                    array: false,
                    ty: parse_type_name(ty_str),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typed_and_array_params_with_docstring() {
        let tokens: Vec<String> = "name age:int hobbies:[str] -- greets someone"
            .split_whitespace()
            .map(String::from)
            .collect();
        let (params, doc) = parse_params(&tokens);
        assert_eq!(params.len(), 3);
        assert_eq!(params[0].name, "name");
        assert!(!params[0].array && params[0].ty.is_none());
        assert_eq!(params[1].name, "age");
        assert_eq!(params[1].ty, Some(TypeTag::Int));
        assert_eq!(params[2].name, "hobbies");
        assert!(params[2].array);
        assert_eq!(params[2].ty, Some(TypeTag::Str));
        assert_eq!(doc.as_deref(), Some("greets someone"));
    }
}
