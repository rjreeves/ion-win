//! `$(( expr ))` arithmetic expansion (ion-manual page 31).
//!
//! Supports `+ - * / ** % ^ & | << >>`, parentheses, unary `+`/`-`/`~`, and
//! the postfix `²`/`³` square/cube shorthand. Bare identifiers resolve as
//! scalar variable references without needing the `$` sigil, per the
//! manual's own description ("Variables may be passed into arithmetic
//! expansions without the $ sigil").
//!
//! Precedence (low to high): `|` < `^` < `&` < `<<`/`>>` < `+`/`-` <
//! `*`/`/`/`%` < `**` (right-assoc) < unary (`-`/`+`/`~`) < postfix `²`/`³`.
//! The manual doesn't specify precedence beyond "use parenthesis when
//! unsure," so this follows the common C-like convention with `**` bound
//! tighter than `*`/`/`.
//!
//! `~` ("Bitwise NOT") is documented (page 31) with a two-operand syntax
//! (`$((a ~ b))`) and no worked example, ambiguous against the fact that
//! real bitwise NOT is unary (see `ARCHITECTURE.md` section 8 for the full
//! history — upstream's own separate `calc` arithmetic crate wasn't
//! fetchable to resolve it definitively). Implemented here as standard
//! unary bit-complement (`~x`, flipping every bit — Rust's `!` on `i64`),
//! same precedence tier as unary `-`/`+`; integer operands only, matching
//! every other bitwise operator in this module (`&`/`|`/`^`/`<<`/`>>`).
//!
//! NOT implemented: bignum support (values are `i64`/`f64`, matching
//! `let`'s arithmetic).

use std::iter::Peekable;
use std::str::Chars;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Num {
    Int(i64),
    Float(f64),
}

impl Num {
    pub fn to_display_string(self) -> String {
        match self {
            Num::Int(v) => v.to_string(),
            Num::Float(v) => format!("{v:?}"),
        }
    }

    fn as_f64(self) -> f64 {
        match self {
            Num::Int(v) => v as f64,
            Num::Float(v) => v,
        }
    }
}

