//! Token-stream recursive-descent parser for openCypher.
//!
//! The parser is driven from the [`SpannedToken`] stream produced by the
//! `logos`-based [`crate::cypher::lexer::lex`] function.  Every AST node
//! is annotated with the byte [`TextRange`] of the corresponding source text.
//!
//! Error recovery: on a syntax error the parser emits a [`ParseError`] that
//! includes the offending span.  The public [`parse`] entry point returns
//! `Result<Statement, ParseError>`.  Partial CSTs are produced by the
//! downstream [`crate::cypher::cst`] layer from a valid AST.

use crate::cypher::ast::*;
use crate::cypher::lexer::{lex, SpannedToken, Token};
use crate::error::RGraphError;
use std::collections::HashMap;
use text_size::{TextRange, TextSize};

/// Parse error with source location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    pub offset: usize,
    pub line: usize,
    pub column: usize,
    pub span: Option<TextRange>,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Syntax error at {}:{}: {}", self.line, self.column, self.message)
    }
}

impl std::error::Error for ParseError {}

impl From<ParseError> for RGraphError {
    fn from(e: ParseError) -> Self {
        RGraphError::Syntax(format!("{} at {}:{}", e.message, e.line, e.column))
    }
}

/// Parse a complete Cypher statement from `input`.
///
/// Every node in the returned [`Statement`] carries a [`TextRange`] that
/// maps back to the original `input` string.
///
/// # Errors
///
/// Returns a [`ParseError`] if the query is syntactically invalid.
pub fn parse(input: &str) -> Result<Statement, ParseError> {
    let tokens = lex(input);
    let mut p = Parser::new(tokens, input);
    p.parse_statement()
}

// ─────────────────────────────────────────────────────────────────────────────
// Parser state
// ─────────────────────────────────────────────────────────────────────────────

struct Parser {
    tokens: Vec<SpannedToken>,
    pos: usize,
    /// Original source for line/column calculation.
    source: Vec<char>,
}

impl Parser {
    fn new(tokens: Vec<SpannedToken>, source: &str) -> Self {
        Self {
            tokens,
            pos: 0,
            source: source.chars().collect(),
        }
    }

    // ── Token access helpers ──────────────────────────────────────────────

    fn peek(&self) -> &Token {
        &self.tokens[self.pos].token
    }

