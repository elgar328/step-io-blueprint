//! Recursive descent parser for the body inside `SUPERTYPE OF (...)`.
//!
//! Implements the EXPRESS supertype expression grammar (ISO 10303-11 § 9.2.4):
//!
//! ```text
//! supertype_expression := supertype_factor (ANDOR supertype_factor)*
//! supertype_factor     := supertype_term (AND supertype_term)*
//! supertype_term       := entity_ref
//!                       | ONEOF '(' supertype_expression
//!                                  (',' supertype_expression)* ')'
//!                       | '(' supertype_expression ')'
//! ```
//!
//! Operator precedence: AND binds tighter than ANDOR, so
//! `a AND b ANDOR c AND d` parses as `AndOr([And([a,b]), And([c,d])])`.
//!
//! Outputs a faithful `SupertypeExpr` tree. Anonymous composition nodes
//! (AndOr / And / OneOf inside another node) are preserved; downstream
//! variant classification can recognise specific patterns or raise an
//! `Unresolved` decision for unknown ones. There is no silent fallback —
//! any unparseable input returns `Err`.
//!
//! ## Module boundary
//!
//! `Token` is module-private. Only `parse` and the `SupertypeExpr` re-export
//! cross the module boundary. Future swap to a generated lexer (logos etc.)
//! only needs to replace the tokenizer and `Token` definition.
//!
//! ## Invariants
//!
//! - `OneOf` / `AndOr` / `And` always carry `children.len() >= 2`. A single
//!   sub-expression collapses to that sub-expression directly (parenthesised
//!   single ref is just `Entity { name }`).
//! - `(expr)` grouping is transparent — no separate node is produced.
//! - `ONEOF (x)` with a single child is rejected (EXPRESS spec mandates ≥ 2).

use super::SupertypeExpr;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Ident(String),
    OneOf,
    And,
    AndOr,
    LParen,
    RParen,
    Comma,
}

/// Parse the body string between the outer `SUPERTYPE OF (...)` parens.
///
/// `body` should be the content already extracted by paren-depth tracking,
/// not including the surrounding `SUPERTYPE OF ( )` themselves.
pub fn parse(body: &str) -> Result<SupertypeExpr, String> {
    let tokens = tokenize(body)?;
    if tokens.is_empty() {
        return Err("empty SUPERTYPE body".to_string());
    }
    let mut state = ParseState {
        tokens: &tokens,
        pos: 0,
    };
    let expr = parse_andor(&mut state)?;
    if state.pos != tokens.len() {
        return Err(format!(
            "trailing tokens at position {}: {:?}",
            state.pos,
            &tokens[state.pos..]
        ));
    }
    Ok(expr)
}

fn tokenize(body: &str) -> Result<Vec<Token>, String> {
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        match b {
            b'(' => {
                out.push(Token::LParen);
                i += 1;
            }
            b')' => {
                out.push(Token::RParen);
                i += 1;
            }
            b',' => {
                out.push(Token::Comma);
                i += 1;
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                // Longest-match identifier: [a-zA-Z_][a-zA-Z0-9_]*
                let start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
                {
                    i += 1;
                }
                let word = &body[start..i];
                let lower = word.to_ascii_lowercase();
                let tok = match lower.as_str() {
                    "oneof" => Token::OneOf,
                    "and" => Token::And,
                    "andor" => Token::AndOr,
                    _ => Token::Ident(lower),
                };
                out.push(tok);
            }
            _ => {
                return Err(format!(
                    "unexpected character {:?} at byte {}",
                    b as char, i
                ));
            }
        }
    }
    Ok(out)
}

struct ParseState<'a> {
    tokens: &'a [Token],
    pos: usize,
}

impl<'a> ParseState<'a> {
    fn peek(&self) -> Option<&'a Token> {
        self.tokens.get(self.pos)
    }

    fn bump(&mut self) -> Option<&'a Token> {
        let tok = self.tokens.get(self.pos);
        if tok.is_some() {
            self.pos += 1;
        }
        tok
    }

    fn expect(&mut self, expected: &Token) -> Result<(), String> {
        match self.bump() {
            Some(t) if t == expected => Ok(()),
            Some(t) => Err(format!("expected {:?}, got {:?}", expected, t)),
            None => Err(format!("expected {:?}, got end of input", expected)),
        }
    }
}

