//! Abstract Syntax Tree for openCypher queries.
//!
//! The AST covers the full openCypher surface area required for Sprint C:
//! all read clauses (MATCH, OPTIONAL MATCH, WITH, UNWIND, UNION, CALL),
//! write clauses (CREATE, SET, REMOVE, DELETE, MERGE, FOREACH), and a
//! complete expression language (CASE, comprehensions, quantifiers, parameters,
//! temporal/spatial literals, EXISTS{}).
//!
//! Every AST node carries an optional [`TextRange`](text_size::TextRange)
//! representing its source span in the original query text.

use crate::graph::property::{OrderedF64, Property};
use std::collections::HashMap;
use text_size::TextRange;

// ─────────────────────────────────────────────────────────────────────────────
// Top-level
// ─────────────────────────────────────────────────────────────────────────────

/// A top-level Cypher statement (may include UNION chains).
#[derive(Debug, Clone, PartialEq)]
pub struct Statement {
    pub clauses: Vec<Clause>,
    /// Source span covering the entire statement text.
    pub span: Option<TextRange>,
}

impl Statement {
    pub fn new() -> Self {
        Self { clauses: Vec::new(), span: None }
    }

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

// ─────────────────────────────────────────────────────────────────────────────
// Clauses
// ─────────────────────────────────────────────────────────────────────────────

/// A single clause in a Cypher statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Clause {
    Match(MatchClause),
    OptionalMatch(MatchClause),
    Return(ReturnClause),
    Where(WhereClause),
    Create(CreateClause),
    Delete(DeleteClause),
    Set(SetClause),
    Remove(RemoveClause),
    Merge(MergeClause),
    With(WithClause),
    Unwind(UnwindClause),
    Union(UnionClause),
    Call(CallClause),
    Foreach(ForeachClause),
}

impl Clause {
    pub fn span(&self) -> Option<TextRange> {
        match self {
            Clause::Match(c) => c.span,
            Clause::OptionalMatch(c) => c.span,
            Clause::Return(c) => c.span,
            Clause::Where(c) => c.span,
            Clause::Create(c) => c.span,
            Clause::Delete(c) => c.span,
            Clause::Set(c) => c.span,
            Clause::Remove(c) => c.span,
            Clause::Merge(c) => c.span,
            Clause::With(c) => c.span,
            Clause::Unwind(c) => c.span,
            Clause::Union(c) => c.span,
            Clause::Call(c) => c.span,
            Clause::Foreach(c) => c.span,
        }
    }
}

/// `MATCH (pattern)` clause (also used for OPTIONAL MATCH).
#[derive(Debug, Clone, PartialEq)]
pub struct MatchClause {
    /// One or more comma-separated patterns.
    pub patterns: Vec<NamedPattern>,
    pub span: Option<TextRange>,
}

// Legacy compat shim: the old field name was `pattern` (single pattern).
impl MatchClause {
    /// Convenience getter — returns the first pattern's inner pattern.
    pub fn pattern(&self) -> &Pattern {
        &self.patterns[0].pattern
    }
}

/// A pattern optionally bound to a path variable: `p = (a)-[r]->(b)`.
#[derive(Debug, Clone, PartialEq)]
pub struct NamedPattern {
    /// Optional path variable binding (`p = ...`).
    pub variable: Option<String>,
    pub pattern: Pattern,
}

/// `RETURN projection_list [DISTINCT] [ORDER BY ...] [SKIP n] [LIMIT n]`
#[derive(Debug, Clone, PartialEq)]
pub struct ReturnClause {
    pub distinct: bool,
    /// When `star` is true the projection list is `*` (all in-scope variables).
    pub star: bool,
    pub projections: Vec<Projection>,
    pub order_by: Vec<OrderItem>,
    pub skip: Option<Expression>,
    pub limit: Option<Expression>,
    pub span: Option<TextRange>,
}

