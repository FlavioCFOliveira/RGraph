//! Recursive-descent parser for the Sprint 8 Cypher subset.
//!
//! Supports:
//!   MATCH, WHERE, RETURN, CREATE
//!   Fixed-length patterns with labels, types, and property maps
//!   Literals, variables, property access, comparisons, AND/OR/NOT
//!
//! The parser produces an [`ast::Statement`](crate::cypher::ast::Statement)
//! annotated with source [`TextRange`](text_size::TextRange) spans on every
//! node.  Spans are accumulated during parsing so that error reporters, the
//! TCK harness, and IDE features can map AST elements back to the original
//! query text.

use crate::cypher::ast::*;
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
    /// The source span where the error occurred.
    pub span: Option<TextRange>,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Syntax error at {}:{}: {}",
            self.line, self.column, self.message
        )
    }
}

impl std::error::Error for ParseError {}

impl From<ParseError> for RGraphError {
    fn from(e: ParseError) -> Self {
        RGraphError::Syntax(format!(
            "{} at {}:{}",
            e.message, e.line, e.column
        ))
    }
}

/// Parse a complete Cypher statement.
///
/// Every node in the returned [`Statement`] carries a [`TextRange`] that
/// maps back to the original `input` string.
pub fn parse(input: &str) -> Result<Statement, ParseError> {
    let mut parser = Parser::new(input);
    let mut clauses = Vec::new();
    let stmt_start = parser.offset();

    while !parser.is_eof() {
        parser.skip_whitespace();
        if parser.is_eof() {
            break;
        }
        let clause = parser.parse_clause()?;
        clauses.push(clause);
        parser.skip_whitespace();
        if parser.peek_char() == Some(';') {
            parser.advance(); // optional statement terminator
            parser.skip_whitespace();
        }
    }

    if clauses.is_empty() {
        return Err(parser.error("empty statement"));
    }

    let stmt_end = parser.offset();
    Ok(Statement {
        clauses,
        span: Some(TextRange::new(stmt_start, stmt_end)),
    })
}

struct Parser {
    input: Vec<char>,
    pos: usize,
    line: usize,
    column: usize,
}

impl Parser {
    fn new(input: &str) -> Self {
        Self {
            input: input.chars().collect(),
            pos: 0,
            line: 1,
            column: 1,
        }
    }

    fn is_eof(&self) -> bool {
        self.pos >= self.input.len()
    }

    fn peek_char(&self) -> Option<char> {
        self.input.get(self.pos).copied()
    }

    fn offset(&self) -> TextSize {
        TextSize::from(self.pos as u32)
    }

    fn advance(&mut self) -> Option<char> {
        let ch = self.input.get(self.pos).copied();
        if let Some(c) = ch {
            self.pos += 1;
            if c == '\n' {
                self.line += 1;
                self.column = 1;
            } else {
                self.column += 1;
            }
        }
        ch
    }

    fn error(&self, msg: &str) -> ParseError {
        let start = self.offset();
        let end = TextSize::from((self.pos + 1).min(self.input.len()) as u32);
        ParseError {
            message: msg.to_string(),
            offset: self.pos,
            line: self.line,
            column: self.column,
            span: Some(TextRange::new(start, end)),
        }
    }

    fn skip_whitespace(&mut self) {
        while let Some(c) = self.peek_char() {
            if c.is_whitespace() {
                self.advance();
            } else if c == '/' && self.input.get(self.pos + 1) == Some(&'/') {
                // Skip single-line comment.
                while let Some(c2) = self.peek_char() {
                    self.advance();
                    if c2 == '\n' {
                        break;
                    }
                }
            } else {
                break;
            }
        }
    }

    fn expect_keyword(&mut self,
        keyword: &str,
    ) -> Result<(), ParseError> {
        let start = self.pos;
        let mut collected = String::new();
        while let Some(c) = self.peek_char() {
            if c.is_ascii_alphabetic() || c == '_' {
                collected.push(c);
                self.advance();
            } else {
                break;
            }
        }
        if collected.eq_ignore_ascii_case(keyword) {
            Ok(())
        } else {
            self.pos = start;
            self.column -= collected.chars().count();
            Err(self.error(&format!("expected keyword '{}'", keyword)))
        }
    }

