//! Recursive-descent parser for the Sprint 8 Cypher subset.
//!
//! Supports:
//!   MATCH, WHERE, RETURN, CREATE
//!   Fixed-length patterns with labels, types, and property maps
//!   Literals, variables, property access, comparisons, AND/OR/NOT
//!
//! The parser produces an [`ast::Statement`](crate::cypher::ast::Statement).

use crate::cypher::ast::*;
use crate::error::RGraphError;
use std::collections::HashMap;

/// Parse error with source location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    pub offset: usize,
    pub line: usize,
    pub column: usize,
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
pub fn parse(input: &str) -> Result<Statement, ParseError> {
    let mut parser = Parser::new(input);
    let mut stmt = Statement::new();

    while !parser.is_eof() {
        parser.skip_whitespace();
        if parser.is_eof() {
            break;
        }
        let clause = parser.parse_clause()?;
        let _clause_name = match &clause {
            Clause::Match(_) => "MATCH",
            Clause::Where(_) => "WHERE",
            Clause::Return(_) => "RETURN",
            Clause::Create(_) => "CREATE",
        };
        stmt.clauses.push(clause);
        parser.skip_whitespace();
        if parser.peek_char() == Some(';') {
            parser.advance(); // optional statement terminator
            parser.skip_whitespace();
        }
    }

    if stmt.clauses.is_empty() {
        return Err(parser.error("empty statement"));
    }

    Ok(stmt)
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
        ParseError {
            message: msg.to_string(),
            offset: self.pos,
            line: self.line,
            column: self.column,
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

        let start = self.pos;
        let keyword = self.read_keyword();
        match keyword.to_ascii_uppercase().as_str() {
            "MATCH" => {
                self.skip_whitespace();
                let pattern = self.parse_pattern()?;
                Ok(Clause::Match(MatchClause { pattern }))
            }
            "WHERE" => {
                self.skip_whitespace();
                let predicate = self.parse_expression(0)?;
                Ok(Clause::Where(WhereClause { predicate }))
            }
            "RETURN" => {
                self.skip_whitespace();
                let (projections, order_by, skip, limit) = self.parse_return_body()?;
                Ok(Clause::Return(ReturnClause {
                    projections,
                    order_by,
                    skip,
                    limit,
                }))
            }
            "CREATE" => {
                self.skip_whitespace();
                let pattern = self.parse_pattern()?;
                Ok(Clause::Create(CreateClause { pattern }))
            }
            _ => {
                self.pos = start;
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

            projections.push(Projection {
                expression: expr,
                alias,
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
            let expr = self.parse_expression(0)?;
            self.skip_whitespace();
            let ascending = match self.read_keyword().to_ascii_uppercase().as_str() {
                "DESC" => false,
                "ASC" => true,
                _ => true,
            };
            items.push(OrderItem { expression: expr, ascending });
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
        Ok(Pattern { elements })
    }

    fn parse_node_pattern(&mut self) -> Result<NodePattern, ParseError> {
        self.skip_whitespace();
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

        Ok(NodePattern {
            variable,
            labels,
            properties,
        })
    }

    fn parse_relationship_pattern(&mut self) -> Result<RelationshipPattern, ParseError> {
        self.skip_whitespace();
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

        Ok(RelationshipPattern {
            direction,
            types,
            variable,
            properties,
            length: PathLength::Fixed(1),
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

    // Pratt parser for expressions.
    fn parse_expression(&mut self,
        min_bp: u8,
    ) -> Result<Expression, ParseError> {
        self.skip_whitespace();
        let mut lhs = self.parse_primary()?;

        loop {
            self.skip_whitespace();
            let op = match self.peek_operator() {
                Some(op) => op,
                None => break,
            };
            let (lbp, rbp) = infix_binding_power(&op);
            if lbp < min_bp {
                break;
            }
            self.advance_operator(&op)?;
            self.skip_whitespace();
            let rhs = self.parse_expression(rbp)?;
            if let Some(bin_op) = arithmetic_op(&op) {
                lhs = Expression::BinaryOp {
                    op: bin_op,
                    left: Box::new(lhs),
                    right: Box::new(rhs),
                };
            } else {
                lhs = Expression::Comparison {
                    op: comparison_op(&op),
                    left: Box::new(lhs),
                    right: Box::new(rhs),
                };
            }
        }

        // Handle AND / OR (lowest precedence)
        loop {
            self.skip_whitespace();
            let start_pos = self.pos;
            let start_line = self.line;
            let start_col = self.column;
            let kw = self.read_keyword().to_ascii_uppercase();
            match kw.as_str() {
                "AND" => {
                    self.skip_whitespace();
                    let rhs = self.parse_expression(0)?;
                    lhs = Expression::BinaryOp {
                        op: BinaryOperator::Mul, // reuse Mul as AND placeholder
                        left: Box::new(lhs),
                        right: Box::new(rhs),
                    };
                }
                "OR" => {
                    self.skip_whitespace();
                    let rhs = self.parse_expression(0)?;
                    lhs = Expression::BinaryOp {
                        op: BinaryOperator::Add, // reuse Add as OR placeholder
                        left: Box::new(lhs),
                        right: Box::new(rhs),
                    };
                }
                _ => {
                    // Backtrack: restore position so the caller can parse the keyword.
                    self.pos = start_pos;
                    self.line = start_line;
                    self.column = start_col;
                    break;
                }
            }
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
                Ok(Expression::UnaryOp {
                    op: UnaryOperator::Neg,
                    expr: Box::new(expr),
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
                    Ok(Expression::PropertyAccess {
                        base: Box::new(Expression::Variable(ident)),
                        property: prop,
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
                    Ok(Expression::FunctionCall {
                        name: ident,
                        args,
                        distinct: false, // TODO: propagate distinct correctly
                    })
                } else {
                    match ident.to_ascii_uppercase().as_str() {
                        "TRUE" => Ok(Expression::Literal(Literal::Boolean(true))),
                        "FALSE" => Ok(Expression::Literal(Literal::Boolean(false))),
                        "NULL" => Ok(Expression::Literal(Literal::Null)),
                        "NOT" => {
                            self.skip_whitespace();
                            let expr = self.parse_primary()?;
                            Ok(Expression::UnaryOp {
                                op: UnaryOperator::Not,
                                expr: Box::new(expr),
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
        if self.peek_char() != Some('[') {
            return Err(self.error("expected '[' to start list literal"));
        }
        self.advance(); // consume '['
        self.skip_whitespace();

        let mut items = Vec::new();
        if self.peek_char() == Some(']') {
            self.advance();
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
        Ok(Expression::List(items))
    }

    fn parse_map_literal(&mut self) -> Result<Expression, ParseError> {
        self.skip_whitespace();
        if self.peek_char() != Some('{') {
            return Err(self.error("expected '{' to start map literal"));
        }
        self.advance(); // consume '{'
        self.skip_whitespace();

        let mut entries = Vec::new();
        if self.peek_char() == Some('}') {
            self.advance();
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
        Ok(Expression::Map(entries))
    }

    fn parse_string_literal(&mut self) -> Result<Expression, ParseError> {
        let quote = self.advance().unwrap();
        let mut s = String::new();
        while let Some(c) = self.peek_char() {
            if c == quote {
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
            if "<>=!+-*/%".contains(c) {
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
        "=" | "<>" => (1, 2),
        "<" | ">" | "<=" | ">=" => (3, 4),
        "+" | "-" => (5, 6),
        "*" | "/" | "%" => (7, 8),
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

    #[test]
    fn parse_simple_match_return() {
        let stmt = parse("MATCH (n:Person)-[:KNOWS]->(m:Person) RETURN n, m").unwrap();
        assert_eq!(stmt.clauses.len(), 2);
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
}