/// `WHERE expression` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct WhereClause {
    pub predicate: Expression,
    pub span: Option<TextRange>,
}

/// `CREATE (pattern)` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateClause {
    pub patterns: Vec<NamedPattern>,
    pub span: Option<TextRange>,
}

impl CreateClause {
    /// Convenience getter — returns the first pattern's inner pattern.
    pub fn pattern(&self) -> &Pattern {
        &self.patterns[0].pattern
    }
}

/// `DELETE expression_list` (optionally DETACH).
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteClause {
    pub expressions: Vec<Expression>,
    pub detach: bool,
    pub span: Option<TextRange>,
}

/// `SET item_list`
#[derive(Debug, Clone, PartialEq)]
pub struct SetClause {
    pub items: Vec<SetItem>,
    pub span: Option<TextRange>,
}

/// `REMOVE item_list`
#[derive(Debug, Clone, PartialEq)]
pub struct RemoveClause {
    pub items: Vec<RemoveItem>,
    pub span: Option<TextRange>,
}

/// `MERGE (pattern) [ON CREATE SET ...] [ON MATCH SET ...]`
#[derive(Debug, Clone, PartialEq)]
pub struct MergeClause {
    pub pattern: Pattern,
    pub on_create: Vec<SetItem>,
    pub on_match: Vec<SetItem>,
    pub span: Option<TextRange>,
}

/// `WITH projection_list [WHERE predicate]` — scope barrier and variable rename.
#[derive(Debug, Clone, PartialEq)]
pub struct WithClause {
    pub distinct: bool,
    pub star: bool,
    pub projections: Vec<Projection>,
    pub order_by: Vec<OrderItem>,
    pub skip: Option<Expression>,
    pub limit: Option<Expression>,
    /// Optional WHERE predicate applied after the projection.
    pub where_: Option<Expression>,
    pub span: Option<TextRange>,
}

/// `UNWIND expression AS variable`
#[derive(Debug, Clone, PartialEq)]
pub struct UnwindClause {
    pub expression: Expression,
    pub variable: String,
    pub span: Option<TextRange>,
}

/// `UNION [ALL]` — combine two query parts.
#[derive(Debug, Clone, PartialEq)]
pub struct UnionClause {
    pub all: bool,
    pub span: Option<TextRange>,
}

/// `CALL procedure_name(args) YIELD ...` or `CALL { subquery }`.
#[derive(Debug, Clone, PartialEq)]
pub struct CallClause {
    /// None if this is a `CALL { subquery }`.
    pub procedure: Option<String>,
    pub args: Vec<Expression>,
    pub yield_items: Vec<Projection>,
    /// Subquery body for `CALL { ... }`.
    pub subquery: Option<Vec<Clause>>,
    pub span: Option<TextRange>,
}

/// `FOREACH (variable IN expression | write_clauses...)`
#[derive(Debug, Clone, PartialEq)]
pub struct ForeachClause {
    pub variable: String,
    pub expression: Expression,
    pub body: Vec<Clause>,
    pub span: Option<TextRange>,
}

// ─────────────────────────────────────────────────────────────────────────────
// SET / REMOVE items
// ─────────────────────────────────────────────────────────────────────────────

/// A single item inside a SET clause.
#[derive(Debug, Clone, PartialEq)]
pub enum SetItem {
    /// `variable.prop = expr`
    Property { target: Box<Expression>, value: Expression },
    /// `variable:Label`
    Label { variable: String, labels: Vec<String> },
    /// `variable += {map}` — merge properties.
    Merge { variable: String, value: Expression },
    /// `variable = {map}` — replace all properties.
    Replace { variable: String, value: Expression },
}

/// A single item inside a REMOVE clause.
#[derive(Debug, Clone, PartialEq)]
pub enum RemoveItem {
    /// `variable.prop`
    Property { target: Box<Expression> },
    /// `variable:Label`
    Label { variable: String, labels: Vec<String> },
}