/// Evaluates an arithmetic expression, resolving bare identifiers through
/// `lookup` (a scalar variable name -> its string value, if defined).
pub fn eval(expr: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<Num, String> {
    let mut parser = Parser {
        chars: expr.chars().peekable(),
        lookup,
    };
    let value = parser.parse_or()?;
    parser.skip_ws();
    if parser.chars.peek().is_some() {
        return Err(format!("unexpected trailing input in expression '{expr}'"));
    }
    Ok(value)
}

struct Parser<'a> {
    chars: Peekable<Chars<'a>>,
    lookup: &'a dyn Fn(&str) -> Option<String>,
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while matches!(self.chars.peek(), Some(c) if c.is_whitespace()) {
            self.chars.next();
        }
    }

    /// Peeks (skipping leading whitespace first) whether the upcoming
    /// characters match `s`, without consuming anything.
    fn peek_is(&mut self, s: &str) -> bool {
        self.skip_ws();
        let mut clone = self.chars.clone();
        for expected in s.chars() {
            if clone.next() != Some(expected) {
                return false;
            }
        }
        true
    }

    fn consume(&mut self, s: &str) {
        for _ in s.chars() {
            self.chars.next();
        }
    }

    fn parse_or(&mut self) -> Result<Num, String> {
        let mut lhs = self.parse_xor()?;
        while self.peek_is("|") {
            self.consume("|");
            let rhs = self.parse_xor()?;
            lhs = int_op(lhs, rhs, "|", |a, b| a | b)?;
        }
        Ok(lhs)
    }

    fn parse_xor(&mut self) -> Result<Num, String> {
        let mut lhs = self.parse_and()?;
        while self.peek_is("^") {
            self.consume("^");
            let rhs = self.parse_and()?;
            lhs = int_op(lhs, rhs, "^", |a, b| a ^ b)?;
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Num, String> {
        let mut lhs = self.parse_shift()?;
        while self.peek_is("&") {
            self.consume("&");
            let rhs = self.parse_shift()?;
            lhs = int_op(lhs, rhs, "&", |a, b| a & b)?;
        }
        Ok(lhs)
    }

    fn parse_shift(&mut self) -> Result<Num, String> {
        let mut lhs = self.parse_additive()?;
        loop {
            if self.peek_is("<<") {
                self.consume("<<");
                let rhs = self.parse_additive()?;
                lhs = int_op(lhs, rhs, "<<", |a, b| a << b)?;
            } else if self.peek_is(">>") {
                self.consume(">>");
                let rhs = self.parse_additive()?;
                lhs = int_op(lhs, rhs, ">>", |a, b| a >> b)?;
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_additive(&mut self) -> Result<Num, String> {
        let mut lhs = self.parse_multiplicative()?;
        loop {
            if self.peek_is("+") {
                self.consume("+");
                let rhs = self.parse_multiplicative()?;
                lhs = numeric_op(lhs, rhs, |a, b| a.wrapping_add(b), |a, b| a + b);
            } else if self.peek_is("-") {
                self.consume("-");
                let rhs = self.parse_multiplicative()?;
                lhs = numeric_op(lhs, rhs, |a, b| a.wrapping_sub(b), |a, b| a - b);
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_multiplicative(&mut self) -> Result<Num, String> {
        let mut lhs = self.parse_power()?;
        loop {
            if self.peek_is("*") {
                self.consume("*");
                let rhs = self.parse_power()?;
                lhs = numeric_op(lhs, rhs, |a, b| a.wrapping_mul(b), |a, b| a * b);
            } else if self.peek_is("/") {
                self.consume("/");
                let rhs = self.parse_power()?;
                // Divide always yields a float (ion-manual: `/` is float-typed).
                lhs = Num::Float(lhs.as_f64() / rhs.as_f64());
            } else if self.peek_is("%") {
                self.consume("%");
                let rhs = self.parse_power()?;
                lhs = match (lhs, rhs) {
                    (Num::Int(a), Num::Int(b)) => {
                        if b == 0 {
                            return Err("modulus by zero".to_string());
                        }
                        Num::Int(a % b)
                    }
                    _ => Num::Float(lhs.as_f64() % rhs.as_f64()),
                };
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    /// `**` is right-associative and binds tighter than `*`/`/`/`%`.
    fn parse_power(&mut self) -> Result<Num, String> {
        let base = self.parse_unary()?;
        if self.peek_is("**") {
            self.consume("**");
            let exp = self.parse_power()?;
            return Ok(Num::Float(base.as_f64().powf(exp.as_f64())));
        }
        Ok(base)
    }

    fn parse_unary(&mut self) -> Result<Num, String> {
        if self.peek_is("-") {
            self.consume("-");
            let value = self.parse_unary()?;
            return Ok(match value {
                Num::Int(v) => Num::Int(-v),
                Num::Float(v) => Num::Float(-v),
            });
        }
        if self.peek_is("+") {
            self.consume("+");
            return self.parse_unary();
        }
        if self.peek_is("~") {
            self.consume("~");
            let value = self.parse_unary()?;
            return match value {
                Num::Int(v) => Ok(Num::Int(!v)),
                Num::Float(_) => Err("bitwise NOT ('~') requires an integer operand".to_string()),
            };
        }
        self.parse_postfix()
    }

    /// Applies the documented `²`/`³` postfix square/cube shorthand.
    fn parse_postfix(&mut self) -> Result<Num, String> {
        let mut value = self.parse_primary()?;
        loop {
            if self.peek_is("\u{00B2}") {
                self.consume("\u{00B2}");
                value = Num::Float(value.as_f64().powf(2.0));
            } else if self.peek_is("\u{00B3}") {
                self.consume("\u{00B3}");
                value = Num::Float(value.as_f64().powf(3.0));
            } else {
                break;
            }
        }
        Ok(value)
    }

    fn parse_primary(&mut self) -> Result<Num, String> {
        self.skip_ws();
        if self.peek_is("(") {
            self.consume("(");
            let value = self.parse_or()?;
            if self.peek_is(")") {
                self.consume(")");
            } else {
                return Err("expected closing ')'".to_string());
            }
            return Ok(value);
        }

        if let Some(&c) = self.chars.peek() {
            if c.is_ascii_digit() || c == '.' {
                return self.parse_number();
            }
            if c.is_alphabetic() || c == '_' {
                return self.parse_identifier();
            }
        }
        Err("expected a number, variable, or '('".to_string())
    }

    fn parse_number(&mut self) -> Result<Num, String> {
        let mut buf = String::new();
        let mut is_float = false;
        while let Some(&c) = self.chars.peek() {
            if c.is_ascii_digit() {
                buf.push(c);
                self.chars.next();
            } else if c == '.' && !is_float {
                is_float = true;
                buf.push(c);
                self.chars.next();
            } else {
                break;
            }
        }
        if is_float {
            buf.parse::<f64>()
                .map(Num::Float)
                .map_err(|_| format!("invalid number '{buf}'"))
        } else {
            buf.parse::<i64>()
                .map(Num::Int)
                .map_err(|_| format!("invalid number '{buf}'"))
        }
    }

    fn parse_identifier(&mut self) -> Result<Num, String> {
        let mut name = String::new();
        while let Some(&c) = self.chars.peek() {
            if c.is_alphanumeric() || c == '_' {
                name.push(c);
                self.chars.next();
            } else {
                break;
            }
        }
        match (self.lookup)(&name) {
            Some(value) => {
                if let Ok(i) = value.parse::<i64>() {
                    Ok(Num::Int(i))
                } else if let Ok(f) = value.parse::<f64>() {
                    Ok(Num::Float(f))
                } else {
                    Err(format!(
                        "variable '{name}' does not hold a number (got '{value}')"
                    ))
                }
            }
            None => Err(format!("{name}: variable does not exist")),
        }
    }
}

fn int_op(lhs: Num, rhs: Num, op_name: &str, f: impl Fn(i64, i64) -> i64) -> Result<Num, String> {
    match (lhs, rhs) {
        (Num::Int(a), Num::Int(b)) => Ok(Num::Int(f(a, b))),
        _ => Err(format!("operator '{op_name}' requires integer operands")),
    }
}

fn numeric_op(
    lhs: Num,
    rhs: Num,
    fi: impl Fn(i64, i64) -> i64,
    ff: impl Fn(f64, f64) -> f64,
) -> Num {
    match (lhs, rhs) {
        (Num::Int(a), Num::Int(b)) => Num::Int(fi(a, b)),
        _ => Num::Float(ff(lhs.as_f64(), rhs.as_f64())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_vars(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn basic_add() {
        assert_eq!(
            eval("a + b", &|n| match n {
                "a" => Some("2".to_string()),
                "b" => Some("3".to_string()),
                _ => None,
            })
            .unwrap(),
            Num::Int(5)
        );
    }

    #[test]
    fn power_binds_tighter_than_multiply() {
        // 2 * 3 ** 2 == 2 * 9 == 18
        assert_eq!(eval("2 * 3 ** 2", &no_vars).unwrap(), Num::Float(18.0));
    }

    #[test]
    fn divide_and_power_are_always_float() {
        assert_eq!(eval("4 / 2", &no_vars).unwrap(), Num::Float(2.0));
        assert_eq!(eval("2 ** 3", &no_vars).unwrap(), Num::Float(8.0));
    }

    #[test]
    fn parenthesized_expression_matches_manual_example() {
        // $((4 * (pi * r^2))) with pi=3.0, r=2 -> 4 * (3.0 * 4) = 48.0
        assert_eq!(
            eval("4 * (pi * (r ** 2))", &|n| match n {
                "pi" => Some("3.0".to_string()),
                "r" => Some("2".to_string()),
                _ => None,
            })
            .unwrap(),
            Num::Float(48.0)
        );
    }

    #[test]
    fn postfix_square_and_cube() {
        assert_eq!(eval("3\u{00B2}", &no_vars).unwrap(), Num::Float(9.0));
        assert_eq!(eval("2\u{00B3}", &no_vars).unwrap(), Num::Float(8.0));
    }

    #[test]
    fn unary_minus() {
        assert_eq!(eval("5 - -3", &no_vars).unwrap(), Num::Int(8));
    }

    #[test]
    fn undefined_variable_errors() {
        assert!(eval("missing + 1", &no_vars).is_err());
    }

    #[test]
    fn bitwise_not_complements_all_bits() {
        assert_eq!(eval("~0", &no_vars).unwrap(), Num::Int(-1));
        assert_eq!(eval("~5", &no_vars).unwrap(), Num::Int(-6));
        assert_eq!(eval("~-1", &no_vars).unwrap(), Num::Int(0));
    }

    #[test]
    fn bitwise_not_binds_at_the_unary_tier() {
        // ~ sits alongside unary -/+, and this module's unary tier binds
        // *tighter* than ** (parse_power takes parse_unary's result as its
        // base): ~2 ** 2 == (~2) ** 2 == (-3) ** 2 == 9.0.
        assert_eq!(eval("~2 ** 2", &no_vars).unwrap(), Num::Float(9.0));
        assert_eq!(eval("~(1 + 1)", &no_vars).unwrap(), Num::Int(-3));
    }

    #[test]
    fn bitwise_not_rejects_float_operand() {
        let err = eval("~3.5", &no_vars).unwrap_err();
        assert!(
            err.contains("integer"),
            "error should name the integer requirement: {err}"
        );
    }
}