    fn parse_clause(&mut self) -> Result<Clause, ParseError> {
        self.skip_whitespace();
        if self.is_eof() {
            return Err(self.error("unexpected end of input"));
        }

        let clause_start = self.offset();
        let keyword = self.read_keyword();
        let kw_upper = keyword.to_ascii_uppercase();
        match kw_upper.as_str() {
            "MATCH" => {
                self.skip_whitespace();
                let pattern = self.parse_pattern()?;
                let clause_end = self.offset();
                Ok(Clause::Match(MatchClause {
                    pattern,
                    span: Some(TextRange::new(clause_start, clause_end)),
                }))
            }
            "WHERE" => {
                self.skip_whitespace();
                let predicate = self.parse_expression(0)?;
                let clause_end = self.offset();
                Ok(Clause::Where(WhereClause {
                    predicate,
                    span: Some(TextRange::new(clause_start, clause_end)),
                }))
            }
            "RETURN" => {
                self.skip_whitespace();
                let (projections, order_by, skip, limit) = self.parse_return_body()?;
                let clause_end = self.offset();
                Ok(Clause::Return(ReturnClause {
                    projections,
                    order_by,
                    skip,
                    limit,
                    span: Some(TextRange::new(clause_start, clause_end)),
                }))
            }
            "CREATE" => {
                self.skip_whitespace();
                let pattern = self.parse_pattern()?;
                let clause_end = self.offset();
                Ok(Clause::Create(CreateClause {
                    pattern,
                    span: Some(TextRange::new(clause_start, clause_end)),
                }))
            }
            _ => {
                self.pos = clause_start.into();
                Err(self.error(&format!("unexpected keyword or token: '{}'", keyword)))
            }
        }
    }

    fn read_keyword(&mut self) -> String {
        let mut s = String::new();
        while let Some(c) = self.peek_char() {
            if c.is_ascii_alphabetic() || c == '_' {
                s.push(c);
                self.advance();
            } else {
                break;
            }
        }
        s
    }

    fn parse_return_body(
        &mut self,
    ) -> Result<(
        Vec<Projection>,
        Vec<OrderItem>,
        Option<Expression>,
        Option<Expression>,
    ), ParseError> {
        let projections = self.parse_projection_list()?;
        let mut order_by = Vec::new();
        let mut skip = None;
        let mut limit = None;

        loop {
            self.skip_whitespace();
            if self.is_eof() {
                break;
            }
            let kw_start = self.offset();
            let kw = self.read_keyword().to_ascii_uppercase();
            match kw.as_str() {
                "ORDER" => {
                    self.skip_whitespace();
                    self.expect_keyword("BY")?;
                    self.skip_whitespace();
                    order_by = self.parse_order_by()?;
                }
                "SKIP" => {
                    self.skip_whitespace();
                    skip = Some(self.parse_expression(0)?);
                }
                "LIMIT" => {
                    self.skip_whitespace();
                    limit = Some(self.parse_expression(0)?);
                }
                "" => break,
                _ => {
                    // Not a keyword we recognise; backtrack.
                    self.pos = kw_start.into();
                    break;
                }
            }
        }

        Ok((projections, order_by, skip, limit))
    }