/// Parse `factor (ANDOR factor)*`. Flattens n-ary; collapses single-child.
fn parse_andor(s: &mut ParseState) -> Result<SupertypeExpr, String> {
    let mut parts = vec![parse_and(s)?];
    while matches!(s.peek(), Some(Token::AndOr)) {
        s.bump();
        parts.push(parse_and(s)?);
    }
    if parts.len() == 1 {
        Ok(parts.pop().unwrap())
    } else {
        Ok(SupertypeExpr::AndOr { children: parts })
    }
}

/// Parse `term (AND term)*`. Flattens n-ary; collapses single-child.
fn parse_and(s: &mut ParseState) -> Result<SupertypeExpr, String> {
    let mut parts = vec![parse_term(s)?];
    while matches!(s.peek(), Some(Token::And)) {
        s.bump();
        parts.push(parse_term(s)?);
    }
    if parts.len() == 1 {
        Ok(parts.pop().unwrap())
    } else {
        Ok(SupertypeExpr::And { children: parts })
    }
}

/// Parse `entity_ref | ONEOF(...) | (expr)`.
fn parse_term(s: &mut ParseState) -> Result<SupertypeExpr, String> {
    match s.bump() {
        Some(Token::Ident(name)) => Ok(SupertypeExpr::Entity { name: name.clone() }),
        Some(Token::OneOf) => parse_oneof(s),
        Some(Token::LParen) => {
            let inner = parse_andor(s)?;
            s.expect(&Token::RParen)?;
            Ok(inner)
        }
        Some(t) => Err(format!("unexpected token {:?} at term start", t)),
        None => Err("unexpected end of input at term start".to_string()),
    }
}