// ─────────────────────────────────────────────────────────────────────────────
// Patterns
// ─────────────────────────────────────────────────────────────────────────────

/// A pattern is an alternating sequence of nodes and relationships.
#[derive(Debug, Clone, PartialEq)]
pub struct Pattern {
    pub elements: Vec<PatternElement>,
    pub span: Option<TextRange>,
}

/// One element in a pattern chain.
#[derive(Debug, Clone, PartialEq)]
pub enum PatternElement {
    Node(NodePattern),
    Relationship(RelationshipPattern),
}

impl PatternElement {
    pub fn span(&self) -> Option<TextRange> {
        match self {
            PatternElement::Node(n) => n.span,
            PatternElement::Relationship(r) => r.span,
        }
    }
}

/// `(variable:Label {prop: value})`
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NodePattern {
    pub variable: Option<String>,
    /// Label alternatives: `:A|B` is represented as `vec!["A", "B"]`.
    pub labels: Vec<String>,
    pub properties: HashMap<String, Expression>,
    pub span: Option<TextRange>,
}

/// `-[:TYPE]->` or `<-[:TYPE]-`
#[derive(Debug, Clone, PartialEq)]
pub struct RelationshipPattern {
    pub direction: Direction,
    /// Relationship type alternatives.
    pub types: Vec<String>,
    pub variable: Option<String>,
    pub properties: HashMap<String, Expression>,
    pub length: PathLength,
    pub span: Option<TextRange>,
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
    /// Exactly one hop (default).
    Fixed(u32),
    /// Variable hops with optional min and max bounds.
    /// `[*]` → Range(1, None), `[*2]` → Range(2, Some(2)),
    /// `[*2..5]` → Range(2, Some(5)), `[*..5]` → Range(1, Some(5)).
    Range(u32, Option<u32>),
}

