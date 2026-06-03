//! Abstract Syntax Tree for openCypher queries.
//!
//! The AST is intentionally minimal: it supports the clauses and expressions
//! required for Sprint 8 (fixed-length MATCH, WHERE, RETURN) and can be
//! extended later for write clauses, aggregations, and sub-queries.

use crate::graph::property::{OrderedF64, Property};
use std::collections::HashMap;

/// A top-level Cypher statement (e.g. a full query).
#[derive(Debug, Clone, PartialEq)]
pub struct Statement {
    pub clauses: Vec<Clause>,
}

/// A single clause in a Cypher statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Clause {
    Match(MatchClause),
    Return(ReturnClause),
    Where(WhereClause),
    Create(CreateClause),
    // TODO: DELETE, SET, WITH, UNWIND, etc.
}

/// `MATCH (pattern)` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchClause {
    pub pattern: Pattern,
}

/// `RETURN projection_list` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct ReturnClause {
    pub projections: Vec<Projection>,
    pub order_by: Vec<OrderItem>,
    pub skip: Option<Expression>,
    pub limit: Option<Expression>,
}

/// `WHERE expression` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct WhereClause {
    pub predicate: Expression,
}

/// `CREATE (pattern)` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateClause {
    pub pattern: Pattern,
}

/// A pattern is an alternating sequence of nodes and relationships.
#[derive(Debug, Clone, PartialEq)]
pub struct Pattern {
    pub elements: Vec<PatternElement>,
}

/// One element in a pattern chain.
#[derive(Debug, Clone, PartialEq)]
pub enum PatternElement {
    Node(NodePattern),
    Relationship(RelationshipPattern),
}

/// `(variable:Label {prop: value})`
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NodePattern {
    pub variable: Option<String>,
    pub labels: Vec<String>,
    pub properties: HashMap<String, Expression>,
}

/// `-[:TYPE]->` or `<-[:TYPE]-`
#[derive(Debug, Clone, PartialEq)]
pub struct RelationshipPattern {
    pub direction: Direction,
    pub types: Vec<String>,
    pub variable: Option<String>,
    pub properties: HashMap<String, Expression>,
    pub length: PathLength,
}

/// Direction of a relationship in a pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Outgoing,  // ->
    Incoming,  // <-
    Both,      // -
}

/// Fixed or variable path length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathLength {
    Fixed(u32),
    Variable, // TODO: support min..max
}

impl Default for PathLength {
    fn default() -> Self {
        PathLength::Fixed(1)
    }
}

/// `expression AS alias` in a RETURN clause.
#[derive(Debug, Clone, PartialEq)]
pub struct Projection {
    pub expression: Expression,
    pub alias: Option<String>,
}

/// `expression ASC|DESC` in ORDER BY.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderItem {
    pub expression: Expression,
    pub ascending: bool,
}

/// Expressions supported by the Sprint 8 subset.
#[derive(Debug, Clone, PartialEq)]
pub enum Expression {
    Literal(Literal),
    Variable(String),
    PropertyAccess { base: Box<Expression>, property: String },
    BinaryOp { op: BinaryOperator, left: Box<Expression>, right: Box<Expression> },
    Comparison { op: ComparisonOperator, left: Box<Expression>, right: Box<Expression> },
    UnaryOp { op: UnaryOperator, expr: Box<Expression> },
    IsNull(Box<Expression>),
    IsNotNull(Box<Expression>),
    List(Vec<Expression>),
    Map(Vec<(String, Expression)>),
    /// Function call: `name(args...)`.
    FunctionCall {
        name: String,
        args: Vec<Expression>,
        distinct: bool,
    },
    /// Wildcard `*` used inside `count(*)`.
    Wildcard,
}

/// Binary arithmetic operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOperator {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
}

/// Comparison operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComparisonOperator {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// Unary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOperator {
    Not,
    Neg,
}

/// Literal values.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Null,
    Boolean(bool),
    Integer(i64),
    Float(f64),
    String(String),
    // TODO: Date, Duration, Point
}

impl Statement {
    /// Create a new empty statement.
    pub fn new() -> Self {
        Self { clauses: Vec::new() }
    }

    /// Append a clause.
    pub fn with_clause(mut self, clause: Clause) -> Self {
        self.clauses.push(clause);
        self
    }
}

impl Default for Statement {
    fn default() -> Self {
        Self::new()
    }
}