    fn parse_projection_list(
        &mut self,
    ) -> Result<Vec<Projection>, ParseError> {
        let mut projections = Vec::new();
        loop {
            self.skip_whitespace();
            let proj_start = self.offset();
            let expr = self.parse_expression(0)?;
            self.skip_whitespace();

            // Attempt to read an optional AS alias.
            let alias = {
                let start_pos = self.pos;
                let start_line = self.line;
                let start_col = self.column;
                let kw = self.read_keyword().to_ascii_uppercase();
                if kw == "AS" {
                    self.skip_whitespace();
                    Some(self.parse_identifier()?)
                } else {
                    // Backtrack so that any RETURN-body keyword (ORDER, SKIP,
                    // LIMIT) remains unconsumed for the caller.
                    self.pos = start_pos;
                    self.line = start_line;
                    self.column = start_col;
                    None
                }
            };
            let proj_end = self.offset();

            projections.push(Projection {
                expression: expr,
                alias,
                span: Some(TextRange::new(proj_start, proj_end)),
            });
            self.skip_whitespace();
            if self.peek_char() == Some(',') {
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
            self.skip_whitespace();
            let item_start = self.offset();
            let expr = self.parse_expression(0)?;
            self.skip_whitespace();
            let ascending = match self.read_keyword().to_ascii_uppercase().as_str() {
                "DESC" => false,
                "ASC" => true,
                _ => true,
            };
            let item_end = self.offset();
            items.push(OrderItem {
                expression: expr,
                ascending,
                span: Some(TextRange::new(item_start, item_end)),
            });
            self.skip_whitespace();
            if self.peek_char() == Some(',') {
                self.advance();
            } else {
                break;
            }
        }
        Ok(items)
    }

    fn parse_pattern(&mut self) -> Result<Pattern, ParseError> {
        let pattern_start = self.offset();
        let mut elements = Vec::new();
        loop {
            self.skip_whitespace();
            let node = self.parse_node_pattern()?;
            elements.push(PatternElement::Node(node));

            self.skip_whitespace();
            // Check for relationship
            if self.peek_char() == Some('<') || self.peek_char() == Some('-') {
                let rel = self.parse_relationship_pattern()?;
                elements.push(PatternElement::Relationship(rel));
            } else {
                break;
            }
        }
        let pattern_end = self.offset();
        Ok(Pattern {
            elements,
            span: Some(TextRange::new(pattern_start, pattern_end)),
        })
    }

    fn parse_node_pattern(&mut self) -> Result<NodePattern, ParseError> {
        self.skip_whitespace();
        let node_start = self.offset();
        if self.peek_char() != Some('(') {
            return Err(self.error("expected '(' to start node pattern"));
        }
        self.advance(); // consume '('

        self.skip_whitespace();
        let variable = if self.peek_char().map(|c| c.is_ascii_alphabetic()).unwrap_or(false) {
            Some(self.parse_identifier()?)
        } else {
            None
        };

        let mut labels = Vec::new();
        let mut properties = HashMap::new();

        self.skip_whitespace();
        if self.peek_char() == Some(':') {
            self.advance();
            labels.push(self.parse_identifier()?);
            while self.peek_char() == Some(':') {
                self.advance();
                labels.push(self.parse_identifier()?);
            }
        }

        self.skip_whitespace();
        if self.peek_char() == Some('{') {
            properties = self.parse_property_map()?;
        }

        self.skip_whitespace();
        if self.peek_char() != Some(')') {
            return Err(self.error("expected ')' to end node pattern"));
        }
        self.advance(); // consume ')'
        let node_end = self.offset();

        Ok(NodePattern {
            variable,
            labels,
            properties,
            span: Some(TextRange::new(node_start, node_end)),
        })
    }

    fn parse_relationship_pattern(&mut self) -> Result<RelationshipPattern, ParseError> {
        self.skip_whitespace();
        let rel_start = self.offset();
        let mut direction = Direction::Both;

        if self.peek_char() == Some('<') {
            self.advance();
            direction = Direction::Incoming;
        }

        self.skip_whitespace();
        if self.peek_char() != Some('-') {
            return Err(self.error("expected '-' in relationship pattern"));
        }
        self.advance(); // consume '-'

        self.skip_whitespace();
        let mut variable = None;
        let mut types = Vec::new();
        let mut properties = HashMap::new();

        if self.peek_char() == Some('[') {
            self.advance(); // consume '['
            self.skip_whitespace();

            // Optional variable
            if self.peek_char().map(|c| c.is_ascii_alphabetic()).unwrap_or(false) {
                let ident = self.parse_identifier()?;
                self.skip_whitespace();
                if self.peek_char() == Some(':') {
                    // ident was actually a type, not a variable
                    types.push(ident);
                } else {
                    variable = Some(ident);
                }
            }

            // Types
            while self.peek_char() == Some(':') {
                self.advance();
                types.push(self.parse_identifier()?);
                self.skip_whitespace();
            }

            // Properties
            if self.peek_char() == Some('{') {
                properties = self.parse_property_map()?;
            }

            self.skip_whitespace();
            if self.peek_char() != Some(']') {
                return Err(self.error("expected ']' to end relationship pattern"));
            }
            self.advance(); // consume ']'
        }

        self.skip_whitespace();
        if self.peek_char() != Some('-') {
            return Err(self.error("expected '-' after relationship details"));
        }
        self.advance(); // consume '-'

        if self.peek_char() == Some('>') {
            if direction == Direction::Incoming {
                return Err(self.error("relationship cannot be both incoming and outgoing"));
            }
            self.advance();
            direction = Direction::Outgoing;
        }
        let rel_end = self.offset();

        Ok(RelationshipPattern {
            direction,
            types,
            variable,
            properties,
            length: PathLength::Fixed(1),
            span: Some(TextRange::new(rel_start, rel_end)),
        })
    }

    fn parse_property_map(&mut self) -> Result<HashMap<String, Expression>, ParseError> {
        let mut map = HashMap::new();
        if self.peek_char() != Some('{') {
            return Err(self.error("expected '{' to start property map"));
        }
        self.advance(); // consume '{'

        self.skip_whitespace();
        if self.peek_char() == Some('}') {
            self.advance();
            return Ok(map);
        }

        loop {
            self.skip_whitespace();
            let key = self.parse_identifier()?;
            self.skip_whitespace();
            if self.peek_char() != Some(':') {
                return Err(self.error("expected ':' after property key"));
            }
            self.advance();
            self.skip_whitespace();
            let value = self.parse_expression(0)?;
            map.insert(key, value);
            self.skip_whitespace();
            if self.peek_char() == Some(',') {
                self.advance();
            } else {
                break;
            }
        }

        self.skip_whitespace();
        if self.peek_char() != Some('}') {
            return Err(self.error("expected '}' to end property map"));
        }
        self.advance(); // consume '}'
        Ok(map)
    }

    fn parse_identifier(&mut self) -> Result<String, ParseError> {
        let mut s = String::new();
        if let Some(c) = self.peek_char() {
            if c.is_ascii_alphabetic() || c == '_' {
                s.push(c);
                self.advance();
            } else {
                return Err(self.error("expected identifier"));
            }
        } else {
            return Err(self.error("unexpected end of input, expected identifier"));
        }

        while let Some(c) = self.peek_char() {
            if c.is_ascii_alphanumeric() || c == '_' {
                s.push(c);
                self.advance();
            } else {
                break;
            }
        }
        Ok(s)
    }

    /// Return the next keyword without advancing the cursor.
    fn peek_keyword(&self) -> Option<String> {
        let mut s = String::new();
        let mut pos = self.pos;
        while let Some(&c) = self.input.get(pos) {
            if c.is_ascii_alphabetic() || c == '_' {
                s.push(c);
                pos += 1;
            } else {
                break;
            }
        }
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }

    // Pratt parser for expressions.
    fn parse_expression(&mut self,
        min_bp: u8,
    ) -> Result<Expression, ParseError> {
        self.skip_whitespace();
        let expr_start = self.offset();
        let mut lhs = self.parse_primary()?;

        loop {
            self.skip_whitespace();

            // Determine the next infix operator (symbolic or keyword).
            let op = if let Some(sym_op) = self.peek_operator() {
                sym_op
            } else if let Some(kw) = self.peek_keyword() {
                let kw_upper = kw.to_ascii_uppercase();
                match kw_upper.as_str() {
                    "AND" | "OR" | "XOR" | "STARTS" | "ENDS" | "CONTAINS" | "IN" => kw_upper,
                    _ => break,
                }
            } else {
                break;
            };

            let (lbp, rbp) = infix_binding_power(&op);
            if lbp < min_bp {
                break;
            }

            // Consume the operator.
            if op == "STARTS" || op == "ENDS" {
                self.read_keyword(); // consume STARTS / ENDS
                self.skip_whitespace();
                self.expect_keyword("WITH")?;
            } else if op.len() == 1 || op.starts_with('<') || op.starts_with('>') || op.starts_with('=') || op.starts_with('!') {
                // Symbolic operator (including multi-char like <=, >=, <>, =~).
                for _ in 0..op.len() {
                    self.advance();
                }
            } else {
                // Single-word keyword operator (AND, OR, XOR, CONTAINS, IN).
                self.read_keyword();
            }

            self.skip_whitespace();
            let rhs = self.parse_expression(rbp)?;
            let expr_end = self.offset();

            lhs = match op.as_str() {
                "AND" => Expression::And {
                    left: Box::new(lhs),
                    right: Box::new(rhs),
                    span: Some(TextRange::new(expr_start, expr_end)),
                },
                "OR" => Expression::Or {
                    left: Box::new(lhs),
                    right: Box::new(rhs),
                    span: Some(TextRange::new(expr_start, expr_end)),
                },
                "XOR" => Expression::Xor {
                    left: Box::new(lhs),
                    right: Box::new(rhs),
                    span: Some(TextRange::new(expr_start, expr_end)),
                },
                "STARTS" => Expression::StartsWith {
                    left: Box::new(lhs),
                    right: Box::new(rhs),
                    span: Some(TextRange::new(expr_start, expr_end)),
                },
                "ENDS" => Expression::EndsWith {
                    left: Box::new(lhs),
                    right: Box::new(rhs),
                    span: Some(TextRange::new(expr_start, expr_end)),
                },
                "CONTAINS" => Expression::Contains {
                    left: Box::new(lhs),
                    right: Box::new(rhs),
                    span: Some(TextRange::new(expr_start, expr_end)),
                },
                "IN" => Expression::In {
                    left: Box::new(lhs),
                    right: Box::new(rhs),
                    span: Some(TextRange::new(expr_start, expr_end)),
                },
                "=~" => Expression::Regex {
                    left: Box::new(lhs),
                    right: Box::new(rhs),
                    span: Some(TextRange::new(expr_start, expr_end)),
                },
                _ => {
                    if let Some(bin_op) = arithmetic_op(&op) {
                        Expression::BinaryOp {
                            op: bin_op,
                            left: Box::new(lhs),
                            right: Box::new(rhs),
                            span: Some(TextRange::new(expr_start, expr_end)),
                        }
                    } else {
                        Expression::Comparison {
                            op: comparison_op(&op),
                            left: Box::new(lhs),
                            right: Box::new(rhs),
                            span: Some(TextRange::new(expr_start, expr_end)),
                        }
                    }
                }
            };
        }

        // Handle IS NULL / IS NOT NULL postfix operators.
        loop {
            self.skip_whitespace();
            let start_pos = self.pos;
            let start_line = self.line;
            let start_col = self.column;
            let kw = self.read_keyword().to_ascii_uppercase();
            if kw == "IS" {
                self.skip_whitespace();
                let next_kw = self.read_keyword().to_ascii_uppercase();
                if next_kw == "NOT" {
                    self.skip_whitespace();
                    let null_kw = self.read_keyword().to_ascii_uppercase();
                    if null_kw == "NULL" {
                        lhs = Expression::IsNotNull(Box::new(lhs));
                        continue;
                    } else {
                        return Err(self.error("expected NULL after IS NOT"));
                    }
                } else if next_kw == "NULL" {
                    lhs = Expression::IsNull(Box::new(lhs));
                    continue;
                } else {
                    return Err(self.error("expected NULL or NOT NULL after IS"));
                }
            } else {
                self.pos = start_pos;
                self.line = start_line;
                self.column = start_col;
                break;
            }
        }

        Ok(lhs)
    }

    fn parse_primary(&mut self) -> Result<Expression, ParseError> {
        self.skip_whitespace();
        let expr_start = self.offset();
        match self.peek_char() {
            None => Err(self.error("unexpected end of input")),
            Some('(') => {
                self.advance();
                self.skip_whitespace();
                let expr = self.parse_expression(0)?;
                self.skip_whitespace();
                if self.peek_char() != Some(')') {
                    return Err(self.error("expected ')'"));
                }
                self.advance();
                Ok(expr)
            }
            Some('[') => self.parse_list_literal(),
            Some('{') => self.parse_map_literal(),
            Some('-') => {
                self.advance();
                let expr = self.parse_primary()?;
                let expr_end = self.offset();
                Ok(Expression::UnaryOp {
                    op: UnaryOperator::Neg,
                    expr: Box::new(expr),
                    span: Some(TextRange::new(expr_start, expr_end)),
                })
            }
            Some('\'') | Some('"') => self.parse_string_literal(),
            Some(c) if c.is_ascii_digit() => self.parse_number_literal(),
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {
                let ident = self.parse_identifier()?;
                self.skip_whitespace();
                if self.peek_char() == Some('.') {
                    self.advance();
                    let prop = self.parse_identifier()?;
                    let expr_end = self.offset();
                    Ok(Expression::PropertyAccess {
                        base: Box::new(Expression::Variable(ident)),
                        property: prop,
                        span: Some(TextRange::new(expr_start, expr_end)),
                    })
                } else if self.peek_char() == Some('(') {
                    // Function call: ident(args...)
                    self.advance(); // consume '('
                    self.skip_whitespace();
                    let mut args = Vec::new();
                    if self.peek_char() != Some(')') {
                        loop {
                            self.skip_whitespace();
                            // Support DISTINCT keyword as first argument.
                            let arg_start = self.pos;
                            let arg_line = self.line;
                            let arg_col = self.column;
                            let kw = self.read_keyword().to_ascii_uppercase();
                            let distinct = if kw == "DISTINCT" {
                                self.skip_whitespace();
                                true
                            } else {
                                self.pos = arg_start;
                                self.line = arg_line;
                                self.column = arg_col;
                                false
                            };
                            // Handle wildcard `*` as a special argument.
                            if self.peek_char() == Some('*') {
                                self.advance();
                                args.push(Expression::Wildcard);
                            } else {
                                let arg = self.parse_expression(0)?;
                                args.push(arg);
                            }
                            self.skip_whitespace();
                            if self.peek_char() == Some(',') {
                                self.advance();
                            } else {
                                break;
                            }
                        }
                    }
                    self.skip_whitespace();
                    if self.peek_char() != Some(')') {
                        return Err(self.error("expected ')' to end function call"));
                    }
                    self.advance(); // consume ')'
                    let expr_end = self.offset();
                    Ok(Expression::FunctionCall {
                        name: ident,
                        args,
                        distinct: false, // TODO: propagate distinct correctly
                        span: Some(TextRange::new(expr_start, expr_end)),
                    })
                } else {
                    match ident.to_ascii_uppercase().as_str() {
                        "TRUE" => Ok(Expression::Literal(Literal::Boolean(true))),
                        "FALSE" => Ok(Expression::Literal(Literal::Boolean(false))),
                        "NULL" => Ok(Expression::Literal(Literal::Null)),
                        "NOT" => {
                            self.skip_whitespace();
                            let expr = self.parse_primary()?;
                            let expr_end = self.offset();
                            Ok(Expression::UnaryOp {
                                op: UnaryOperator::Not,
                                expr: Box::new(expr),
                                span: Some(TextRange::new(expr_start, expr_end)),
                            })
                        }
                        _ => Ok(Expression::Variable(ident)),
                    }
                }
            }
            Some(c) => Err(self.error(&format!("unexpected character: '{}'", c))),
        }
    }

    fn parse_list_literal(&mut self) -> Result<Expression, ParseError> {
        self.skip_whitespace();
        let list_start = self.offset();
        if self.peek_char() != Some('[') {
            return Err(self.error("expected '[' to start list literal"));
        }
        self.advance(); // consume '['
        self.skip_whitespace();

        let mut items = Vec::new();
        if self.peek_char() == Some(']') {
            self.advance();
            let list_end = self.offset();
            return Ok(Expression::List(items));
        }

        loop {
            self.skip_whitespace();
            let expr = self.parse_expression(0)?;
            items.push(expr);
            self.skip_whitespace();
            if self.peek_char() == Some(',') {
                self.advance();
            } else {
                break;
            }
        }

        self.skip_whitespace();
        if self.peek_char() != Some(']') {
            return Err(self.error("expected ']' to end list literal"));
        }
        self.advance();
        let list_end = self.offset();
        Ok(Expression::List(items))
    }

    fn parse_map_literal(&mut self) -> Result<Expression, ParseError> {
        self.skip_whitespace();
        let map_start = self.offset();
        if self.peek_char() != Some('{') {
            return Err(self.error("expected '{' to start map literal"));
        }
        self.advance(); // consume '{'
        self.skip_whitespace();

        let mut entries = Vec::new();
        if self.peek_char() == Some('}') {
            self.advance();
            let map_end = self.offset();
            return Ok(Expression::Map(entries));
        }

        loop {
            self.skip_whitespace();
            let key = self.parse_identifier()?;
            self.skip_whitespace();
            if self.peek_char() != Some(':') {
                return Err(self.error("expected ':' after map key"));
            }
            self.advance();
            self.skip_whitespace();
            let value = self.parse_expression(0)?;
            entries.push((key, value));
            self.skip_whitespace();
            if self.peek_char() == Some(',') {
                self.advance();
            } else {
                break;
            }
        }

        self.skip_whitespace();
        if self.peek_char() != Some('}') {
            return Err(self.error("expected '}' to end map literal"));
        }
        self.advance();
        let map_end = self.offset();
        Ok(Expression::Map(entries))
    }

    fn parse_string_literal(&mut self) -> Result<Expression, ParseError> {
        let _quote = self.advance().unwrap();
        let mut s = String::new();
        while let Some(c) = self.peek_char() {
            if c == _quote {
                self.advance();
                break;
            } else if c == '\\' {
                self.advance();
                match self.advance() {
                    Some('n') => s.push('\n'),
                    Some('t') => s.push('\t'),
                    Some('\\') => s.push('\\'),
                    Some('"') => s.push('"'),
                    Some('\'') => s.push('\''),
                    Some(other) => s.push(other),
                    None => return Err(self.error("unterminated string literal")),
                }
            } else {
                s.push(c);
                self.advance();
            }
        }
        Ok(Expression::Literal(Literal::String(s)))
    }

    fn parse_number_literal(&mut self) -> Result<Expression, ParseError> {
        let mut s = String::new();
        let mut is_float = false;
        while let Some(c) = self.peek_char() {
            if c.is_ascii_digit() {
                s.push(c);
                self.advance();
            } else if c == '.' && !is_float {
                is_float = true;
                s.push(c);
                self.advance();
            } else {
                break;
            }
        }
        if is_float {
            match s.parse::<f64>() {
                Ok(v) => Ok(Expression::Literal(Literal::Float(v))),
                Err(_) => Err(self.error("invalid float literal")),
            }
        } else {
            match s.parse::<i64>() {
                Ok(v) => Ok(Expression::Literal(Literal::Integer(v))),
                Err(_) => Err(self.error("invalid integer literal")),
            }
        }
    }

    fn peek_operator(&self) -> Option<String> {
        let mut op = String::new();
        let mut lookahead = self.pos;
        while let Some(&c) = self.input.get(lookahead) {
            if "<>=!+-*/%~".contains(c) {
                op.push(c);
                lookahead += 1;
            } else {
                break;
            }
        }
        if op.is_empty() {
            None
        } else {
            Some(op)
        }
    }

    fn advance_operator(&mut self, op: &str) -> Result<(), ParseError> {
        for _ in 0..op.len() {
            self.advance();
        }
        Ok(())
    }
}

fn infix_binding_power(op: &str) -> (u8, u8) {
    match op {
        // Logical (lowest precedence)
        "OR" => (10, 11),
        "XOR" => (20, 21),
        "AND" => (30, 31),
        // Comparisons and string/list operators
        "=" | "<>" | "<" | ">" | "<=" | ">=" | "=~" | "STARTS" | "ENDS" | "CONTAINS" | "IN" => {
            (40, 41)
        }
        // Additive
        "+" | "-" => (50, 51),
        // Multiplicative
        "*" | "/" | "%" => (60, 61),
        _ => (0, 0),
    }
}

fn comparison_op(op: &str) -> ComparisonOperator {
    match op {
        "=" => ComparisonOperator::Eq,
        "<>" => ComparisonOperator::Ne,
        "<" => ComparisonOperator::Lt,
        "<=" => ComparisonOperator::Le,
        ">" => ComparisonOperator::Gt,
        ">=" => ComparisonOperator::Ge,
        _ => ComparisonOperator::Eq,
    }
}

fn arithmetic_op(op: &str) -> Option<BinaryOperator> {
    match op {
        "+" => Some(BinaryOperator::Add),
        "-" => Some(BinaryOperator::Sub),
        "*" => Some(BinaryOperator::Mul),
        "/" => Some(BinaryOperator::Div),
        "%" => Some(BinaryOperator::Mod),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use text_size::TextSize;

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
            assert!(matches!(
                w.predicate,
                Expression::Comparison {
                    op: ComparisonOperator::Gt,
                    ..
                }
            ));
        } else {
            panic!("expected WHERE clause");
        }
    }

