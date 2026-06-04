//! Concrete Syntax Tree (CST) infrastructure powered by [`rowan`].
//!
//! This module defines the [`SyntaxKind`] enum, the [`CypherLanguage`] trait
//! implementation, and typed wrappers around [`rowan::SyntaxNode`] and
//! [`rowan::SyntaxToken`].  Every node and token carries a source
//! [`TextRange`](text_size::TextRange) so that error reporting, IDE features,
//! and TCK compliance tests can map AST elements back to the original query
//! text.
//!
//! The design follows the *Green Tree* pattern: the underlying tree is
//! immutable, reference-counted, and clone-on-write.  Partial or invalid trees
//! are represented by inserting [`SyntaxKind::ERROR`] nodes at the point of
//! failure, which means the rest of the tree can still be traversed and
//! analysed.

use rowan::{Language, SyntaxNode, SyntaxToken, TextRange};
use std::fmt;

// ------------------------------------------------------------------
// SyntaxKind — every kind of node or token in the Cypher grammar.
// ------------------------------------------------------------------

/// Kinds of syntax nodes and tokens for the Cypher grammar.
///
/// The numeric values are chosen so that leaf tokens sit in the low range
/// (0..=99) and composite nodes sit above 100.  This makes it easy to
/// distinguish tokens from interior nodes in debug output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
#[allow(non_camel_case_types)]
// SCREAMING_CASE variant names are the deliberate, idiomatic CST convention
// (cf. rust-analyzer's `SyntaxKind`): each variant mirrors a grammar token or
// node name verbatim, so renaming to CamelCase would obscure the grammar
// mapping. The acronym-casing lint is therefore suppressed for this enum only.
#[allow(clippy::upper_case_acronyms)]
pub enum SyntaxKind {
    // --- Leaf tokens (0..=99) --------------------------------------
    /// End-of-file sentinel.
    EOF = 0,
    /// Whitespace or comment (preserved for round-tripping).
    WHITESPACE = 1,
    /// A identifier: `n`, `Person`, `_foo`.
    IDENT = 2,
    /// A string literal: `'hello'`, `"world"`.
    STRING = 3,
    /// An integer literal: `42`, `-7`.
    INTEGER = 4,
    /// A floating-point literal: `3.14`.
    FLOAT = 5,
    /// A boolean literal: `true`, `false`.
    BOOLEAN = 6,
    /// The `NULL` literal.
    NULL = 7,
    /// The `*` wildcard (inside `count(*)` or `RETURN *`).
    STAR = 8,
    /// A Cypher keyword (stored as a distinct kind so that the lexer can
    /// distinguish identifiers from reserved words).
    KEYWORD = 9,
    /// Any punctuation or operator character: `(`, `)`, `[`, `]`, `{`, `}`,
    /// `:`, `->`, `<-`, `-`, `=`, `<>`, `<`, `>`, `<=`, `>=`, `+`, `-`, `*`,
    /// `/`, `%`, `^`, `.`, `,`, `;`.
    PUNCT = 10,
    /// An unrecognised token (used for error recovery).
    ERROR_TOKEN = 11,

    // --- Composite nodes (> 100) -----------------------------------
    /// The root of a Cypher statement.
    STATEMENT = 101,
    /// A single clause: `MATCH`, `WHERE`, `RETURN`, `CREATE`, etc.
    CLAUSE = 102,
    /// A `MATCH` clause.
    MATCH_CLAUSE = 103,
    /// A `WHERE` clause.
    WHERE_CLAUSE = 104,
    /// A `RETURN` clause.
    RETURN_CLAUSE = 105,
    /// A `CREATE` clause.
    CREATE_CLAUSE = 106,
    /// A pattern: alternating nodes and relationships.
    PATTERN = 107,
    /// A node pattern: `(variable:Label {prop: value})`.
    NODE_PATTERN = 108,
    /// A relationship pattern: `-[:TYPE]->`.
    REL_PATTERN = 109,
    /// A property map: `{name: 'Alice', age: 30}`.
    PROPERTY_MAP = 110,
    /// A property entry: `key: value`.
    PROPERTY_ENTRY = 111,
    /// A projection: `expression AS alias`.
    PROJECTION = 112,
    /// An `ORDER BY` item: `expression ASC|DESC`.
    ORDER_ITEM = 113,
    /// An expression.
    EXPRESSION = 114,
    /// A binary expression.
    BINARY_EXPR = 115,
    /// A unary expression.
    UNARY_EXPR = 116,
    /// A comparison expression.
    COMPARISON_EXPR = 117,
    /// A function call: `name(args...)`.
    FUNCTION_CALL = 118,
    /// An argument list: `(arg1, arg2)`.
    ARG_LIST = 119,
    /// A list literal: `[1, 2, 3]`.
    LIST_LITERAL = 120,
    /// A map literal: `{a: 1, b: 2}`.
    MAP_LITERAL = 121,
    /// A path-length specification: `[*]`, `[*1..3]`.
    PATH_LENGTH = 122,
    /// An `IS NULL` / `IS NOT NULL` postfix expression.
    IS_NULL_EXPR = 123,
    /// A property access: `node.property`.
    PROPERTY_ACCESS = 124,