impl Default for PathLength {
    fn default() -> Self {
        PathLength::Fixed(1)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Return / WITH body
// ─────────────────────────────────────────────────────────────────────────────

/// `expression AS alias` in a RETURN/WITH clause.
#[derive(Debug, Clone, PartialEq)]
pub struct Projection {
    pub expression: Expression,
    pub alias: Option<String>,
    pub span: Option<TextRange>,
}

/// `expression ASC|DESC` in ORDER BY.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderItem {
    pub expression: Expression,
    pub ascending: bool,
    pub span: Option<TextRange>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Expressions
// ─────────────────────────────────────────────────────────────────────────────

/// All Cypher expressions.
#[derive(Debug, Clone, PartialEq)]
pub enum Expression {
    Literal(Literal),
    Variable(String),
    Parameter(String),
    PropertyAccess { base: Box<Expression>, property: String, span: Option<TextRange> },
    /// Dynamic property access: `map[$key]`.
    DynamicPropertyAccess { base: Box<Expression>, index: Box<Expression>, span: Option<TextRange> },
    /// Slice: `list[from..to]`.
    Slice { base: Box<Expression>, from: Option<Box<Expression>>, to: Option<Box<Expression>>, span: Option<TextRange> },
    BinaryOp { op: BinaryOperator, left: Box<Expression>, right: Box<Expression>, span: Option<TextRange> },
    Comparison { op: ComparisonOperator, left: Box<Expression>, right: Box<Expression>, span: Option<TextRange> },
    UnaryOp { op: UnaryOperator, expr: Box<Expression>, span: Option<TextRange> },
    IsNull(Box<Expression>),
    IsNotNull(Box<Expression>),
    List(Vec<Expression>),
    Map(Vec<(String, Expression)>),
    FunctionCall {
        name: String,
        args: Vec<Expression>,
        distinct: bool,
        span: Option<TextRange>,
    },
    And { left: Box<Expression>, right: Box<Expression>, span: Option<TextRange> },
    Or { left: Box<Expression>, right: Box<Expression>, span: Option<TextRange> },
    Xor { left: Box<Expression>, right: Box<Expression>, span: Option<TextRange> },
    Not { expr: Box<Expression>, span: Option<TextRange> },
    StartsWith { left: Box<Expression>, right: Box<Expression>, span: Option<TextRange> },
    EndsWith { left: Box<Expression>, right: Box<Expression>, span: Option<TextRange> },
    Contains { left: Box<Expression>, right: Box<Expression>, span: Option<TextRange> },
    In { left: Box<Expression>, right: Box<Expression>, span: Option<TextRange> },
    Regex { left: Box<Expression>, right: Box<Expression>, span: Option<TextRange> },
    Wildcard,
    /// `CASE subject WHEN ... THEN ... [ELSE ...] END` (simple form).
    Case {
        subject: Option<Box<Expression>>,
        alternatives: Vec<CaseAlternative>,
        default: Option<Box<Expression>>,
        span: Option<TextRange>,
    },
    /// List comprehension: `[x IN list WHERE pred | expr]`.
    ListComprehension {
        variable: String,
        source: Box<Expression>,
        filter: Option<Box<Expression>>,
        projection: Option<Box<Expression>>,
        span: Option<TextRange>,
    },
    /// Pattern comprehension: `[(a)-[r]->(b) | expr]`.
    PatternComprehension {
        variable: Option<String>,
        pattern: Pattern,
        filter: Option<Box<Expression>>,
        projection: Box<Expression>,
        span: Option<TextRange>,
    },
    /// `reduce(acc = init, x IN list | expr)`.
    Reduce {
        accumulator: String,
        init: Box<Expression>,
        variable: String,
        source: Box<Expression>,
        body: Box<Expression>,
        span: Option<TextRange>,
    },
    /// Quantifier predicates: `ALL`, `ANY`, `NONE`, `SINGLE`.
    Quantifier {
        kind: QuantifierKind,
        variable: String,
        source: Box<Expression>,
        filter: Box<Expression>,
        span: Option<TextRange>,
    },
    /// `EXISTS { subquery }` or `EXISTS pattern`.
    Exists {
        subquery: Option<Vec<Clause>>,
        pattern: Option<Pattern>,
        span: Option<TextRange>,
    },
}

impl Expression {
    pub fn span(&self) -> Option<TextRange> {
        match self {
            Expression::Literal(l) => l.span(),
            Expression::Variable(_) | Expression::Parameter(_) => None,
            Expression::PropertyAccess { span, .. } => *span,
            Expression::DynamicPropertyAccess { span, .. } => *span,
            Expression::Slice { span, .. } => *span,
            Expression::BinaryOp { span, .. } => *span,
            Expression::Comparison { span, .. } => *span,
            Expression::UnaryOp { span, .. } => *span,
            Expression::IsNull(_) | Expression::IsNotNull(_) => None,
            Expression::List(_) => None,
            Expression::Map(_) => None,
            Expression::FunctionCall { span, .. } => *span,
            Expression::And { span, .. } => *span,
            Expression::Or { span, .. } => *span,
            Expression::Xor { span, .. } => *span,
            Expression::Not { span, .. } => *span,
            Expression::StartsWith { span, .. } => *span,
            Expression::EndsWith { span, .. } => *span,
            Expression::Contains { span, .. } => *span,
            Expression::In { span, .. } => *span,
            Expression::Regex { span, .. } => *span,
            Expression::Wildcard => None,
            Expression::Case { span, .. } => *span,
            Expression::ListComprehension { span, .. } => *span,
            Expression::PatternComprehension { span, .. } => *span,
            Expression::Reduce { span, .. } => *span,
            Expression::Quantifier { span, .. } => *span,
            Expression::Exists { span, .. } => *span,
        }
    }
}

/// CASE alternative: `WHEN condition THEN result`.
#[derive(Debug, Clone, PartialEq)]
pub struct CaseAlternative {
    pub condition: Expression,
    pub result: Expression,
}

/// Quantifier predicate kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuantifierKind {
    All,
    Any,
    None,
    Single,
}

// ─────────────────────────────────────────────────────────────────────────────
// Operators
// ─────────────────────────────────────────────────────────────────────────────

/// Binary arithmetic operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOperator {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Concat, // string concatenation (+) — same as Add, used for disambiguation
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

// ─────────────────────────────────────────────────────────────────────────────
// Literals
// ─────────────────────────────────────────────────────────────────────────────

/// Literal values (lexer-level).
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Null,
    Boolean(bool),
    Integer(i64),
    Float(f64),
    String(String),
    // Temporal literals (the string is the ISO 8601 value).
    Date(String),
    Time(String),
    LocalTime(String),
    DateTime(String),
    LocalDateTime(String),
    Duration(String),
}