impl Literal {
    /// Convert to a domain [`Property`] value.
    pub fn to_property(&self) -> Property {
        match self {
            Literal::Null => Property::Null,
            Literal::Boolean(b) => Property::Boolean(*b),
            Literal::Integer(v) => Property::Integer(*v),
            Literal::Float(v) => Property::Float(OrderedF64(*v)),
            Literal::String(s) => Property::String(s.clone()),
        }
    }
}

// ------------------------------------------------------------------
// Display impls for debugging
// ------------------------------------------------------------------

impl std::fmt::Display for Statement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for clause in &self.clauses {
            writeln!(f, "{}", clause)?;
        }
        Ok(())
    }
}

impl std::fmt::Display for Clause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Clause::Match(m) => write!(f, "MATCH {}", m.pattern),
            Clause::Return(r) => {
                write!(f, "RETURN ")?;
                let items: Vec<String> = r.projections.iter().map(|p| p.to_string()).collect();
                write!(f, "{}", items.join(", "))?;
                if !r.order_by.is_empty() {
                    let order: Vec<String> = r.order_by.iter().map(|o| o.to_string()).collect();
                    write!(f, " ORDER BY {}", order.join(", "))?;
                }
                if let Some(skip) = &r.skip {
                    write!(f, " SKIP {}", skip)?;
                }
                if let Some(limit) = &r.limit {
                    write!(f, " LIMIT {}", limit)?;
                }
                Ok(())
            }
            Clause::Where(w) => write!(f, "WHERE {}", w.predicate),
            Clause::Create(c) => write!(f, "CREATE {}", c.pattern),
        }
    }
}

impl std::fmt::Display for Pattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, elem) in self.elements.iter().enumerate() {
            if i > 0 {
                write!(f, " ")?;
            }
            write!(f, "{}", elem)?;
        }
        Ok(())
    }
}

impl std::fmt::Display for PatternElement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PatternElement::Node(n) => write!(f, "{}", n),
            PatternElement::Relationship(r) => write!(f, "{}", r),
        }
    }
}

impl std::fmt::Display for NodePattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "(")?;
        if let Some(v) = &self.variable {
            write!(f, "{}", v)?;
        }
        for label in &self.labels {
            write!(f, ":{}", label)?;
        }
        if !self.properties.is_empty() {
            let props: Vec<String> = self.properties.iter()
                .map(|(k, v)| format!("{}: {}", k, v))
                .collect();
            write!(f, " {{{}}}", props.join(", "))?;
        }
        write!(f, ")")
    }
}

impl std::fmt::Display for RelationshipPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let _arrow = match self.direction {
            Direction::Outgoing => "->",
            Direction::Incoming => "<-",
            Direction::Both => "-",
        };
        let left = if self.direction == Direction::Incoming { "<" } else { "" };
        let right = if self.direction == Direction::Outgoing { ">" } else { "" };

        write!(f, "{}{}[:", left, if self.direction == Direction::Both { "" } else { "-" })?;
        if let Some(v) = &self.variable {
            write!(f, "{}", v)?;
        }
        for t in &self.types {
            write!(f, ":{}", t)?;
        }
        if !self.properties.is_empty() {
            let props: Vec<String> = self.properties.iter()
                .map(|(k, v)| format!("{}: {}", k, v))
                .collect();
            write!(f, " {{{}}}", props.join(", "))?;
        }
        write!(f, "]{}", right)
    }
}

impl std::fmt::Display for Expression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Expression::Literal(l) => write!(f, "{}", l),
            Expression::Variable(v) => write!(f, "{}", v),
            Expression::PropertyAccess { base, property } => write!(f, "{}.{}", base, property),
            Expression::BinaryOp { op, left, right } => write!(f, "({} {} {})", left, op, right),
            Expression::Comparison { op, left, right } => write!(f, "({} {} {})", left, op, right),
            Expression::UnaryOp { op, expr } => write!(f, "{}{}", op, expr),
            Expression::IsNull(e) => write!(f, "{} IS NULL", e),
            Expression::IsNotNull(e) => write!(f, "{} IS NOT NULL", e),
            Expression::List(items) => {
                let elems: Vec<String> = items.iter().map(|e| e.to_string()).collect();
                write!(f, "[{}]", elems.join(", "))
            }
            Expression::Map(entries) => {
                let elems: Vec<String> = entries.iter()
                    .map(|(k, v)| format!("{}: {}", k, v))
                    .collect();
                write!(f, "{{{}}}", elems.join(", "))
            }
            Expression::FunctionCall { name, args, distinct } => {
                let prefix = if *distinct { "DISTINCT " } else { "" };
                let elems: Vec<String> = args.iter().map(|a| a.to_string()).collect();
                write!(f, "{}{}({})", prefix, name, elems.join(", "))
            }
            Expression::Wildcard => write!(f, "*"),
        }
    }
}