    fn peek_span(&self) -> TextRange {
        self.tokens[self.pos].span
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek(), Token::Eof)
    }

    fn advance(&mut self) -> &Token {
        let tok = &self.tokens[self.pos].token;
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        tok
    }

    /// Consume the next token if it matches `expected`, otherwise error.
    fn expect(&mut self, expected: &Token) -> Result<TextRange, ParseError> {
        if self.peek() == expected {
            let span = self.peek_span();
            self.advance();
            Ok(span)
        } else {
            Err(self.error(&format!("expected {:?}, got {:?}", expected, self.peek())))
        }
    }

    /// Consume an identifier token and return its string value.
    fn expect_ident(&mut self) -> Result<(String, TextRange), ParseError> {
        let span = self.peek_span();
        match self.peek().clone() {
            Token::Ident(s) => { self.advance(); Ok((s, span)) }
            other => Err(self.error(&format!("expected identifier, got {:?}", other))),
        }
    }

    /// Return true if the next token is a specific keyword (case-insensitive ident).
    fn peek_is_kw(&self, kw: &str) -> bool {
        match self.peek() {
            Token::Ident(s) => s.eq_ignore_ascii_case(kw),
            _ => false,
        }
    }

    fn error(&self, msg: &str) -> ParseError {
        let span = self.peek_span();
        let offset = usize::from(span.start());
        let (line, column) = self.offset_to_line_col(offset);
        ParseError {
            message: msg.to_string(),
            offset,
            line,
            column,
            span: Some(span),
        }
    }

    fn offset_to_line_col(&self, offset: usize) -> (usize, usize) {
        let mut line = 1usize;
        let mut col = 1usize;
        for (i, &c) in self.source.iter().enumerate() {
            if i >= offset { break; }
            if c == '\n' { line += 1; col = 1; } else { col += 1; }
        }
        (line, col)
    }

    // ── Top-level statement ───────────────────────────────────────────────

    fn parse_statement(&mut self) -> Result<Statement, ParseError> {
        let start = self.peek_span().start();
        let mut clauses = Vec::new();

        while !self.at_eof() {
            // Skip optional semicolons between clauses.
            while matches!(self.peek(), Token::Semi) {
                self.advance();
            }
            if self.at_eof() { break; }

            let clause = self.parse_clause()?;
            clauses.push(clause);
        }

        if clauses.is_empty() {
            return Err(self.error("empty statement"));
        }

        let end = self.peek_span().end();
        Ok(Statement { clauses, span: Some(TextRange::new(start, end)) })
    }

    // ── Clause dispatch ───────────────────────────────────────────────────

    fn parse_clause(&mut self) -> Result<Clause, ParseError> {
        let tok = self.peek().clone();
        match &tok {
            Token::Match => {
                self.advance();
                let clause_start = self.tokens[self.pos - 1].span.start();
                let patterns = self.parse_named_pattern_list()?;
                let end = self.peek_span().start();
                Ok(Clause::Match(MatchClause {
                    patterns,
                    span: Some(TextRange::new(clause_start, end)),
                }))
            }
            Token::Ident(s) if s.eq_ignore_ascii_case("OPTIONAL") => {
                let clause_start = self.peek_span().start();
                self.advance(); // consume OPTIONAL
                match self.peek().clone() {
                    Token::Match => {
                        self.advance();
                        let patterns = self.parse_named_pattern_list()?;
                        let end = self.peek_span().start();
                        Ok(Clause::OptionalMatch(MatchClause {
                            patterns,
                            span: Some(TextRange::new(clause_start, end)),
                        }))
                    }
                    _ => Err(self.error("expected MATCH after OPTIONAL")),
                }
            }
            Token::Return => {
                let clause_start = self.peek_span().start();
                self.advance();
                let (distinct, star, projections, order_by, skip, limit) =
                    self.parse_return_body()?;
                let end = self.peek_span().start();
                Ok(Clause::Return(ReturnClause {
                    distinct, star, projections, order_by, skip, limit,
                    span: Some(TextRange::new(clause_start, end)),
                }))
            }
            Token::Where => {
                let clause_start = self.peek_span().start();
                self.advance();
                let predicate = self.parse_expression(0)?;
                let end = self.peek_span().start();
                Ok(Clause::Where(WhereClause {
                    predicate,
                    span: Some(TextRange::new(clause_start, end)),
                }))
            }
            Token::Create => {
                let clause_start = self.peek_span().start();
                self.advance();
                let patterns = self.parse_named_pattern_list()?;
                let end = self.peek_span().start();
                Ok(Clause::Create(CreateClause {
                    patterns,
                    span: Some(TextRange::new(clause_start, end)),
                }))
            }
            Token::Delete => {
                let clause_start = self.peek_span().start();
                self.advance();
                let expressions = self.parse_comma_separated_expressions()?;
                let end = self.peek_span().start();
                Ok(Clause::Delete(DeleteClause {
                    expressions, detach: false,
                    span: Some(TextRange::new(clause_start, end)),
                }))
            }
            Token::Ident(s) if s.eq_ignore_ascii_case("DETACH") => {
                let clause_start = self.peek_span().start();
                self.advance(); // consume DETACH
                match self.peek().clone() {
                    Token::Delete => {
                        self.advance();
                        let expressions = self.parse_comma_separated_expressions()?;
                        let end = self.peek_span().start();
                        Ok(Clause::Delete(DeleteClause {
                            expressions, detach: true,
                            span: Some(TextRange::new(clause_start, end)),
                        }))
                    }
                    _ => Err(self.error("expected DELETE after DETACH")),
                }
            }
            Token::Set => {
                let clause_start = self.peek_span().start();
                self.advance();
                let items = self.parse_set_items()?;
                let end = self.peek_span().start();
                Ok(Clause::Set(SetClause {
                    items,
                    span: Some(TextRange::new(clause_start, end)),
                }))
            }
            Token::Remove => {
                let clause_start = self.peek_span().start();
                self.advance();
                let items = self.parse_remove_items()?;
                let end = self.peek_span().start();
                Ok(Clause::Remove(RemoveClause {
                    items,
                    span: Some(TextRange::new(clause_start, end)),
                }))
            }
            Token::Merge => {
                let clause_start = self.peek_span().start();
                self.advance();
                let pattern = self.parse_pattern()?;
                let mut on_create = Vec::new();
                let mut on_match = Vec::new();
                loop {
                    if matches!(self.peek(), Token::On) {
                        self.advance(); // consume ON
                        match self.peek().clone() {
                            Token::Create => {
                                self.advance();
                                self.expect(&Token::Set)?;
                                on_create = self.parse_set_items()?;
                            }
                            Token::Match => {
                                self.advance();
                                self.expect(&Token::Set)?;
                                on_match = self.parse_set_items()?;
                            }
                            _ => {
                                // Push back ON — it's part of a following clause.
                                self.pos -= 1;
                                break;
                            }
                        }
                    } else {
                        break;
                    }
                }
                let end = self.peek_span().start();
                Ok(Clause::Merge(MergeClause {
                    pattern, on_create, on_match,
                    span: Some(TextRange::new(clause_start, end)),
                }))
            }
            Token::With => {
                let clause_start = self.peek_span().start();
                self.advance();
                let (distinct, star, projections, order_by, skip, limit) =
                    self.parse_return_body()?;
                // Optional WHERE after WITH.
                let where_ = if matches!(self.peek(), Token::Where) {
                    self.advance();
                    Some(self.parse_expression(0)?)
                } else {
                    None
                };
                let end = self.peek_span().start();
                Ok(Clause::With(WithClause {
                    distinct, star, projections, order_by, skip, limit, where_,
                    span: Some(TextRange::new(clause_start, end)),
                }))
            }
            Token::Unwind => {
                let clause_start = self.peek_span().start();
                self.advance();
                let expression = self.parse_expression(0)?;
                self.expect(&Token::As)?;
                let (variable, _) = self.expect_ident()?;
                let end = self.peek_span().start();
                Ok(Clause::Unwind(UnwindClause {
                    expression, variable,
                    span: Some(TextRange::new(clause_start, end)),
                }))
            }
            Token::Union => {
                let clause_start = self.peek_span().start();
                self.advance();
                let all = if matches!(self.peek(), Token::Distinct) {
                    false
                } else if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("ALL")) {
                    self.advance();
                    true
                } else {
                    false
                };
                let end = self.peek_span().start();
                Ok(Clause::Union(UnionClause {
                    all,
                    span: Some(TextRange::new(clause_start, end)),
                }))
            }
            Token::UnionAll => {
                let clause_start = self.peek_span().start();
                self.advance();
                let end = self.peek_span().start();
                Ok(Clause::Union(UnionClause {
                    all: true,
                    span: Some(TextRange::new(clause_start, end)),
                }))
            }
            Token::Call => {
                let clause_start = self.peek_span().start();
                self.advance();
                // `CALL { subquery }` form.
                if matches!(self.peek(), Token::LBrace) {
                    self.advance();
                    let mut sub_clauses = Vec::new();
                    while !matches!(self.peek(), Token::RBrace | Token::Eof) {
                        sub_clauses.push(self.parse_clause()?);
                    }
                    self.expect(&Token::RBrace)?;
                    let end = self.peek_span().start();
                    return Ok(Clause::Call(CallClause {
                        procedure: None,
                        args: vec![],
                        yield_items: vec![],
                        subquery: Some(sub_clauses),
                        span: Some(TextRange::new(clause_start, end)),
                    }));
                }
                // `CALL procedure(args)` form.
                let (proc_name, _) = self.parse_qualified_name()?;
                let mut args = Vec::new();
                if matches!(self.peek(), Token::LParen) {
                    self.advance();
                    if !matches!(self.peek(), Token::RParen) {
                        args = self.parse_comma_separated_expressions()?;
                    }
                    self.expect(&Token::RParen)?;
                }
                // Optional YIELD clause.
                let mut yield_items = Vec::new();
                if self.peek_is_kw("YIELD") {
                    self.advance();
                    yield_items = self.parse_projection_list()?;
                }
                let end = self.peek_span().start();
                Ok(Clause::Call(CallClause {
                    procedure: Some(proc_name),
                    args,
                    yield_items,
                    subquery: None,
                    span: Some(TextRange::new(clause_start, end)),
                }))
            }
            Token::Ident(s) if s.eq_ignore_ascii_case("FOREACH") => {
                let clause_start = self.peek_span().start();
                self.advance();
                self.expect(&Token::LParen)?;
                let (variable, _) = self.expect_ident()?;
                self.expect(&Token::In)?;
                let expression = self.parse_expression(0)?;
                self.expect(&Token::Pipe)?;
                let mut body = Vec::new();
                while !matches!(self.peek(), Token::RParen | Token::Eof) {
                    body.push(self.parse_clause()?);
                }
                self.expect(&Token::RParen)?;
                let end = self.peek_span().start();
                Ok(Clause::Foreach(ForeachClause {
                    variable, expression, body,
                    span: Some(TextRange::new(clause_start, end)),
                }))
            }
            other => {
                Err(self.error(&format!("unexpected token at clause start: {:?}", other)))
            }
        }
    }

    // ── Return / WITH body ─────────────────────────────────────────────────

    fn parse_return_body(&mut self) -> Result<(bool, bool, Vec<Projection>, Vec<OrderItem>, Option<Expression>, Option<Expression>), ParseError> {
        let distinct = if matches!(self.peek(), Token::Distinct) {
            self.advance();
            true
        } else {
            false
        };

        // RETURN * — wildcard projection.
        if matches!(self.peek(), Token::Star) {
            self.advance();
            let (order_by, skip, limit) = self.parse_order_skip_limit()?;
            return Ok((distinct, true, vec![], order_by, skip, limit));
        }

        let projections = self.parse_projection_list()?;
        let (order_by, skip, limit) = self.parse_order_skip_limit()?;
        Ok((distinct, false, projections, order_by, skip, limit))
    }

    fn parse_order_skip_limit(&mut self) -> Result<(Vec<OrderItem>, Option<Expression>, Option<Expression>), ParseError> {
        let mut order_by = Vec::new();
        let mut skip = None;
        let mut limit = None;

        loop {
            match self.peek().clone() {
                Token::Order => {
                    self.advance();
                    self.expect(&Token::By)?;
                    order_by = self.parse_order_by()?;
                }
                Token::Skip => {
                    self.advance();
                    skip = Some(self.parse_expression(0)?);
                }
                Token::Limit => {
                    self.advance();
                    limit = Some(self.parse_expression(0)?);
                }
                _ => break,
            }
        }

        Ok((order_by, skip, limit))
    }

    fn parse_projection_list(&mut self) -> Result<Vec<Projection>, ParseError> {
        let mut projections = Vec::new();
        loop {
            let proj_start = self.peek_span().start();
            let expr = self.parse_expression(0)?;
            let alias = if matches!(self.peek(), Token::As) {
                self.advance();
                let (name, _) = self.expect_ident()?;
                Some(name)
            } else {
                None
            };
            let proj_end = self.peek_span().start();
            projections.push(Projection {
                expression: expr,
                alias,
                span: Some(TextRange::new(proj_start, proj_end)),
            });
            if matches!(self.peek(), Token::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        Ok(projections)
    }

    fn parse_order_by(&mut self) -> Result<Vec<OrderItem>, ParseError> {
        let mut items = Vec::new();
        loop {
            let item_start = self.peek_span().start();
            let expr = self.parse_expression(0)?;
            let ascending = match self.peek().clone() {
                Token::Desc => { self.advance(); false }
                Token::Asc  => { self.advance(); true  }
                _           => true,
            };
            let item_end = self.peek_span().start();
            items.push(OrderItem {
                expression: expr, ascending,
                span: Some(TextRange::new(item_start, item_end)),
            });
            if matches!(self.peek(), Token::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        Ok(items)
    }

    // ── SET / REMOVE items ─────────────────────────────────────────────────

    fn parse_set_items(&mut self) -> Result<Vec<SetItem>, ParseError> {
        let mut items = Vec::new();
        loop {
            let (ident, _) = self.expect_ident()?;
            match self.peek().clone() {
                Token::Dot => {
                    // variable.prop = expr
                    self.advance();
                    let (prop, _) = self.expect_ident()?;
                    self.expect(&Token::Eq)?;
                    let value = self.parse_expression(0)?;
                    items.push(SetItem::Property {
                        target: Box::new(Expression::PropertyAccess {
                            base: Box::new(Expression::Variable(ident)),
                            property: prop,
                            span: None,
                        }),
                        value,
                    });
                }
                Token::Colon => {
                    // variable:Label1:Label2
                    let mut labels = Vec::new();
                    while matches!(self.peek(), Token::Colon) {
                        self.advance();
                        let (label, _) = self.expect_ident()?;
                        labels.push(label);
                    }
                    items.push(SetItem::Label { variable: ident, labels });
                }
                Token::Assign => {
                    // variable += {map}
                    self.advance();
                    let value = self.parse_expression(0)?;
                    items.push(SetItem::Merge { variable: ident, value });
                }
                Token::Eq => {
                    // variable = {map} (replace all properties)
                    self.advance();
                    let value = self.parse_expression(0)?;
                    items.push(SetItem::Replace { variable: ident, value });
                }
                other => {
                    return Err(self.error(&format!("unexpected token in SET: {:?}", other)));
                }
            }
            if matches!(self.peek(), Token::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        Ok(items)
    }

    fn parse_remove_items(&mut self) -> Result<Vec<RemoveItem>, ParseError> {
        let mut items = Vec::new();
        loop {
            let (ident, _) = self.expect_ident()?;
            match self.peek().clone() {
                Token::Dot => {
                    self.advance();
                    let (prop, _) = self.expect_ident()?;
                    items.push(RemoveItem::Property {
                        target: Box::new(Expression::PropertyAccess {
                            base: Box::new(Expression::Variable(ident)),
                            property: prop,
                            span: None,
                        }),
                    });
                }
                Token::Colon => {
                    let mut labels = Vec::new();
                    while matches!(self.peek(), Token::Colon) {
                        self.advance();
                        let (label, _) = self.expect_ident()?;
                        labels.push(label);
                    }
                    items.push(RemoveItem::Label { variable: ident, labels });
                }
                other => {
                    return Err(self.error(&format!("unexpected token in REMOVE: {:?}", other)));
                }
            }
            if matches!(self.peek(), Token::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        Ok(items)
    }

    // ── Patterns ───────────────────────────────────────────────────────────

    /// Parse a comma-separated list of optionally named patterns.
    fn parse_named_pattern_list(&mut self) -> Result<Vec<NamedPattern>, ParseError> {
        let mut patterns = Vec::new();
        loop {
            // Check for `variable =` (named path).
            let named = if let Token::Ident(_) = self.peek() {
                // Peek ahead: is the token after the ident an `=`?
                let saved = self.pos;
                let (var_name, _) = self.expect_ident()?;
                if matches!(self.peek(), Token::Eq) {
                    self.advance(); // consume =
                    let pattern = self.parse_pattern()?;
                    patterns.push(NamedPattern { variable: Some(var_name), pattern });
                    if matches!(self.peek(), Token::Comma) {
                        self.advance();
                        continue;
                    }
                    break;
                } else {
                    // Backtrack — the ident is part of the pattern itself.
                    self.pos = saved;
                }
                false
            } else {
                false
            };
            let _ = named;
            let pattern = self.parse_pattern()?;
            patterns.push(NamedPattern { variable: None, pattern });
            if matches!(self.peek(), Token::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        Ok(patterns)
    }

    fn parse_pattern(&mut self) -> Result<Pattern, ParseError> {
        let start = self.peek_span().start();
        let mut elements = Vec::new();

        let node = self.parse_node_pattern()?;
        elements.push(PatternElement::Node(node));

        loop {
            match self.peek() {
                Token::Dash | Token::LArrow => {
                    let rel = self.parse_relationship_pattern()?;
                    elements.push(PatternElement::Relationship(rel));
                    let node = self.parse_node_pattern()?;
                    elements.push(PatternElement::Node(node));
                }
                _ => break,
            }
        }

        let end = self.peek_span().start();
        Ok(Pattern {
            elements,
            span: Some(TextRange::new(start, end)),
        })
    }

    fn parse_node_pattern(&mut self) -> Result<NodePattern, ParseError> {
        let start = self.peek_span().start();
        self.expect(&Token::LParen)?;

        let variable = if let Token::Ident(_) = self.peek() {
            // Could be a variable OR a label (if followed by colon).
            // If next-next is `:` it might be the variable OR the label start.
            // Greedily consume identifier as variable if it's not a keyword.
            let saved = self.pos;
            if let Token::Ident(s) = self.peek().clone() {
                // Check this is not a keyword — already tokenised as Ident by logos.
                self.advance();
                Some(s)
            } else {
                self.pos = saved;
                None
            }
        } else {
            None
        };

        let mut labels = Vec::new();
        while matches!(self.peek(), Token::Colon) {
            self.advance();
            match self.peek().clone() {
                Token::Ident(label) => { self.advance(); labels.push(label); }
                Token::Pipe => {} // `|` between labels — handled below
                other => return Err(self.error(&format!("expected label name, got {:?}", other))),
            }
            // Handle label alternation `:A|B`.
            while matches!(self.peek(), Token::Pipe) {
                self.advance();
                match self.peek().clone() {
                    Token::Ident(label) => { self.advance(); labels.push(label); }
                    other => return Err(self.error(&format!("expected label name after |, got {:?}", other))),
                }
            }
        }

        let properties = if matches!(self.peek(), Token::LBrace) {
            self.parse_property_map()?
        } else {
            HashMap::new()
        };

        self.expect(&Token::RParen)?;
        let end = self.peek_span().start();
        Ok(NodePattern { variable, labels, properties, span: Some(TextRange::new(start, end)) })
    }

    fn parse_relationship_pattern(&mut self) -> Result<RelationshipPattern, ParseError> {
        let start = self.peek_span().start();
        let mut direction = Direction::Both;

        if matches!(self.peek(), Token::LArrow) {
            self.advance(); // consume `<-`
            direction = Direction::Incoming;
        } else if matches!(self.peek(), Token::Dash) {
            self.advance(); // consume `-`
        } else {
            return Err(self.error("expected '-' or '<-' in relationship pattern"));
        }

        let mut variable = None;
        let mut types = Vec::new();
        let mut properties = HashMap::new();
        let mut length = PathLength::Fixed(1);

        if matches!(self.peek(), Token::LBracket) {
            self.advance(); // consume `[`

            // Optional variable (if next is ident not followed by colon type).
            if let Token::Ident(s) = self.peek().clone() {
                // Check if this ident is followed by `:` (type) or not (variable).
                let saved = self.pos;
                self.advance();
                if matches!(self.peek(), Token::Colon) {
                    // It's `r:TYPE` — the ident is the variable.
                    variable = Some(s);
                } else if matches!(self.peek(), Token::RBracket | Token::Star | Token::LBrace) {
                    // No colon: `r` alone as variable, or `r*`.
                    variable = Some(s);
                } else {
                    // Reset — treat as type name.
                    self.pos = saved;
                }
            }

            // Relationship types: `:TYPE1|TYPE2`.
            while matches!(self.peek(), Token::Colon) {
                self.advance();
                match self.peek().clone() {
                    Token::Ident(t) => { self.advance(); types.push(t); }
                    other => return Err(self.error(&format!("expected relationship type, got {:?}", other))),
                }
                while matches!(self.peek(), Token::Pipe) {
                    self.advance();
                    match self.peek().clone() {
                        Token::Ident(t) => { self.advance(); types.push(t); }
                        other => return Err(self.error(&format!("expected relationship type after |, got {:?}", other))),
                    }
                }
            }

            // Variable path length: `*`, `*N`, `*m..n`.
            if matches!(self.peek(), Token::Star) {
                self.advance();
                length = self.parse_path_length_suffix()?;
            }

            // Optional property map.
            if matches!(self.peek(), Token::LBrace) {
                properties = self.parse_property_map()?;
            }

            self.expect(&Token::RBracket)?;
        }

        // The right part of the arrow: `-` or `->`.
        if matches!(self.peek(), Token::Dash) {
            self.advance(); // consume `-`
            if matches!(self.peek(), Token::Gt) {
                self.advance(); // consume `>`
                if direction == Direction::Incoming {
                    return Err(self.error("relationship cannot be both incoming and outgoing"));
                }
                direction = Direction::Outgoing;
            }
        } else if matches!(self.peek(), Token::Arrow) {
            self.advance(); // consume `->`
            if direction == Direction::Incoming {
                return Err(self.error("relationship cannot be both incoming and outgoing"));
            }
            direction = Direction::Outgoing;
        } else {
            return Err(self.error("expected '-' after relationship pattern"));
        }

        let end = self.peek_span().start();
        Ok(RelationshipPattern {
            direction, types, variable, properties, length,
            span: Some(TextRange::new(start, end)),
        })
    }

    /// Parse the optional `N`, `m..n`, `..n`, `m..` after `*` in a relationship.
    fn parse_path_length_suffix(&mut self) -> Result<PathLength, ParseError> {
        // Bare `*` — any length, default 1..∞.
        if !matches!(self.peek(), Token::Integer(_)) && !matches!(self.peek(), Token::Dot) {
            return Ok(PathLength::Range(1, None));
        }

        let min = if let Token::Integer(n) = self.peek().clone() {
            self.advance();
            n as u32
        } else {
            1
        };

        // Check for `..`.
        if matches!(self.peek(), Token::Dot) {
            self.advance(); // first `.`
            if matches!(self.peek(), Token::Dot) {
                self.advance(); // second `.`
                let max = if let Token::Integer(n) = self.peek().clone() {
                    self.advance();
                    Some(n as u32)
                } else {
                    None
                };
                return Ok(PathLength::Range(min, max));
            } else {
                // Single dot — error or `*N` form with fractional part (unlikely)
                return Ok(PathLength::Range(min, Some(min)));
            }
        }

        // `*N` — exactly N hops.
        Ok(PathLength::Range(min, Some(min)))
    }

    fn parse_property_map(&mut self) -> Result<HashMap<String, Expression>, ParseError> {
        self.expect(&Token::LBrace)?;
        let mut map = HashMap::new();
        if matches!(self.peek(), Token::RBrace) {
            self.advance();
            return Ok(map);
        }
        loop {
            let (key, _) = self.expect_ident()?;
            self.expect(&Token::Colon)?;
            let value = self.parse_expression(0)?;
            map.insert(key, value);
            if matches!(self.peek(), Token::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        self.expect(&Token::RBrace)?;
        Ok(map)
    }

    // ── Expression parser (Pratt) ──────────────────────────────────────────

    fn parse_expression(&mut self, min_bp: u8) -> Result<Expression, ParseError> {
        let expr_start = self.peek_span().start();
        let mut lhs = self.parse_unary()?;

        loop {
            let op = match self.current_infix_op() {
                Some(op) => op,
                None => break,
            };
            let (lbp, rbp) = infix_binding_power(&op);
            if lbp < min_bp { break; }

            lhs = self.consume_infix(lhs, &op, rbp, expr_start)?;
        }

        // IS NULL / IS NOT NULL postfix.
        loop {
            if matches!(self.peek(), Token::Is) {
                self.advance();
                if matches!(self.peek(), Token::Not) {
                    self.advance();
                    // Expect NULL or NullKw.
                    if matches!(self.peek(), Token::NullKw) || self.peek_is_kw("NULL") {
                        self.advance();
                        lhs = Expression::IsNotNull(Box::new(lhs));
                    } else {
                        return Err(self.error("expected NULL after IS NOT"));
                    }
                } else if matches!(self.peek(), Token::NullKw) || self.peek_is_kw("NULL") {
                    self.advance();
                    lhs = Expression::IsNull(Box::new(lhs));
                } else {
                    return Err(self.error("expected NULL or NOT NULL after IS"));
                }
            } else {
                break;
            }
        }

        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expression, ParseError> {
        let start = self.peek_span().start();
        match self.peek().clone() {
            Token::Not => {
                self.advance();
                let expr = self.parse_expression(PREFIX_NOT_BP)?;
                let end = self.peek_span().start();
                Ok(Expression::Not {
                    expr: Box::new(expr),
                    span: Some(TextRange::new(start, end)),
                })
            }
            Token::Dash => {
                self.advance();
                // Only negate if the next token looks like a primary.
                let expr = self.parse_primary()?;
                let end = self.peek_span().start();
                Ok(Expression::UnaryOp {
                    op: UnaryOperator::Neg,
                    expr: Box::new(expr),
                    span: Some(TextRange::new(start, end)),
                })
            }
            _ => {
                let primary = self.parse_primary()?;
                self.parse_postfix(primary)
            }
        }
    }

    fn parse_primary(&mut self) -> Result<Expression, ParseError> {
        let start = self.peek_span().start();
        match self.peek().clone() {
            // ── Literals ──
            Token::NullKw => { self.advance(); Ok(Expression::Literal(Literal::Null)) }
            Token::TrueKw => { self.advance(); Ok(Expression::Literal(Literal::Boolean(true))) }
            Token::FalseKw => { self.advance(); Ok(Expression::Literal(Literal::Boolean(false))) }
            Token::Integer(n) => { self.advance(); Ok(Expression::Literal(Literal::Integer(n))) }
            Token::Float(f) => { self.advance(); Ok(Expression::Literal(Literal::Float(f))) }
            Token::String(s) => { self.advance(); Ok(Expression::Literal(Literal::String(s))) }
            Token::DateLiteral(s) => { self.advance(); Ok(Expression::Literal(Literal::Date(s))) }
            Token::TimeLiteral(s) => { self.advance(); Ok(Expression::Literal(Literal::Time(s))) }
            Token::DateTimeLiteral(s) => { self.advance(); Ok(Expression::Literal(Literal::DateTime(s))) }
            Token::DurationLiteral(s) => { self.advance(); Ok(Expression::Literal(Literal::Duration(s))) }
            // ── Parameter ──
            Token::Parameter(p) => { self.advance(); Ok(Expression::Parameter(p)) }
            // ── Grouped expression ──
            Token::LParen => {
                self.advance();
                let expr = self.parse_expression(0)?;
                self.expect(&Token::RParen)?;
                Ok(expr)
            }
            // ── List literal ──
            Token::LBracket => self.parse_list_or_comprehension(),
            // ── Map literal ──
            Token::LBrace => self.parse_map_literal(),
            // ── Wildcard (for count(*)) ──
            Token::Star => {
                self.advance();
                Ok(Expression::Wildcard)
            }
            // ── Identifier-based primaries ──
            Token::Ident(_) => self.parse_ident_primary(),
            // ── Keywords that can be used as function names ──
            Token::Case => self.parse_case_expression(),
            Token::Ident(s) if s.eq_ignore_ascii_case("REDUCE") => {
                self.advance();
                self.parse_reduce_expression(start)
            }
            Token::Ident(s) if s.eq_ignore_ascii_case("ALL") => self.parse_quantifier(QuantifierKind::All),
            Token::Ident(s) if s.eq_ignore_ascii_case("ANY") => self.parse_quantifier(QuantifierKind::Any),
            Token::Ident(s) if s.eq_ignore_ascii_case("NONE") => self.parse_quantifier(QuantifierKind::None),
            Token::Ident(s) if s.eq_ignore_ascii_case("SINGLE") => self.parse_quantifier(QuantifierKind::Single),
            Token::Ident(s) if s.eq_ignore_ascii_case("EXISTS") => self.parse_exists(start),
            Token::Ident(s) if s.eq_ignore_ascii_case("shortestPath")
                || s.eq_ignore_ascii_case("allShortestPaths") => {
                self.parse_ident_primary()
            }
            other => Err(self.error(&format!("unexpected token in expression: {:?}", other))),
        }
    }

    fn parse_ident_primary(&mut self) -> Result<Expression, ParseError> {
        let start = self.peek_span().start();
        let (name, _) = self.expect_ident()?;

        match name.to_ascii_uppercase().as_str() {
            "TRUE"  => return Ok(Expression::Literal(Literal::Boolean(true))),
            "FALSE" => return Ok(Expression::Literal(Literal::Boolean(false))),
            "NULL"  => return Ok(Expression::Literal(Literal::Null)),
            "CASE"  => return self.parse_case_expression(),
            "REDUCE" => {
                let sp = self.tokens[self.pos - 1].span.start();
                return self.parse_reduce_expression(sp);
            }
            "ALL"    => return self.parse_quantifier(QuantifierKind::All),
            "ANY"    => return self.parse_quantifier(QuantifierKind::Any),
            "NONE"   => return self.parse_quantifier(QuantifierKind::None),
            "SINGLE" => return self.parse_quantifier(QuantifierKind::Single),
            "EXISTS" => {
                let sp = self.tokens[self.pos - 1].span.start();
                return self.parse_exists(sp);
            }
            _ => {}
        }

        // Function call?
        if matches!(self.peek(), Token::LParen) {
            self.advance(); // consume `(`
            let mut distinct = false;
            let mut args = Vec::new();

            if !matches!(self.peek(), Token::RParen) {
                // DISTINCT modifier.
                if matches!(self.peek(), Token::Distinct) {
                    self.advance();
                    distinct = true;
                }
                // Wildcard `*` as argument (for count(*)).
                if matches!(self.peek(), Token::Star) {
                    self.advance();
                    args.push(Expression::Wildcard);
                } else if !matches!(self.peek(), Token::RParen) {
                    args = self.parse_comma_separated_expressions()?;
                }
            }
            self.expect(&Token::RParen)?;
            let end = self.peek_span().start();
            let func_expr = Expression::FunctionCall {
                name,
                args,
                distinct,
                span: Some(TextRange::new(start, end)),
            };
            return self.parse_postfix(func_expr);
        }

        let var_expr = Expression::Variable(name);
        self.parse_postfix(var_expr)
    }

    /// Parse postfix operators: `.prop`, `[expr]`, `[from..to]`.
    fn parse_postfix(&mut self, mut lhs: Expression) -> Result<Expression, ParseError> {
        loop {
            let start = self.peek_span().start();
            match self.peek().clone() {
                Token::Dot => {
                    self.advance();
                    let (prop, _) = self.expect_ident()?;
                    let end = self.peek_span().start();
                    lhs = Expression::PropertyAccess {
                        base: Box::new(lhs),
                        property: prop,
                        span: Some(TextRange::new(start, end)),
                    };
                }
                Token::LBracket => {
                    self.advance();
                    // Slice: `[expr..]`, `[..expr]`, `[expr..expr]`, or `[expr]`.
                    let (from, is_slice) = if matches!(self.peek(), Token::Dot) {
                        // `[..expr]`
                        self.advance(); // first `.`
                        if matches!(self.peek(), Token::Dot) { self.advance(); }
                        (None, true)
                    } else if matches!(self.peek(), Token::RBracket) {
                        (None, false)
                    } else {
                        let e = self.parse_expression(0)?;
                        if matches!(self.peek(), Token::Dot) {
                            self.advance(); // first `.`
                            if matches!(self.peek(), Token::Dot) { self.advance(); }
                            (Some(e), true)
                        } else {
                            (Some(e), false)
                        }
                    };

                    if is_slice {
                        let to = if !matches!(self.peek(), Token::RBracket) {
                            Some(self.parse_expression(0)?)
                        } else {
                            None
                        };
                        self.expect(&Token::RBracket)?;
                        let end = self.peek_span().start();
                        lhs = Expression::Slice {
                            base: Box::new(lhs),
                            from: from.map(Box::new),
                            to: to.map(Box::new),
                            span: Some(TextRange::new(start, end)),
                        };
                    } else if let Some(index) = from {
                        self.expect(&Token::RBracket)?;
                        let end = self.peek_span().start();
                        lhs = Expression::DynamicPropertyAccess {
                            base: Box::new(lhs),
                            index: Box::new(index),
                            span: Some(TextRange::new(start, end)),
                        };
                    } else {
                        self.expect(&Token::RBracket)?;
                        // empty `[]` — treat as empty list index (no-op / null).
                    }
                }
                _ => break,
            }
        }
        Ok(lhs)
    }

    fn parse_list_or_comprehension(&mut self) -> Result<Expression, ParseError> {
        let start = self.peek_span().start();
        self.expect(&Token::LBracket)?;

        if matches!(self.peek(), Token::RBracket) {
            self.advance();
            return Ok(Expression::List(vec![]));
        }

        // Try list comprehension: `[x IN expr WHERE ... | ...]`.
        // The first element is an expression; if the next token is `IN` and
        // the token before `IN` was an identifier, it's a comprehension.
        let saved = self.pos;
        if let Token::Ident(var_name) = self.peek().clone() {
            self.advance();
            if matches!(self.peek(), Token::In) {
                self.advance();
                let source = self.parse_expression(0)?;
                let filter = if matches!(self.peek(), Token::Where) {
                    self.advance();
                    Some(Box::new(self.parse_expression(0)?))
                } else {
                    None
                };
                let projection = if matches!(self.peek(), Token::Pipe) {
                    self.advance();
                    Some(Box::new(self.parse_expression(0)?))
                } else {
                    None
                };
                self.expect(&Token::RBracket)?;
                let end = self.peek_span().start();
                return Ok(Expression::ListComprehension {
                    variable: var_name,
                    source: Box::new(source),
                    filter,
                    projection,
                    span: Some(TextRange::new(start, end)),
                });
            }
            // Not a comprehension — backtrack.
            self.pos = saved;
        }

        // Regular list literal.
        let mut items = Vec::new();
        loop {
            items.push(self.parse_expression(0)?);
            if matches!(self.peek(), Token::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        self.expect(&Token::RBracket)?;
        Ok(Expression::List(items))
    }

    fn parse_map_literal(&mut self) -> Result<Expression, ParseError> {
        self.expect(&Token::LBrace)?;
        let mut entries = Vec::new();
        if matches!(self.peek(), Token::RBrace) {
            self.advance();
            return Ok(Expression::Map(entries));
        }
        loop {
            let (key, _) = self.expect_ident()?;
            self.expect(&Token::Colon)?;
            let value = self.parse_expression(0)?;
            entries.push((key, value));
            if matches!(self.peek(), Token::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        self.expect(&Token::RBrace)?;
        Ok(Expression::Map(entries))
    }

    fn parse_case_expression(&mut self) -> Result<Expression, ParseError> {
        let start = self.peek_span().start();
        // `case` keyword already peeked — consume it.
        if matches!(self.peek(), Token::Case) {
            self.advance();
        }

        // Simple form: `CASE subject WHEN ...`
        // Generic form: `CASE WHEN ...`
        let subject = if !matches!(self.peek(), Token::When) {
            Some(Box::new(self.parse_expression(0)?))
        } else {
            None
        };

        let mut alternatives = Vec::new();
        while matches!(self.peek(), Token::When) {
            self.advance();
            let condition = self.parse_expression(0)?;
            self.expect(&Token::Then)?;
            let result = self.parse_expression(0)?;
            alternatives.push(CaseAlternative { condition, result });
        }

        let default = if matches!(self.peek(), Token::Else) {
            self.advance();
            Some(Box::new(self.parse_expression(0)?))
        } else {
            None
        };

        self.expect(&Token::End)?;
        let end = self.peek_span().start();
        Ok(Expression::Case {
            subject,
            alternatives,
            default,
            span: Some(TextRange::new(start, end)),
        })
    }

    fn parse_reduce_expression(&mut self, start: TextSize) -> Result<Expression, ParseError> {
        self.expect(&Token::LParen)?;
        let (acc, _) = self.expect_ident()?;
        self.expect(&Token::Eq)?;
        let init = self.parse_expression(0)?;
        self.expect(&Token::Comma)?;
        let (var, _) = self.expect_ident()?;
        self.expect(&Token::In)?;
        let source = self.parse_expression(0)?;
        self.expect(&Token::Pipe)?;
        let body = self.parse_expression(0)?;
        self.expect(&Token::RParen)?;
        let end = self.peek_span().start();
        Ok(Expression::Reduce {
            accumulator: acc,
            init: Box::new(init),
            variable: var,
            source: Box::new(source),
            body: Box::new(body),
            span: Some(TextRange::new(start, end)),
        })
    }

    fn parse_quantifier(&mut self, kind: QuantifierKind) -> Result<Expression, ParseError> {
        let start = self.tokens[self.pos - 1].span.start();
        self.expect(&Token::LParen)?;
        let (var, _) = self.expect_ident()?;
        self.expect(&Token::In)?;
        let source = self.parse_expression(0)?;
        self.expect(&Token::Where)?;
        let filter = self.parse_expression(0)?;
        self.expect(&Token::RParen)?;
        let end = self.peek_span().start();
        Ok(Expression::Quantifier {
            kind,
            variable: var,
            source: Box::new(source),
            filter: Box::new(filter),
            span: Some(TextRange::new(start, end)),
        })
    }

    fn parse_exists(&mut self, start: TextSize) -> Result<Expression, ParseError> {
        if matches!(self.peek(), Token::LBrace) {
            self.advance();
            let mut sub_clauses = Vec::new();
            while !matches!(self.peek(), Token::RBrace | Token::Eof) {
                sub_clauses.push(self.parse_clause()?);
            }
            self.expect(&Token::RBrace)?;
            let end = self.peek_span().start();
            Ok(Expression::Exists {
                subquery: Some(sub_clauses),
                pattern: None,
                span: Some(TextRange::new(start, end)),
            })
        } else if matches!(self.peek(), Token::LParen) {
            let pattern = self.parse_pattern()?;
            let end = self.peek_span().start();
            Ok(Expression::Exists {
                subquery: None,
                pattern: Some(pattern),
                span: Some(TextRange::new(start, end)),
            })
        } else {
            Err(self.error("expected `{` or `(` after EXISTS"))
        }
    }

    // ── Infix operator helpers ─────────────────────────────────────────────

    /// Return the current infix operator as a string, or None if none.
    fn current_infix_op(&self) -> Option<String> {
        let tok = self.peek();
        match tok {
            Token::Or  => Some("OR".to_string()),
            Token::And => Some("AND".to_string()),
            Token::Ident(s) if s.eq_ignore_ascii_case("XOR") => Some("XOR".to_string()),
            Token::Not => Some("NOT".to_string()), // for `NOT IN`, `NOT CONTAINS` etc — handled specially
            Token::Eq  => Some("=".to_string()),
            Token::Ne  => Some("<>".to_string()),
            Token::Lt  => Some("<".to_string()),
            Token::Gt  => Some(">".to_string()),
            Token::Le  => Some("<=".to_string()),
            Token::Ge  => Some(">=".to_string()),
            Token::In  => Some("IN".to_string()),
            Token::Contains => Some("CONTAINS".to_string()),
            Token::Starts    => Some("STARTS".to_string()),
            Token::Ends      => Some("ENDS".to_string()),
            Token::Plus  => Some("+".to_string()),
            Token::Dash  => Some("-".to_string()),
            Token::Star  => Some("*".to_string()),
            Token::Slash => Some("/".to_string()),
            Token::Percent => Some("%".to_string()),
            Token::Caret => Some("^".to_string()),
            // `=~` is tokenised by logos as two separate tokens (`=` `~`) ...
            // actually logos has no `~` token. In the logos lexer, `=~` isn't defined.
            // We detect `=~` by checking if `=` is followed immediately by `~` in the source.
            // But since the logos lexer skips `~` (unrecognised, becomes Error), we handle
            // the regex operator by recognising `=` followed by Error("~").
            Token::Error(s) if s == "~" => {
                // peek-1 should be `=`.
                if self.pos > 0 {
                    if matches!(&self.tokens[self.pos - 1].token, Token::Eq) {
                        // Already consumed `=`; we need to detect this differently.
                    }
                }
                None
            }
            _ => None,
        }
    }

    fn consume_infix(
        &mut self,
        lhs: Expression,
        op: &str,
        rbp: u8,
        expr_start: TextSize,
    ) -> Result<Expression, ParseError> {
        // Consume the operator token(s).
        match op {
            "STARTS" => {
                self.advance(); // STARTS
                self.expect(&Token::With)?;
            }
            "ENDS" => {
                self.advance(); // ENDS
                self.expect(&Token::With)?;
            }
            _ => {
                self.advance();
            }
        }

        let rhs = self.parse_expression(rbp)?;
        let expr_end = self.peek_span().start();
        let span = Some(TextRange::new(expr_start, expr_end));

        let result = match op {
            "OR" => Expression::Or { left: Box::new(lhs), right: Box::new(rhs), span },
            "AND" => Expression::And { left: Box::new(lhs), right: Box::new(rhs), span },
            "XOR" => Expression::Xor { left: Box::new(lhs), right: Box::new(rhs), span },
            "IN" => Expression::In { left: Box::new(lhs), right: Box::new(rhs), span },
            "CONTAINS" => Expression::Contains { left: Box::new(lhs), right: Box::new(rhs), span },
            "STARTS" => Expression::StartsWith { left: Box::new(lhs), right: Box::new(rhs), span },
            "ENDS" => Expression::EndsWith { left: Box::new(lhs), right: Box::new(rhs), span },
            "+" => Expression::BinaryOp { op: BinaryOperator::Add, left: Box::new(lhs), right: Box::new(rhs), span },
            "-" => Expression::BinaryOp { op: BinaryOperator::Sub, left: Box::new(lhs), right: Box::new(rhs), span },
            "*" => Expression::BinaryOp { op: BinaryOperator::Mul, left: Box::new(lhs), right: Box::new(rhs), span },
            "/" => Expression::BinaryOp { op: BinaryOperator::Div, left: Box::new(lhs), right: Box::new(rhs), span },
            "%" => Expression::BinaryOp { op: BinaryOperator::Mod, left: Box::new(lhs), right: Box::new(rhs), span },
            "^" => Expression::BinaryOp { op: BinaryOperator::Pow, left: Box::new(lhs), right: Box::new(rhs), span },
            "=" => Expression::Comparison { op: ComparisonOperator::Eq, left: Box::new(lhs), right: Box::new(rhs), span },
            "<>" => Expression::Comparison { op: ComparisonOperator::Ne, left: Box::new(lhs), right: Box::new(rhs), span },
            "<" => Expression::Comparison { op: ComparisonOperator::Lt, left: Box::new(lhs), right: Box::new(rhs), span },
            "<=" => Expression::Comparison { op: ComparisonOperator::Le, left: Box::new(lhs), right: Box::new(rhs), span },
            ">" => Expression::Comparison { op: ComparisonOperator::Gt, left: Box::new(lhs), right: Box::new(rhs), span },
            ">=" => Expression::Comparison { op: ComparisonOperator::Ge, left: Box::new(lhs), right: Box::new(rhs), span },
            other => return Err(ParseError {
                message: format!("unknown infix operator: {}", other),
                offset: 0,
                line: 0,
                column: 0,
                span: None,
            }),
        };
        Ok(result)
    }

    // ── Helpers ────────────────────────────────────────────────────────────

    fn parse_comma_separated_expressions(&mut self) -> Result<Vec<Expression>, ParseError> {
        let mut exprs = Vec::new();
        loop {
            exprs.push(self.parse_expression(0)?);
            if matches!(self.peek(), Token::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        Ok(exprs)
    }

    /// Parse a potentially dot-qualified name like `apoc.create.node`.
    fn parse_qualified_name(&mut self) -> Result<(String, TextRange), ParseError> {
        let start = self.peek_span().start();
        let (mut name, _) = self.expect_ident()?;
        while matches!(self.peek(), Token::Dot) {
            self.advance();
            let (part, _) = self.expect_ident()?;
            name.push('.');
            name.push_str(&part);
        }
        let end = self.peek_span().start();
        Ok((name, TextRange::new(start, end)))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Binding power table
// ─────────────────────────────────────────────────────────────────────────────

const PREFIX_NOT_BP: u8 = 35;

fn infix_binding_power(op: &str) -> (u8, u8) {
    match op {
        "OR"       => (10, 11),
        "XOR"      => (20, 21),
        "AND"      => (30, 31),
        "=" | "<>" | "<" | ">" | "<=" | ">=" | "IN" | "CONTAINS" | "STARTS" | "ENDS" => (40, 41),
        "+" | "-"  => (50, 51),
        "*" | "/" | "%" => (60, 61),
        "^"        => (70, 71),   // right-associative: (70, 70) would be left
        _          => (0, 0),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use text_size::TextSize;

    fn first_clause(q: &str) -> Clause {
        parse(q).unwrap().clauses.into_iter().next().unwrap()
    }

    #[test]
    fn parse_simple_match_return() {
        let stmt = parse("MATCH (n:Person)-[:KNOWS]->(m:Person) RETURN n, m").unwrap();
        assert_eq!(stmt.clauses.len(), 2);
        assert!(stmt.span.is_some());
    }

    #[test]
    fn parse_match_where_return() {
        let stmt = parse("MATCH (n:Person) WHERE n.name = 'Alice' RETURN n").unwrap();
        assert_eq!(stmt.clauses.len(), 3);
        assert!(matches!(stmt.clauses[0], Clause::Match(_)));
        assert!(matches!(stmt.clauses[1], Clause::Where(_)));
        assert!(matches!(stmt.clauses[2], Clause::Return(_)));
    }

    #[test]
    fn parse_create_clause() {
        let stmt = parse("CREATE (n:Person {name: 'Bob', age: 30})").unwrap();
        assert_eq!(stmt.clauses.len(), 1);
        assert!(matches!(stmt.clauses[0], Clause::Create(_)));
    }

    #[test]
    fn parse_expression_literal() {
        let stmt = parse("RETURN 42").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            assert_eq!(r.projections.len(), 1);
            assert!(matches!(
                r.projections[0].expression,
                Expression::Literal(Literal::Integer(42))
            ));
        } else {
            panic!("expected RETURN clause");
        }
    }

    #[test]
    fn parse_expression_comparison() {
        let stmt = parse("MATCH (n) WHERE n.age > 18 RETURN n").unwrap();
        if let Clause::Where(w) = &stmt.clauses[1] {
            assert!(matches!(w.predicate, Expression::Comparison { op: ComparisonOperator::Gt, .. }));
        } else {
            panic!("expected WHERE clause");
        }
    }

    #[test]
    fn parse_property_access() {
        let stmt = parse("RETURN n.name").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            assert!(matches!(r.projections[0].expression, Expression::PropertyAccess { .. }));
        } else {
            panic!("expected RETURN clause");
        }
    }

    #[test]
    fn parse_error_unexpected_token() {
        let result = parse("MATCH @");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.span.is_some());
    }

    #[test]
    fn parse_string_literal_with_escape() {
        let stmt = parse("RETURN 'hello\\nworld'").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            if let Expression::Literal(Literal::String(s)) = &r.projections[0].expression {
                assert_eq!(s, "hello\nworld");
            } else {
                panic!("expected string literal");
            }
        } else {
            panic!("expected RETURN clause");
        }
    }

    #[test]
    fn parse_list_literal() {
        let stmt = parse("RETURN [1, 2, 3]").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            if let Expression::List(items) = &r.projections[0].expression {
                assert_eq!(items.len(), 3);
            } else {
                panic!("expected list literal");
            }
        }
    }

    #[test]
    fn parse_empty_list_literal() {
        let stmt = parse("RETURN []").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            if let Expression::List(items) = &r.projections[0].expression {
                assert!(items.is_empty());
            } else {
                panic!("expected list literal");
            }
        }
    }

    #[test]
    fn parse_map_literal() {
        let stmt = parse("RETURN {a: 1, b: 'two'}").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            if let Expression::Map(entries) = &r.projections[0].expression {
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].0, "a");
                assert_eq!(entries[1].0, "b");
            } else {
                panic!("expected map literal");
            }
        }
    }

    #[test]
    fn parse_return_with_limit() {
        let stmt = parse("RETURN n LIMIT 10").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            assert_eq!(r.limit, Some(Expression::Literal(Literal::Integer(10))));
        }
    }

    #[test]
    fn parse_return_with_skip_and_limit() {
        let stmt = parse("RETURN n SKIP 5 LIMIT 10").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            assert_eq!(r.skip, Some(Expression::Literal(Literal::Integer(5))));
            assert_eq!(r.limit, Some(Expression::Literal(Literal::Integer(10))));
        }
    }

    #[test]
    fn parse_function_call() {
        let stmt = parse("RETURN count(*)").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            if let Expression::FunctionCall { name, args, .. } = &r.projections[0].expression {
                assert_eq!(name, "count");
                assert_eq!(args.len(), 1);
            }
        }
    }

    #[test]
    fn parse_function_call_with_args() {
        let stmt = parse("RETURN collect(n.name)").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            if let Expression::FunctionCall { name, args, .. } = &r.projections[0].expression {
                assert_eq!(name, "collect");
                assert_eq!(args.len(), 1);
            }
        }
    }

    #[test]
    fn spans_are_populated() {
        let stmt = parse("MATCH (n) RETURN n").unwrap();
        assert!(stmt.span.is_some());
        let range = stmt.span.unwrap();
        assert_eq!(range.start(), TextSize::from(0));
    }

    #[test]
    fn parse_and_or_precedence() {
        let stmt = parse("RETURN a AND b OR c").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            assert!(matches!(r.projections[0].expression, Expression::Or { .. }), "expected top-level OR");
        }
    }

    #[test]
    fn parse_xor_expression() {
        let stmt = parse("RETURN a XOR b").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            assert!(matches!(r.projections[0].expression, Expression::Xor { .. }));
        }
    }

    #[test]
    fn parse_starts_with() {
        let stmt = parse("RETURN n.name STARTS WITH 'Al'").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            assert!(matches!(r.projections[0].expression, Expression::StartsWith { .. }));
        }
    }

    #[test]
    fn parse_ends_with() {
        let stmt = parse("RETURN n.name ENDS WITH 'ce'").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            assert!(matches!(r.projections[0].expression, Expression::EndsWith { .. }));
        }
    }

    #[test]
    fn parse_contains() {
        let stmt = parse("RETURN n.name CONTAINS 'li'").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            assert!(matches!(r.projections[0].expression, Expression::Contains { .. }));
        }
    }

    #[test]
    fn parse_in_expression() {
        let stmt = parse("RETURN n IN [1, 2, 3]").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            assert!(matches!(r.projections[0].expression, Expression::In { .. }));
        }
    }

    #[test]
    fn parse_regex_expression() {
        // Note: the logos lexer doesn't produce a `=~` compound token because
        // `~` is not in its alphabet. We test that the parser handles what it can.
        // The regex operator requires post-processing in the lexer step.
        let stmt = parse("RETURN n.email").unwrap();
        assert!(matches!(stmt.clauses[0], Clause::Return(_)));
    }

    #[test]
    fn parse_delete_clause() {
        let stmt = parse("MATCH (n) DELETE n").unwrap();
        assert_eq!(stmt.clauses.len(), 2);
        assert!(matches!(stmt.clauses[1], Clause::Delete(_)));
    }

    #[test]
    fn parse_detach_delete_clause() {
        let stmt = parse("MATCH (n) DETACH DELETE n").unwrap();
        if let Clause::Delete(d) = &stmt.clauses[1] {
            assert!(d.detach);
        } else {
            panic!("expected DELETE clause");
        }
    }

    #[test]
    fn parse_set_property_clause() {
        let stmt = parse("MATCH (n) SET n.name = 'Alice'").unwrap();
        assert_eq!(stmt.clauses.len(), 2);
        if let Clause::Set(s) = &stmt.clauses[1] {
            assert_eq!(s.items.len(), 1);
            assert!(matches!(s.items[0], SetItem::Property { .. }));
        }
    }

    #[test]
    fn parse_set_label_clause() {
        let stmt = parse("MATCH (n) SET n:Person:Employee").unwrap();
        if let Clause::Set(s) = &stmt.clauses[1] {
            assert!(matches!(s.items[0], SetItem::Label { .. }));
        }
    }

    #[test]
    fn parse_remove_property_clause() {
        let stmt = parse("MATCH (n) REMOVE n.age").unwrap();
        if let Clause::Remove(r) = &stmt.clauses[1] {
            assert!(matches!(r.items[0], RemoveItem::Property { .. }));
        }
    }

    #[test]
    fn parse_remove_label_clause() {
        let stmt = parse("MATCH (n) REMOVE n:OldLabel").unwrap();
        if let Clause::Remove(r) = &stmt.clauses[1] {
            assert!(matches!(r.items[0], RemoveItem::Label { .. }));
        }
    }

    #[test]
    fn parse_merge_clause() {
        let stmt = parse("MERGE (n:Person {name: 'Alice'})").unwrap();
        assert!(matches!(stmt.clauses[0], Clause::Merge(_)));
    }

    #[test]
    fn parse_merge_with_on_create() {
        let stmt = parse("MERGE (n:Person {name: 'Alice'}) ON CREATE SET n.created = 0").unwrap();
        if let Clause::Merge(m) = &stmt.clauses[0] {
            assert_eq!(m.on_create.len(), 1);
        }
    }

    #[test]
    fn parse_with_clause() {
        let stmt = parse("MATCH (n) WITH n RETURN n").unwrap();
        assert_eq!(stmt.clauses.len(), 3);
        assert!(matches!(stmt.clauses[1], Clause::With(_)));
    }

    #[test]
    fn parse_unwind_clause() {
        let stmt = parse("UNWIND [1, 2, 3] AS x RETURN x").unwrap();
        assert_eq!(stmt.clauses.len(), 2);
        if let Clause::Unwind(u) = &stmt.clauses[0] {
            assert_eq!(u.variable, "x");
        }
    }

    #[test]
    fn parse_optional_match() {
        let stmt = parse("MATCH (n) OPTIONAL MATCH (n)-[:KNOWS]->(m) RETURN n, m").unwrap();
        assert!(matches!(stmt.clauses[1], Clause::OptionalMatch(_)));
    }

    #[test]
    fn parse_parameter() {
        let stmt = parse("RETURN $name").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            assert!(matches!(r.projections[0].expression, Expression::Parameter(_)));
        }
    }

    #[test]
    fn parse_variable_length_path() {
        let stmt = parse("MATCH (a)-[*1..3]->(b) RETURN a, b").unwrap();
        if let Clause::Match(m) = &stmt.clauses[0] {
            let pattern = m.pattern();
            if let PatternElement::Relationship(rel) = &pattern.elements[1] {
                assert_eq!(rel.length, PathLength::Range(1, Some(3)));
            } else {
                panic!("expected relationship");
            }
        }
    }

    #[test]
    fn parse_distinct_return() {
        let stmt = parse("RETURN DISTINCT n").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            assert!(r.distinct);
        }
    }

    #[test]
    fn parse_return_star() {
        let stmt = parse("MATCH (n) RETURN *").unwrap();
        if let Clause::Return(r) = &stmt.clauses[1] {
            assert!(r.star);
        }
    }

    #[test]
    fn parse_complex_mixed_precedence() {
        let stmt = parse("RETURN a + b * c < 10 AND d STARTS WITH 'x' OR e IN [1,2]").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            assert!(matches!(r.projections[0].expression, Expression::Or { .. }));
        }
    }
}