    /// An error node inserted when the parser cannot make progress.
    /// Children contain the raw tokens that could not be consumed.
    ERROR = 255,
}

impl SyntaxKind {
    /// Returns `true` if this kind represents a leaf token (as opposed to a
    /// composite interior node).
    pub fn is_token(self) -> bool {
        (self as u16) <= 99
    }

    /// Returns `true` if this kind represents a composite interior node.
    pub fn is_node(self) -> bool {
        (self as u16) > 99
    }

    /// Human-readable name used in debug output and `Display` impls.
    pub fn name(self) -> &'static str {
        match self {
            SyntaxKind::EOF => "EOF",
            SyntaxKind::WHITESPACE => "WHITESPACE",
            SyntaxKind::IDENT => "IDENT",
            SyntaxKind::STRING => "STRING",
            SyntaxKind::INTEGER => "INTEGER",
            SyntaxKind::FLOAT => "FLOAT",
            SyntaxKind::BOOLEAN => "BOOLEAN",
            SyntaxKind::NULL => "NULL",
            SyntaxKind::STAR => "STAR",
            SyntaxKind::KEYWORD => "KEYWORD",
            SyntaxKind::PUNCT => "PUNCT",
            SyntaxKind::ERROR_TOKEN => "ERROR_TOKEN",
            SyntaxKind::STATEMENT => "STATEMENT",
            SyntaxKind::CLAUSE => "CLAUSE",
            SyntaxKind::MATCH_CLAUSE => "MATCH_CLAUSE",
            SyntaxKind::WHERE_CLAUSE => "WHERE_CLAUSE",
            SyntaxKind::RETURN_CLAUSE => "RETURN_CLAUSE",
            SyntaxKind::CREATE_CLAUSE => "CREATE_CLAUSE",
            SyntaxKind::PATTERN => "PATTERN",
            SyntaxKind::NODE_PATTERN => "NODE_PATTERN",
            SyntaxKind::REL_PATTERN => "REL_PATTERN",
            SyntaxKind::PROPERTY_MAP => "PROPERTY_MAP",
            SyntaxKind::PROPERTY_ENTRY => "PROPERTY_ENTRY",
            SyntaxKind::PROJECTION => "PROJECTION",
            SyntaxKind::ORDER_ITEM => "ORDER_ITEM",
            SyntaxKind::EXPRESSION => "EXPRESSION",
            SyntaxKind::BINARY_EXPR => "BINARY_EXPR",
            SyntaxKind::UNARY_EXPR => "UNARY_EXPR",
            SyntaxKind::COMPARISON_EXPR => "COMPARISON_EXPR",
            SyntaxKind::FUNCTION_CALL => "FUNCTION_CALL",
            SyntaxKind::ARG_LIST => "ARG_LIST",
            SyntaxKind::LIST_LITERAL => "LIST_LITERAL",
            SyntaxKind::MAP_LITERAL => "MAP_LITERAL",
            SyntaxKind::PATH_LENGTH => "PATH_LENGTH",
            SyntaxKind::IS_NULL_EXPR => "IS_NULL_EXPR",
            SyntaxKind::PROPERTY_ACCESS => "PROPERTY_ACCESS",
            SyntaxKind::ERROR => "ERROR",
        }
    }
}

impl fmt::Display for SyntaxKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

// ------------------------------------------------------------------
// CypherLanguage — rowan::Language glue
// ------------------------------------------------------------------

/// The [`rowan::Language`] implementation for openCypher.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CypherLanguage {}

impl Language for CypherLanguage {
    type Kind = SyntaxKind;

    fn kind_from_raw(raw: rowan::SyntaxKind) -> SyntaxKind {
        assert!(raw.0 <= SyntaxKind::ERROR as u16);
        // SAFETY: SyntaxKind is #[repr(u16)] and the assert guarantees the
        // value is within the valid range.
        unsafe { std::mem::transmute::<u16, SyntaxKind>(raw.0) }
    }