impl std::fmt::Display for Literal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Literal::Null => write!(f, "NULL"),
            Literal::Boolean(b) => write!(f, "{}", b),
            Literal::Integer(v) => write!(f, "{}", v),
            Literal::Float(v) => write!(f, "{}", v),
            Literal::String(s) => write!(f, "'{}'", s),
        }
    }
}

impl std::fmt::Display for BinaryOperator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            BinaryOperator::Add => "+",
            BinaryOperator::Sub => "-",
            BinaryOperator::Mul => "*",
            BinaryOperator::Div => "/",
            BinaryOperator::Mod => "%",
            BinaryOperator::Pow => "^",
        };
        write!(f, "{}", s)
    }
}

impl std::fmt::Display for ComparisonOperator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ComparisonOperator::Eq => "=",
            ComparisonOperator::Ne => "<>",
            ComparisonOperator::Lt => "<",
            ComparisonOperator::Le => "<=",
            ComparisonOperator::Gt => ">",
            ComparisonOperator::Ge => ">=",
        };
        write!(f, "{}", s)
    }
}

impl std::fmt::Display for UnaryOperator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            UnaryOperator::Not => "NOT ",
            UnaryOperator::Neg => "-",
        };
        write!(f, "{}", s)
    }
}

impl std::fmt::Display for Projection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(alias) = &self.alias {
            write!(f, "{} AS {}", self.expression, alias)
        } else {
            write!(f, "{}", self.expression)
        }
    }
}

impl std::fmt::Display for OrderItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.ascending {
            write!(f, "{}", self.expression)
        } else {
            write!(f, "{} DESC", self.expression)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ast_statement_roundtrip() {
        let stmt = Statement::new()
            .with_clause(Clause::Match(MatchClause {
                pattern: Pattern {
                    elements: vec![
                        PatternElement::Node(NodePattern {
                            variable: Some("n".to_string()),
                            labels: vec!["Person".to_string()],
                            properties: HashMap::new(),
                        }),
                        PatternElement::Relationship(RelationshipPattern {
                            direction: Direction::Outgoing,
                            types: vec!["KNOWS".to_string()],
                            variable: None,
                            properties: HashMap::new(),
                            length: PathLength::Fixed(1),
                        }),
                        PatternElement::Node(NodePattern {
                            variable: Some("m".to_string()),
                            labels: vec!["Person".to_string()],
                            properties: HashMap::new(),
                        }),
                    ],
                },
            }))
            .with_clause(Clause::Where(WhereClause {
                predicate: Expression::Comparison {
                    op: ComparisonOperator::Eq,
                    left: Box::new(Expression::PropertyAccess {
                        base: Box::new(Expression::Variable("n".to_string())),
                        property: "name".to_string(),
                    }),
                    right: Box::new(Expression::Literal(Literal::String("Alice".to_string()))),
                },
            }))
            .with_clause(Clause::Return(ReturnClause {
                projections: vec![
                    Projection {
                        expression: Expression::Variable("n".to_string()),
                        alias: None,
                    },
                    Projection {
                        expression: Expression::Variable("m".to_string()),
                        alias: None,
                    },
                ],
                order_by: vec![],
                skip: None,
                limit: None,
            }));

        assert_eq!(stmt.clauses.len(), 3);
    }

    #[test]
    fn literal_to_property_conversion() {
        assert_eq!(Literal::Integer(42).to_property(), Property::Integer(42));
        assert_eq!(Literal::String("hello".to_string()).to_property(), Property::String("hello".to_string()));
        assert_eq!(Literal::Boolean(true).to_property(), Property::Boolean(true));
        assert!(Literal::Null.to_property().is_null());
    }

    #[test]
    fn display_roundtrip() {
        let node = NodePattern {
            variable: Some("n".to_string()),
            labels: vec!["Person".to_string()],
            properties: HashMap::new(),
        };
        assert_eq!(node.to_string(), "(n:Person)");
    }
}