impl Literal {
    pub fn span(&self) -> Option<TextRange> {
        None
    }

    /// Convert to a domain [`Property`] value.
    pub fn to_property(&self) -> Property {
        match self {
            Literal::Null => Property::Null,
            Literal::Boolean(b) => Property::Boolean(*b),
            Literal::Integer(v) => Property::Integer(*v),
            Literal::Float(v) => Property::Float(OrderedF64(*v)),
            Literal::String(s) => Property::String(s.clone()),
            // Temporal literals stored as strings in the property layer for now.
            Literal::Date(s)
            | Literal::Time(s)
            | Literal::LocalTime(s)
            | Literal::DateTime(s)
            | Literal::LocalDateTime(s)
            | Literal::Duration(s) => Property::String(s.clone()),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Display implementations
// ─────────────────────────────────────────────────────────────────────────────

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
            Clause::Match(m) | Clause::OptionalMatch(m) => {
                if matches!(self, Clause::OptionalMatch(_)) {
                    write!(f, "OPTIONAL ")?;
                }
                write!(f, "MATCH ")?;
                for (i, np) in m.patterns.iter().enumerate() {
                    if i > 0 { write!(f, ", ")?; }
                    if let Some(v) = &np.variable {
                        write!(f, "{} = ", v)?;
                    }
                    write!(f, "{}", np.pattern)?;
                }
                Ok(())
            }
            Clause::Return(r) => {
                write!(f, "RETURN ")?;
                if r.distinct { write!(f, "DISTINCT ")?; }
                if r.star { write!(f, "*")?; } else {
                    let items: Vec<String> = r.projections.iter().map(|p| p.to_string()).collect();
                    write!(f, "{}", items.join(", "))?;
                }
                if !r.order_by.is_empty() {
                    let order: Vec<String> = r.order_by.iter().map(|o| o.to_string()).collect();
                    write!(f, " ORDER BY {}", order.join(", "))?;
                }
                if let Some(skip) = &r.skip { write!(f, " SKIP {}", skip)?; }
                if let Some(limit) = &r.limit { write!(f, " LIMIT {}", limit)?; }
                Ok(())
            }
            Clause::Where(w) => write!(f, "WHERE {}", w.predicate),
            Clause::Create(c) => {
                write!(f, "CREATE ")?;
                for (i, np) in c.patterns.iter().enumerate() {
                    if i > 0 { write!(f, ", ")?; }
                    write!(f, "{}", np.pattern)?;
                }
                Ok(())
            }
            Clause::Delete(d) => {
                if d.detach { write!(f, "DETACH DELETE ")?; } else { write!(f, "DELETE ")?; }
                let exprs: Vec<String> = d.expressions.iter().map(|e| e.to_string()).collect();
                write!(f, "{}", exprs.join(", "))
            }
            Clause::Set(s) => {
                write!(f, "SET ")?;
                let items: Vec<String> = s.items.iter().map(|i| i.to_string()).collect();
                write!(f, "{}", items.join(", "))
            }
            Clause::Remove(r) => {
                write!(f, "REMOVE ")?;
                let items: Vec<String> = r.items.iter().map(|i| i.to_string()).collect();
                write!(f, "{}", items.join(", "))
            }
            Clause::Merge(m) => {
                write!(f, "MERGE {}", m.pattern)?;
                if !m.on_create.is_empty() {
                    let items: Vec<String> = m.on_create.iter().map(|i| i.to_string()).collect();
                    write!(f, " ON CREATE SET {}", items.join(", "))?;
                }
                if !m.on_match.is_empty() {
                    let items: Vec<String> = m.on_match.iter().map(|i| i.to_string()).collect();
                    write!(f, " ON MATCH SET {}", items.join(", "))?;
                }
                Ok(())
            }
            Clause::With(w) => {
                write!(f, "WITH ")?;
                if w.distinct { write!(f, "DISTINCT ")?; }
                if w.star { write!(f, "*")?; } else {
                    let items: Vec<String> = w.projections.iter().map(|p| p.to_string()).collect();
                    write!(f, "{}", items.join(", "))?;
                }
                if let Some(pred) = &w.where_ { write!(f, " WHERE {}", pred)?; }
                Ok(())
            }
            Clause::Unwind(u) => write!(f, "UNWIND {} AS {}", u.expression, u.variable),
            Clause::Union(u) => {
                if u.all { write!(f, "UNION ALL") } else { write!(f, "UNION") }
            }
            Clause::Call(c) => {
                if let Some(proc) = &c.procedure {
                    write!(f, "CALL {}", proc)
                } else {
                    write!(f, "CALL {{ ... }}")
                }
            }
            Clause::Foreach(fe) => {
                write!(f, "FOREACH ({} IN {} | ...)", fe.variable, fe.expression)
            }
        }
    }
}