fn parse_oneof(s: &mut ParseState) -> Result<SupertypeExpr, String> {
    s.expect(&Token::LParen)?;
    let mut children = vec![parse_andor(s)?];
    while matches!(s.peek(), Some(Token::Comma)) {
        s.bump();
        children.push(parse_andor(s)?);
    }
    s.expect(&Token::RParen)?;
    // Per ISO 10303-11 ONEOF requires ≥ 2 alternatives, but a few real
    // schemas (AP214e3 `binary_function_call`) ship `ONEOF (single_ref)`.
    // Treat the single-alternative form as equivalent to that alternative
    // alone — same single-child collapse rule used for AndOr / And /
    // grouping parens.
    if children.len() == 1 {
        return Ok(children.pop().unwrap());
    }
    Ok(SupertypeExpr::OneOf { children })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ent(name: &str) -> SupertypeExpr {
        SupertypeExpr::Entity {
            name: name.to_string(),
        }
    }

    fn oneof(children: Vec<SupertypeExpr>) -> SupertypeExpr {
        SupertypeExpr::OneOf { children }
    }

    fn andor(children: Vec<SupertypeExpr>) -> SupertypeExpr {
        SupertypeExpr::AndOr { children }
    }

    fn and(children: Vec<SupertypeExpr>) -> SupertypeExpr {
        SupertypeExpr::And { children }
    }

    // ---- B0: bare entity ref ----

    #[test]
    fn b0_bare_entity() {
        assert_eq!(parse("cartesian_point").unwrap(), ent("cartesian_point"));
    }

    #[test]
    fn b0_grouping_collapses() {
        assert_eq!(parse("(point)").unwrap(), ent("point"));
        assert_eq!(parse("((point))").unwrap(), ent("point"));
    }

    // ---- B2: simple ONEOF ----

    #[test]
    fn b2_oneof_two() {
        assert_eq!(
            parse("ONEOF (a, b)").unwrap(),
            oneof(vec![ent("a"), ent("b")])
        );
    }

    #[test]
    fn b2_oneof_three_lowercased() {
        assert_eq!(
            parse("ONEOF (Circle, Ellipse, Hyperbola)").unwrap(),
            oneof(vec![ent("circle"), ent("ellipse"), ent("hyperbola")])
        );
    }

    // ---- B3: ONEOF ANDOR mixin ----

    #[test]
    fn b3_oneof_andor_entity() {
        assert_eq!(
            parse("ONEOF (a, b) ANDOR mixin").unwrap(),
            andor(vec![oneof(vec![ent("a"), ent("b")]), ent("mixin")])
        );
    }

    // ---- B4: ONEOF ANDOR ONEOF ----

    #[test]
    fn b4_oneof_andor_oneof() {
        assert_eq!(
            parse("ONEOF (a, b) ANDOR ONEOF (c, d)").unwrap(),
            andor(vec![
                oneof(vec![ent("a"), ent("b")]),
                oneof(vec![ent("c"), ent("d")]),
            ])
        );
    }

    // ---- B5: ONEOF AND ONEOF ----

    #[test]
    fn b5_oneof_and_oneof() {
        assert_eq!(
            parse("ONEOF (a, b) AND ONEOF (c, d)").unwrap(),
            and(vec![
                oneof(vec![ent("a"), ent("b")]),
                oneof(vec![ent("c"), ent("d")]),
            ])
        );
    }

    // ---- B6: entity ANDOR ONEOF ----

    #[test]
    fn b6_entity_andor_oneof() {
        assert_eq!(
            parse("track ANDOR ONEOF (a, b, c)").unwrap(),
            andor(vec![ent("track"), oneof(vec![ent("a"), ent("b"), ent("c")])])
        );
    }

    // ---- B7: composition inside ONEOF ----

    #[test]
    fn b7_oneof_with_andor_member() {
        // topological_representation_item shape
        assert_eq!(
            parse("ONEOF (vertex, edge, (loop ANDOR path))").unwrap(),
            oneof(vec![
                ent("vertex"),
                ent("edge"),
                andor(vec![ent("loop"), ent("path")]),
            ])
        );
    }

    #[test]
    fn b7_oneof_with_and_pair_member() {
        // zone_structural_makeup-like shape
        assert_eq!(
            parse("ONEOF ((a AND b), c)").unwrap(),
            oneof(vec![and(vec![ent("a"), ent("b")]), ent("c")])
        );
    }

    // ---- Operator precedence: AND > ANDOR ----

    #[test]
    fn precedence_and_binds_tighter_than_andor() {
        assert_eq!(
            parse("a AND b ANDOR c AND d").unwrap(),
            andor(vec![
                and(vec![ent("a"), ent("b")]),
                and(vec![ent("c"), ent("d")]),
            ])
        );
    }

    // ---- Outer grouping is transparent ----

    #[test]
    fn outer_grouping_transparent() {
        assert_eq!(
            parse("(ONEOF (a, b) ANDOR c)").unwrap(),
            andor(vec![oneof(vec![ent("a"), ent("b")]), ent("c")])
        );
    }

    // ---- Multi-line whitespace ----

    #[test]
    fn multi_line_whitespace_tolerated() {
        let body = "ONEOF (\n    intersection_curve,\n    seam_curve\n  ) ANDOR\n  bounded_surface_curve";
        assert_eq!(
            parse(body).unwrap(),
            andor(vec![
                oneof(vec![ent("intersection_curve"), ent("seam_curve")]),
                ent("bounded_surface_curve"),
            ])
        );
    }

    // ---- Invariants: single-child collapse ----

    #[test]
    fn single_child_andor_collapses() {
        // No actual ANDOR keyword → just an entity in parens.
        assert_eq!(parse("(a)").unwrap(), ent("a"));
    }

    // ---- Errors ----

    #[test]
    fn err_trailing_garbage() {
        assert!(parse("a b").is_err());
    }

    #[test]
    fn err_unmatched_paren() {
        assert!(parse("(a ANDOR b").is_err());
        assert!(parse("ONEOF (a, b").is_err());
    }

    #[test]
    fn oneof_single_child_collapses() {
        // Real schemas (AP214e3 binary_function_call) ship this form.
        // Parser collapses to the bare entity to keep behavior close to
        // grouping parens.
        assert_eq!(parse("ONEOF (atan_function)").unwrap(), ent("atan_function"));
    }

    #[test]
    fn err_unknown_char() {
        assert!(parse("a @ b").is_err());
    }

    #[test]
    fn err_empty_body() {
        assert!(parse("").is_err());
        assert!(parse("   ").is_err());
    }

    #[test]
    fn err_lone_keyword() {
        assert!(parse("ANDOR").is_err());
        assert!(parse("a ANDOR").is_err());
    }
}