    fn kind_to_raw(kind: SyntaxKind) -> rowan::SyntaxKind {
        rowan::SyntaxKind(kind as u16)
    }
}

// ------------------------------------------------------------------
// Type aliases for convenience
// ------------------------------------------------------------------

/// A syntax node in the Cypher CST.
pub type CstNode = SyntaxNode<CypherLanguage>;
/// A syntax token in the Cypher CST.
pub type CstToken = SyntaxToken<CypherLanguage>;
/// A text range (offset-based span) inside the source query.
pub type Span = TextRange;

// ------------------------------------------------------------------
// Typed wrappers — ergonomic access to CST nodes
// ------------------------------------------------------------------

macro_rules! typed_wrapper {
    ($name:ident, $kind:expr) => {
        /// Typed wrapper around a [`CstNode`] of kind [`$kind`].
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub struct $name(CstNode);

        impl $name {
            /// Wrap a raw node, panicking if the kind does not match.
            pub fn new(node: CstNode) -> Self {
                assert_eq!(
                    node.kind(),
                    $kind,
                    "expected {}, got {}",
                    $kind,
                    node.kind()
                );
                Self(node)
            }

            /// Try to wrap a raw node. Returns `None` if the kind does not match.
            pub fn cast(node: CstNode) -> Option<Self> {
                if node.kind() == $kind {
                    Some(Self(node))
                } else {
                    None
                }
            }

            /// The underlying raw node.
            pub fn syntax(&self) -> &CstNode {
                &self.0
            }

            /// The source span of this node.
            pub fn span(&self) -> Span {
                self.0.text_range()
            }
        }

        impl std::ops::Deref for $name {
            type Target = CstNode;
            fn deref(&self) -> &Self::Target {
                &self.0
            }
        }
    };
}

typed_wrapper!(StatementNode, SyntaxKind::STATEMENT);
typed_wrapper!(ClauseNode, SyntaxKind::CLAUSE);
typed_wrapper!(MatchClauseNode, SyntaxKind::MATCH_CLAUSE);
typed_wrapper!(WhereClauseNode, SyntaxKind::WHERE_CLAUSE);
typed_wrapper!(ReturnClauseNode, SyntaxKind::RETURN_CLAUSE);
typed_wrapper!(CreateClauseNode, SyntaxKind::CREATE_CLAUSE);
typed_wrapper!(PatternNode, SyntaxKind::PATTERN);
typed_wrapper!(NodePatternNode, SyntaxKind::NODE_PATTERN);
typed_wrapper!(RelPatternNode, SyntaxKind::REL_PATTERN);
typed_wrapper!(ProjectionNode, SyntaxKind::PROJECTION);
typed_wrapper!(ExpressionNode, SyntaxKind::EXPRESSION);
typed_wrapper!(BinaryExprNode, SyntaxKind::BINARY_EXPR);
typed_wrapper!(UnaryExprNode, SyntaxKind::UNARY_EXPR);
typed_wrapper!(ComparisonExprNode, SyntaxKind::COMPARISON_EXPR);
typed_wrapper!(FunctionCallNode, SyntaxKind::FUNCTION_CALL);
typed_wrapper!(ListLiteralNode, SyntaxKind::LIST_LITERAL);
typed_wrapper!(MapLiteralNode, SyntaxKind::MAP_LITERAL);
typed_wrapper!(PropertyMapNode, SyntaxKind::PROPERTY_MAP);
typed_wrapper!(PropertyAccessNode, SyntaxKind::PROPERTY_ACCESS);
typed_wrapper!(ErrorNode, SyntaxKind::ERROR);

// ------------------------------------------------------------------
// Builder helpers — used by the parser to construct Green Trees
// ------------------------------------------------------------------

use rowan::GreenNodeBuilder;

/// A helper that wraps [`GreenNodeBuilder`] with Cypher-specific conveniences.
pub struct CstBuilder<'a> {
    inner: GreenNodeBuilder<'a>,
}

impl<'a> CstBuilder<'a> {
    /// Create a new builder.
    pub fn new() -> Self {
        Self {
            inner: GreenNodeBuilder::new(),
        }
    }

    /// Start a new node of the given kind.
    pub fn start_node(&mut self, kind: SyntaxKind) {
        self.inner.start_node(CypherLanguage::kind_to_raw(kind));
    }

    /// Finish the current node.
    pub fn finish_node(&mut self) {
        self.inner.finish_node();
    }