impl std::fmt::Display for Pattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, elem) in self.elements.iter().enumerate() {
            if i > 0 { write!(f, " ")?; }
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
        if let Some(v) = &self.variable { write!(f, "{}", v)?; }
        if !self.labels.is_empty() { write!(f, ":{}", self.labels.join("|"))?; }
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
        let left = if self.direction == Direction::Incoming { "<" } else { "" };
        let right = if self.direction == Direction::Outgoing { ">" } else { "" };
        write!(f, "{}-[", left)?;
        if let Some(v) = &self.variable { write!(f, "{}", v)?; }
        if !self.types.is_empty() { write!(f, ":{}", self.types.join("|"))?; }
        match &self.length {
            PathLength::Fixed(1) => {}
            PathLength::Fixed(n) => write!(f, "*{}", n)?,
            PathLength::Range(min, max) => {
                write!(f, "*")?;
                if *min > 1 || max.is_some() {
                    write!(f, "{}", min)?;
                    if let Some(max) = max { write!(f, "..{}", max)?; } else { write!(f, "..")?; }
                }
            }
        }
        write!(f, "]-{}", right)
    }
}

impl std::fmt::Display for Expression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Expression::Literal(l) => write!(f, "{}", l),
            Expression::Variable(v) => write!(f, "{}", v),
            Expression::Parameter(p) => write!(f, "${}", p),
            Expression::PropertyAccess { base, property, .. } => write!(f, "{}.{}", base, property),
            Expression::DynamicPropertyAccess { base, index, .. } => write!(f, "{}[{}]", base, index),
            Expression::Slice { base, from, to, .. } => {
                write!(f, "{}[", base)?;
                if let Some(fr) = from { write!(f, "{}", fr)?; }
                write!(f, "..")?;
                if let Some(t) = to { write!(f, "{}", t)?; }
                write!(f, "]")
            }
            Expression::BinaryOp { op, left, right, .. } => write!(f, "({} {} {})", left, op, right),
            Expression::Comparison { op, left, right, .. } => write!(f, "({} {} {})", left, op, right),
            Expression::UnaryOp { op, expr, .. } => write!(f, "{}{}", op, expr),
            Expression::Not { expr, .. } => write!(f, "(NOT {})", expr),
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
            Expression::And { left, right, .. } => write!(f, "({} AND {})", left, right),
            Expression::Or { left, right, .. } => write!(f, "({} OR {})", left, right),
            Expression::Xor { left, right, .. } => write!(f, "({} XOR {})", left, right),
            Expression::StartsWith { left, right, .. } => write!(f, "({} STARTS WITH {})", left, right),
            Expression::EndsWith { left, right, .. } => write!(f, "({} ENDS WITH {})", left, right),
            Expression::Contains { left, right, .. } => write!(f, "({} CONTAINS {})", left, right),
            Expression::In { left, right, .. } => write!(f, "({} IN {})", left, right),
            Expression::Regex { left, right, .. } => write!(f, "({} =~ {})", left, right),
            Expression::FunctionCall { name, args, distinct, .. } => {
                let prefix = if *distinct { "DISTINCT " } else { "" };
                let elems: Vec<String> = args.iter().map(|a| a.to_string()).collect();
                write!(f, "{}({}{})", name, prefix, elems.join(", "))
            }
            Expression::Wildcard => write!(f, "*"),
            Expression::Case { subject, alternatives, default, .. } => {
                write!(f, "CASE")?;
                if let Some(s) = subject { write!(f, " {}", s)?; }
                for alt in alternatives {
                    write!(f, " WHEN {} THEN {}", alt.condition, alt.result)?;
                }
                if let Some(d) = default { write!(f, " ELSE {}", d)?; }
                write!(f, " END")
            }
            Expression::ListComprehension { variable, source, filter, projection, .. } => {
                write!(f, "[{} IN {}", variable, source)?;
                if let Some(pred) = filter { write!(f, " WHERE {}", pred)?; }
                if let Some(proj) = projection { write!(f, " | {}", proj)?; }
                write!(f, "]")
            }
            Expression::PatternComprehension { pattern, projection, .. } => {
                write!(f, "[{} | {}]", pattern, projection)
            }
            Expression::Reduce { accumulator, init, variable, source, body, .. } => {
                write!(f, "reduce({} = {}, {} IN {} | {})", accumulator, init, variable, source, body)
            }
            Expression::Quantifier { kind, variable, source, filter, .. } => {
                let kw = match kind {
                    QuantifierKind::All => "all",
                    QuantifierKind::Any => "any",
                    QuantifierKind::None => "none",
                    QuantifierKind::Single => "single",
                };
                write!(f, "{}({} IN {} WHERE {})", kw, variable, source, filter)
            }
            Expression::Exists { subquery, pattern, .. } => {
                if subquery.is_some() {
                    write!(f, "EXISTS {{ ... }}")
                } else if let Some(p) = pattern {
                    write!(f, "EXISTS {}", p)
                } else {
                    write!(f, "EXISTS")
                }
            }
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
            Literal::Date(s) => write!(f, "date('{}')", s),
            Literal::Time(s) => write!(f, "time('{}')", s),
            Literal::LocalTime(s) => write!(f, "localtime('{}')", s),
            Literal::DateTime(s) => write!(f, "datetime('{}')", s),
            Literal::LocalDateTime(s) => write!(f, "localdatetime('{}')", s),
            Literal::Duration(s) => write!(f, "duration('{}')", s),
        }
    }
}