    #[test]
    fn parse_property_access() {
        let stmt = parse("RETURN n.name").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            assert!(matches!(
                r.projections[0].expression,
                Expression::PropertyAccess { .. }
            ));
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
        } else {
            panic!("expected RETURN clause");
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
        } else {
            panic!("expected RETURN clause");
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
        } else {
            panic!("expected RETURN clause");
        }
    }

    #[test]
    fn parse_empty_map_literal() {
        let stmt = parse("RETURN {}").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            if let Expression::Map(entries) = &r.projections[0].expression {
                assert!(entries.is_empty());
            } else {
                panic!("expected map literal");
            }
        } else {
            panic!("expected RETURN clause");
        }
    }

    #[test]
    fn parse_return_with_limit() {
        let stmt = parse("RETURN n LIMIT 10").unwrap();
        assert_eq!(stmt.clauses.len(), 1);
        if let Clause::Return(r) = &stmt.clauses[0] {
            assert_eq!(r.limit, Some(Expression::Literal(Literal::Integer(10))));
        } else {
            panic!("expected RETURN clause");
        }
    }

    #[test]
    fn parse_return_with_skip_and_limit() {
        let stmt = parse("RETURN n SKIP 5 LIMIT 10").unwrap();
        assert_eq!(stmt.clauses.len(), 1);
        if let Clause::Return(r) = &stmt.clauses[0] {
            assert_eq!(r.skip, Some(Expression::Literal(Literal::Integer(5))));
            assert_eq!(r.limit, Some(Expression::Literal(Literal::Integer(10))));
        } else {
            panic!("expected RETURN clause");
        }
    }

    #[test]
    fn parse_function_call() {
        let stmt = parse("RETURN count(*)").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            if let Expression::FunctionCall { name, args, .. } = &r.projections[0].expression {
                assert_eq!(name, "count");
                assert_eq!(args.len(), 1);
            } else {
                panic!("expected function call");
            }
        } else {
            panic!("expected RETURN clause");
        }
    }

    #[test]
    fn parse_function_call_with_args() {
        let stmt = parse("RETURN collect(n.name)").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            if let Expression::FunctionCall { name, args, .. } = &r.projections[0].expression {
                assert_eq!(name, "collect");
                assert_eq!(args.len(), 1);
            } else {
                panic!("expected function call");
            }
        } else {
            panic!("expected RETURN clause");
        }
    }

    #[test]
    fn spans_are_populated() {
        let stmt = parse("MATCH (n) RETURN n").unwrap();
        assert!(stmt.span.is_some());
        let range = stmt.span.unwrap();
        assert_eq!(range.start(), TextSize::from(0));
        assert_eq!(range.end(), TextSize::from(18));

        if let Clause::Match(m) = &stmt.clauses[0] {
            assert!(m.span.is_some());
            let mrange = m.span.unwrap();
            assert_eq!(mrange.start(), TextSize::from(0));
            assert_eq!(mrange.end(), TextSize::from(10));
        } else {
            panic!("expected MATCH clause");
        }
    }

    #[test]
    fn parse_and_or_precedence() {
        // AND binds tighter than OR.
        let stmt = parse("RETURN a AND b OR c").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            assert!(
                matches!(
                    r.projections[0].expression,
                    Expression::Or { .. }
                ),
                "expected top-level OR"
            );
        } else {
            panic!("expected RETURN clause");
        }
    }

    #[test]
    fn parse_xor_expression() {
        let stmt = parse("RETURN a XOR b").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            assert!(
                matches!(r.projections[0].expression, Expression::Xor { .. }),
                "expected XOR expression"
            );
        } else {
            panic!("expected RETURN clause");
        }
    }

    #[test]
    fn parse_starts_with() {
        let stmt = parse("RETURN n.name STARTS WITH 'Al'").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            assert!(
                matches!(r.projections[0].expression, Expression::StartsWith { .. }),
                "expected STARTS WITH expression"
            );
        } else {
            panic!("expected RETURN clause");
        }
    }

    #[test]
    fn parse_ends_with() {
        let stmt = parse("RETURN n.name ENDS WITH 'ce'").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            assert!(
                matches!(r.projections[0].expression, Expression::EndsWith { .. }),
                "expected ENDS WITH expression"
            );
        } else {
            panic!("expected RETURN clause");
        }
    }

    #[test]
    fn parse_contains() {
        let stmt = parse("RETURN n.name CONTAINS 'li'").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            assert!(
                matches!(r.projections[0].expression, Expression::Contains { .. }),
                "expected CONTAINS expression"
            );
        } else {
            panic!("expected RETURN clause");
        }
    }

    #[test]
    fn parse_in_expression() {
        let stmt = parse("RETURN n IN [1, 2, 3]").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            assert!(
                matches!(r.projections[0].expression, Expression::In { .. }),
                "expected IN expression"
            );
        } else {
            panic!("expected RETURN clause");
        }
    }

    #[test]
    fn parse_regex_expression() {
        let stmt = parse("RETURN n.email =~ '.*@example.com'").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            assert!(
                matches!(r.projections[0].expression, Expression::Regex { .. }),
                "expected =~ expression"
            );
        } else {
            panic!("expected RETURN clause");
        }
    }

    #[test]
    fn parse_complex_mixed_precedence() {
        // a + b * c < 10 AND d STARTS WITH 'x' OR e IN [1,2]
        let stmt = parse("RETURN a + b * c < 10 AND d STARTS WITH 'x' OR e IN [1,2]").unwrap();
        if let Clause::Return(r) = &stmt.clauses[0] {
            // Top-level must be OR
            assert!(
                matches!(r.projections[0].expression, Expression::Or { .. }),
                "expected top-level OR"
            );
        } else {
            panic!("expected RETURN clause");
        }
    }
}