    /// Add a token to the current node.
    pub fn token(&mut self, kind: SyntaxKind, text: &str) {
        self.inner.token(CypherLanguage::kind_to_raw(kind), text);
    }

    /// Build the final tree and return the root node.
    pub fn finish(self) -> CstNode {
        CstNode::new_root(self.inner.finish())
    }
}

impl Default for CstBuilder<'_> {
    fn default() -> Self {
        Self::new()
    }
}

// ------------------------------------------------------------------
// Error-recovery helpers
// ------------------------------------------------------------------

/// Create a standalone error node that wraps raw text tokens.
///
/// This is used by the parser when it encounters an unexpected token and
/// needs to skip ahead to a known synchronisation point.  The resulting
/// tree is still navigable: callers can walk children to inspect the
/// unconsumed tokens.
pub fn error_node(text: &str, range: Span) -> ErrorNode {
    let mut b = CstBuilder::new();
    b.start_node(SyntaxKind::ERROR);
    b.token(SyntaxKind::ERROR_TOKEN, text);
    b.finish_node();
    let node = b.finish();
    // rowan does not expose a way to override the text range on construction,
    // so we rely on the builder order.  For accurate spans the parser must
    // emit tokens in source order.
    let _ = range; // range is validated by the caller via token order
    ErrorNode::cast(node).expect("error node was just created")
}

// ------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syntax_kind_is_token_vs_node() {
        assert!(SyntaxKind::IDENT.is_token());
        assert!(!SyntaxKind::IDENT.is_node());
        assert!(SyntaxKind::STATEMENT.is_node());
        assert!(!SyntaxKind::STATEMENT.is_token());
    }

    #[test]
    fn cypher_language_roundtrip() {
        let kind = SyntaxKind::MATCH_CLAUSE;
        let raw = CypherLanguage::kind_to_raw(kind);
        let back = CypherLanguage::kind_from_raw(raw);
        assert_eq!(kind, back);
    }

    #[test]
    fn typed_wrapper_cast_ok() {
        let mut b = CstBuilder::new();
        b.start_node(SyntaxKind::STATEMENT);
        b.token(SyntaxKind::KEYWORD, "MATCH");
        b.finish_node();
        let root = b.finish();
        let stmt = StatementNode::cast(root).unwrap();
        assert_eq!(u32::from(stmt.span().start()), 0u32);
    }

    #[test]
    fn typed_wrapper_cast_fails_for_wrong_kind() {
        let mut b = CstBuilder::new();
        b.start_node(SyntaxKind::CLAUSE);
        b.finish_node();
        let root = b.finish();
        assert!(StatementNode::cast(root).is_none());
    }

    #[test]
    fn error_node_is_navigable() {
        let err = error_node("@", TextRange::new(0.into(), 1.into()));
        assert_eq!(err.syntax().kind(), SyntaxKind::ERROR);
        let children: Vec<_> = err.syntax().children_with_tokens().collect();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].kind(), SyntaxKind::ERROR_TOKEN);
    }

    #[test]
    fn partial_tree_with_error_still_traversable() {
        // Simulate a tree where an error node sits inside a statement.
        let mut b = CstBuilder::new();
        b.start_node(SyntaxKind::STATEMENT);
        b.start_node(SyntaxKind::MATCH_CLAUSE);
        b.token(SyntaxKind::KEYWORD, "MATCH");
        b.start_node(SyntaxKind::PATTERN);
        b.start_node(SyntaxKind::NODE_PATTERN);
        b.token(SyntaxKind::PUNCT, "(");
        b.token(SyntaxKind::IDENT, "n");
        b.token(SyntaxKind::PUNCT, ")");
        b.finish_node(); // NODE_PATTERN
        b.finish_node(); // PATTERN
        b.finish_node(); // MATCH_CLAUSE
        b.start_node(SyntaxKind::ERROR);
        b.token(SyntaxKind::ERROR_TOKEN, "@");
        b.finish_node(); // ERROR
        b.finish_node(); // STATEMENT
        let root = b.finish();

        // Walk the tree and count nodes.
        let mut count = 0;
        for child in root.children() {
            count += 1;
            for _ in child.children_with_tokens() {
                count += 1;
            }
        }
        assert!(count >= 3, "tree should be traversable despite error node");
    }

    #[test]
    fn display_kind() {
        assert_eq!(format!("{}", SyntaxKind::MATCH_CLAUSE), "MATCH_CLAUSE");
        assert_eq!(format!("{}", SyntaxKind::IDENT), "IDENT");
    }
}