impl std::fmt::Display for BinaryOperator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            BinaryOperator::Add | BinaryOperator::Concat => "+",
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

impl std::fmt::Display for SetItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetItem::Property { target, value } => write!(f, "{} = {}", target, value),
            SetItem::Label { variable, labels } => {
                write!(f, "{}", variable)?;
                for label in labels { write!(f, ":{}", label)?; }
                Ok(())
            }
            SetItem::Merge { variable, value } => write!(f, "{} += {}", variable, value),
            SetItem::Replace { variable, value } => write!(f, "{} = {}", variable, value),
        }
    }
}

impl std::fmt::Display for RemoveItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RemoveItem::Property { target } => write!(f, "{}", target),
            RemoveItem::Label { variable, labels } => {
                write!(f, "{}", variable)?;
                for label in labels { write!(f, ":{}", label)?; }
                Ok(())
            }
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

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ast_statement_roundtrip() {
        let stmt = Statement::new()
            .with_clause(Clause::Match(MatchClause {
                patterns: vec![NamedPattern {
                    variable: None,
                    pattern: Pattern {
                        elements: vec![
                            PatternElement::Node(NodePattern {
                                variable: Some("n".to_string()),
                                labels: vec!["Person".to_string()],
                                properties: HashMap::new(),
                                span: None,
                            }),
                            PatternElement::Relationship(RelationshipPattern {
                                direction: Direction::Outgoing,
                                types: vec!["KNOWS".to_string()],
                                variable: None,
                                properties: HashMap::new(),
                                length: PathLength::Fixed(1),
                                span: None,
                            }),
                            PatternElement::Node(NodePattern {
                                variable: Some("m".to_string()),
                                labels: vec!["Person".to_string()],
                                properties: HashMap::new(),
                                span: None,
                            }),
                        ],
                        span: None,
                    },
                }],
                span: None,
            }))
            .with_clause(Clause::Where(WhereClause {
                predicate: Expression::Comparison {
                    op: ComparisonOperator::Eq,
                    left: Box::new(Expression::PropertyAccess {
                        base: Box::new(Expression::Variable("n".to_string())),
                        property: "name".to_string(),
                        span: None,
                    }),
                    right: Box::new(Expression::Literal(Literal::String("Alice".to_string()))),
                    span: None,
                },
                span: None,
            }))
            .with_clause(Clause::Return(ReturnClause {
                distinct: false,
                star: false,
                projections: vec![
                    Projection { expression: Expression::Variable("n".to_string()), alias: None, span: None },
                    Projection { expression: Expression::Variable("m".to_string()), alias: None, span: None },
                ],
                order_by: vec![],
                skip: None,
                limit: None,
                span: None,
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
            span: None,
        };
        assert_eq!(node.to_string(), "(n:Person)");
    }

    #[test]
    fn path_length_range_display() {
        let rel = RelationshipPattern {
            direction: Direction::Outgoing,
            types: vec!["KNOWS".to_string()],
            variable: None,
            properties: HashMap::new(),
            length: PathLength::Range(1, Some(3)),
            span: None,
        };
        let s = rel.to_string();
        assert!(s.contains("*"));
    }

    #[test]
    fn with_clause_display() {
        let clause = Clause::With(WithClause {
            distinct: false,
            star: false,
            projections: vec![
                Projection { expression: Expression::Variable("n".to_string()), alias: None, span: None },
            ],
            order_by: vec![],
            skip: None,
            limit: None,
            where_: None,
            span: None,
        });
        assert!(clause.to_string().starts_with("WITH"));
    }

    #[test]
    fn unwind_clause_display() {
        let clause = Clause::Unwind(UnwindClause {
            expression: Expression::List(vec![
                Expression::Literal(Literal::Integer(1)),
            ]),
            variable: "x".to_string(),
            span: None,
        });
        assert!(clause.to_string().starts_with("UNWIND"));
    }

    #[test]
    fn parameter_expression_display() {
        let expr = Expression::Parameter("name".to_string());
        assert_eq!(expr.to_string(), "$name");
    }

    #[test]
    fn case_expression_display() {
        let expr = Expression::Case {
            subject: None,
            alternatives: vec![CaseAlternative {
                condition: Expression::Literal(Literal::Boolean(true)),
                result: Expression::Literal(Literal::Integer(1)),
            }],
            default: Some(Box::new(Expression::Literal(Literal::Integer(0)))),
            span: None,
        };
        assert!(expr.to_string().contains("CASE"));
        assert!(expr.to_string().contains("WHEN"));
        assert!(expr.to_string().contains("END"));
    }
}
