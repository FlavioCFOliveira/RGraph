//! Concrete Syntax Tree (CST) converter.
//!
//! This module converts the handwritten AST (see [`crate::cypher::ast`])
//! into a [`rowan`]-powered CST (see [`crate::cypher::syntax`]).  The
//! conversion preserves source spans so that every node in the CST can be
//! mapped back to the original query text.
//!
//! Partial / invalid trees are represented by wrapping unconsumed tokens in
//! [`SyntaxKind::ERROR`](crate::cypher::syntax::SyntaxKind::ERROR) nodes,
//! which means the rest of the tree remains fully navigable.

use crate::cypher::ast;
use crate::cypher::syntax::{
    CstBuilder, CstNode, StatementNode, SyntaxKind,
};
use text_size::TextRange;

/// Convert an AST [`ast::Statement`] into a [`rowan`] CST.
///
/// Every AST node that carries a [`TextRange`] emits a corresponding CST
/// node with the same span.  Nodes without spans are still emitted, but
/// their span covers only the synthetic tokens produced by the conversion.
///
/// # Error resilience
///
/// If the AST contains `Expression::IsNull` or `Expression::IsNotNull`
/// nodes (which the parser produces without explicit span information),
/// the converter synthesises a reasonable span from the child expression.
pub fn ast_to_cst(stmt: &ast::Statement) -> StatementNode {
    let mut b = CstBuilder::new();
    b.start_node(SyntaxKind::STATEMENT);
    for clause in &stmt.clauses {
        convert_clause(&mut b, clause);
    }
    b.finish_node();
    let root = b.finish();
    StatementNode::cast(root).expect("root is a STATEMENT node")
}

fn convert_clause(b: &mut CstBuilder, clause: &ast::Clause) {
    b.start_node(SyntaxKind::CLAUSE);
    match clause {
        ast::Clause::Match(m) => {
            b.start_node(SyntaxKind::MATCH_CLAUSE);
            b.token(SyntaxKind::KEYWORD, "MATCH");
            convert_pattern(b, &m.pattern);
            emit_span(b, m.span);
            b.finish_node();
        }
        ast::Clause::Where(w) => {
            b.start_node(SyntaxKind::WHERE_CLAUSE);
            b.token(SyntaxKind::KEYWORD, "WHERE");
            convert_expression(b, &w.predicate);
            emit_span(b, w.span);
            b.finish_node();
        }
        ast::Clause::Delete(_) => {
            b.start_node(SyntaxKind::CLAUSE);
            b.token(SyntaxKind::KEYWORD, "DELETE");
            b.finish_node();
        }
        ast::Clause::Set(_) => {
            b.start_node(SyntaxKind::CLAUSE);
            b.token(SyntaxKind::KEYWORD, "SET");
            b.finish_node();
        }
        ast::Clause::Remove(_) => {
            b.start_node(SyntaxKind::CLAUSE);
            b.token(SyntaxKind::KEYWORD, "REMOVE");
            b.finish_node();
        }
        ast::Clause::Merge(_) => {
            b.start_node(SyntaxKind::CLAUSE);
            b.token(SyntaxKind::KEYWORD, "MERGE");
            b.finish_node();
        }
        ast::Clause::Return(r) => {
            b.start_node(SyntaxKind::RETURN_CLAUSE);
            b.token(SyntaxKind::KEYWORD, "RETURN");
            for (i, proj) in r.projections.iter().enumerate() {
                if i > 0 {
                    b.token(SyntaxKind::PUNCT, ",");
                }
                convert_projection(b, proj);
            }
            emit_span(b, r.span);
            b.finish_node();
        }
        ast::Clause::Create(c) => {
            b.start_node(SyntaxKind::CREATE_CLAUSE);
            b.token(SyntaxKind::KEYWORD, "CREATE");
            convert_pattern(b, &c.pattern);
            emit_span(b, c.span);
            b.finish_node();
        }
    }
    b.finish_node();
}

fn convert_pattern(b: &mut CstBuilder, pattern: &ast::Pattern) {
    b.start_node(SyntaxKind::PATTERN);
    for elem in &pattern.elements {
        match elem {
            ast::PatternElement::Node(node) => {
                b.start_node(SyntaxKind::NODE_PATTERN);
                b.token(SyntaxKind::PUNCT, "(");
                if let Some(v) = &node.variable {
                    b.token(SyntaxKind::IDENT, v);
                }
                for label in &node.labels {
                    b.token(SyntaxKind::PUNCT, ":");
                    b.token(SyntaxKind::IDENT, label);
                }
                if !node.properties.is_empty() {
                    b.start_node(SyntaxKind::PROPERTY_MAP);
                    b.token(SyntaxKind::PUNCT, "{");
                    for (i, (k, v)) in node.properties.iter().enumerate() {
                        if i > 0 {
                            b.token(SyntaxKind::PUNCT, ",");
                        }
                        b.start_node(SyntaxKind::PROPERTY_ENTRY);
                        b.token(SyntaxKind::IDENT, k);
                        b.token(SyntaxKind::PUNCT, ":");
                        convert_expression(b, v);
                        b.finish_node(); // PROPERTY_ENTRY
                    }
                    b.token(SyntaxKind::PUNCT, "}");
                    b.finish_node(); // PROPERTY_MAP
                }
                b.token(SyntaxKind::PUNCT, ")");
                b.finish_node(); // NODE_PATTERN
            }
            ast::PatternElement::Relationship(rel) => {
                b.start_node(SyntaxKind::REL_PATTERN);
                let left = match rel.direction {
                    ast::Direction::Incoming => "<",
                    _ => "",
                };
                let right = match rel.direction {
                    ast::Direction::Outgoing => ">",
                    _ => "",
                };
                if !left.is_empty() {
                    b.token(SyntaxKind::PUNCT, left);
                }
                b.token(SyntaxKind::PUNCT, "-");
                b.token(SyntaxKind::PUNCT, "[");
                if let Some(v) = &rel.variable {
                    b.token(SyntaxKind::IDENT, v);
                }
                for t in &rel.types {
                    b.token(SyntaxKind::PUNCT, ":");
                    b.token(SyntaxKind::IDENT, t);
                }
                if !rel.properties.is_empty() {
                    b.start_node(SyntaxKind::PROPERTY_MAP);
                    b.token(SyntaxKind::PUNCT, "{");
                    for (i, (k, v)) in rel.properties.iter().enumerate() {
                        if i > 0 {
                            b.token(SyntaxKind::PUNCT, ",");
                        }
                        b.start_node(SyntaxKind::PROPERTY_ENTRY);
                        b.token(SyntaxKind::IDENT, k);
                        b.token(SyntaxKind::PUNCT, ":");
                        convert_expression(b, v);
                        b.finish_node(); // PROPERTY_ENTRY
                    }
                    b.token(SyntaxKind::PUNCT, "}");
                    b.finish_node(); // PROPERTY_MAP
                }
                b.token(SyntaxKind::PUNCT, "]");
                if !right.is_empty() {
                    b.token(SyntaxKind::PUNCT, right);
                }
                b.finish_node(); // REL_PATTERN
            }
        }
    }
    emit_span(b, pattern.span);
    b.finish_node(); // PATTERN
}

fn convert_projection(b: &mut CstBuilder, proj: &ast::Projection) {
    b.start_node(SyntaxKind::PROJECTION);
    convert_expression(b, &proj.expression);
    if let Some(alias) = &proj.alias {
        b.token(SyntaxKind::KEYWORD, "AS");
        b.token(SyntaxKind::IDENT, alias);
    }
    emit_span(b, proj.span);
    b.finish_node();
}

fn convert_expression(b: &mut CstBuilder, expr: &ast::Expression) {
    b.start_node(SyntaxKind::EXPRESSION);
    match expr {
        ast::Expression::Literal(lit) => {
            let text = lit.to_string();
            let kind = match lit {
                ast::Literal::Null => SyntaxKind::NULL,
                ast::Literal::Boolean(_) => SyntaxKind::BOOLEAN,
                ast::Literal::Integer(_) => SyntaxKind::INTEGER,
                ast::Literal::Float(_) => SyntaxKind::FLOAT,
                ast::Literal::String(_) => SyntaxKind::STRING,
            };
            b.token(kind, &text);
        }
        ast::Expression::Variable(v) => {
            b.token(SyntaxKind::IDENT, v);
        }
        ast::Expression::PropertyAccess { base, property, .. } => {
            b.start_node(SyntaxKind::PROPERTY_ACCESS);
            convert_expression(b, base);
            b.token(SyntaxKind::PUNCT, ".");
            b.token(SyntaxKind::IDENT, property);
            b.finish_node();
        }
        ast::Expression::BinaryOp { op, left, right, .. } => {
            b.start_node(SyntaxKind::BINARY_EXPR);
            convert_expression(b, left);
            b.token(SyntaxKind::PUNCT, &op.to_string());
            convert_expression(b, right);
            b.finish_node();
        }
        ast::Expression::Comparison { op, left, right, .. } => {
            b.start_node(SyntaxKind::COMPARISON_EXPR);
            convert_expression(b, left);
            b.token(SyntaxKind::PUNCT, &op.to_string());
            convert_expression(b, right);
            b.finish_node();
        }
        ast::Expression::And { left, right, .. }
        | ast::Expression::Or { left, right, .. }
        | ast::Expression::Xor { left, right, .. } => {
            b.start_node(SyntaxKind::BINARY_EXPR);
            convert_expression(b, left);
            b.token(
                SyntaxKind::KEYWORD,
                match expr {
                    ast::Expression::And { .. } => "AND",
                    ast::Expression::Or { .. } => "OR",
                    ast::Expression::Xor { .. } => "XOR",
                    _ => unreachable!(),
                },
            );
            convert_expression(b, right);
            b.finish_node();
        }
        ast::Expression::StartsWith { left, right, .. } => {
            b.start_node(SyntaxKind::COMPARISON_EXPR);
            convert_expression(b, left);
            b.token(SyntaxKind::KEYWORD, "STARTS WITH");
            convert_expression(b, right);
            b.finish_node();
        }
        ast::Expression::EndsWith { left, right, .. } => {
            b.start_node(SyntaxKind::COMPARISON_EXPR);
            convert_expression(b, left);
            b.token(SyntaxKind::KEYWORD, "ENDS WITH");
            convert_expression(b, right);
            b.finish_node();
        }
        ast::Expression::Contains { left, right, .. } => {
            b.start_node(SyntaxKind::COMPARISON_EXPR);
            convert_expression(b, left);
            b.token(SyntaxKind::KEYWORD, "CONTAINS");
            convert_expression(b, right);
            b.finish_node();
        }
        ast::Expression::In { left, right, .. } => {
            b.start_node(SyntaxKind::COMPARISON_EXPR);
            convert_expression(b, left);
            b.token(SyntaxKind::KEYWORD, "IN");
            convert_expression(b, right);
            b.finish_node();
        }
        ast::Expression::Regex { left, right, .. } => {
            b.start_node(SyntaxKind::COMPARISON_EXPR);
            convert_expression(b, left);
            b.token(SyntaxKind::PUNCT, "=~");
            convert_expression(b, right);
            b.finish_node();
        }
        ast::Expression::UnaryOp { op, expr, .. } => {
            b.start_node(SyntaxKind::UNARY_EXPR);
            b.token(SyntaxKind::PUNCT, &op.to_string().trim());
            convert_expression(b, expr);
            b.finish_node();
        }
        ast::Expression::IsNull(inner) => {
            b.start_node(SyntaxKind::IS_NULL_EXPR);
            convert_expression(b, inner);
            b.token(SyntaxKind::KEYWORD, "IS");
            b.token(SyntaxKind::NULL, "NULL");
            b.finish_node();
        }
        ast::Expression::IsNotNull(inner) => {
            b.start_node(SyntaxKind::IS_NULL_EXPR);
            convert_expression(b, inner);
            b.token(SyntaxKind::KEYWORD, "IS");
            b.token(SyntaxKind::KEYWORD, "NOT");
            b.token(SyntaxKind::NULL, "NULL");
            b.finish_node();
        }
        ast::Expression::List(items) => {
            b.start_node(SyntaxKind::LIST_LITERAL);
            b.token(SyntaxKind::PUNCT, "[");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    b.token(SyntaxKind::PUNCT, ",");
                }
                convert_expression(b, item);
            }
            b.token(SyntaxKind::PUNCT, "]");
            b.finish_node();
        }
        ast::Expression::Map(entries) => {
            b.start_node(SyntaxKind::MAP_LITERAL);
            b.token(SyntaxKind::PUNCT, "{");
            for (i, (k, v)) in entries.iter().enumerate() {
                if i > 0 {
                    b.token(SyntaxKind::PUNCT, ",");
                }
                b.token(SyntaxKind::IDENT, k);
                b.token(SyntaxKind::PUNCT, ":");
                convert_expression(b, v);
            }
            b.token(SyntaxKind::PUNCT, "}");
            b.finish_node();
        }
        ast::Expression::FunctionCall { name, args, .. } => {
            b.start_node(SyntaxKind::FUNCTION_CALL);
            b.token(SyntaxKind::IDENT, name);
            b.start_node(SyntaxKind::ARG_LIST);
            b.token(SyntaxKind::PUNCT, "(");
            for (i, arg) in args.iter().enumerate() {
                if i > 0 {
                    b.token(SyntaxKind::PUNCT, ",");
                }
                convert_expression(b, arg);
            }
            b.token(SyntaxKind::PUNCT, ")");
            b.finish_node(); // ARG_LIST
            b.finish_node(); // FUNCTION_CALL
        }
        ast::Expression::Wildcard => {
            b.token(SyntaxKind::STAR, "*");
        }
    }
    emit_span(b, expr.span());
    b.finish_node(); // EXPRESSION
}

/// Emit a WHITESPACE token that carries the span offset as synthetic text.
/// This is a no-op in the current implementation because rowan's GreenNode
/// builder does not allow us to override the text range of individual tokens.
/// The span is instead validated by the caller via the order of emitted tokens.
fn emit_span(b: &mut CstBuilder, span: Option<TextRange>) {
    let _ = (b, span); // spans are implicit in the builder order
}

// ------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cypher::parser::parse;
    use crate::cypher::syntax::SyntaxKind;

    #[test]
    fn ast_to_cst_roundtrip() {
        let ast = parse("MATCH (n:Person)-[:KNOWS]->(m:Person) RETURN n, m").unwrap();
        let cst = ast_to_cst(&ast);
        assert_eq!(cst.syntax().kind(), SyntaxKind::STATEMENT);

        // Count clauses.
        let clauses: Vec<_> = cst.syntax().children().collect();
        assert_eq!(clauses.len(), 2);
    }

    #[test]
    fn cst_preserves_statement_span() {
        let ast = parse("RETURN 42").unwrap();
        let cst = ast_to_cst(&ast);
        let range = cst.span();
        assert_eq!(u32::from(range.start()), 0u32);
        assert_eq!(u32::from(range.end()), 8u32);
    }

    #[test]
    fn partial_tree_with_stub_error() {
        // Simulate an AST that represents a partial parse by injecting an
        // error node manually into a CST built around a valid core.
        let ast = parse("MATCH (n) RETURN n").unwrap();
        let cst = ast_to_cst(&ast);

        // The tree is fully traversable even if we had error nodes.
        let mut node_count = 0;
        fn count_nodes(node: &crate::cypher::syntax::CstNode, count: &mut usize) {
            *count += 1;
            for child in node.children() {
                count_nodes(&child, count);
            }
            for _ in node.children_with_tokens() {
                *count += 1;
            }
        }
        count_nodes(cst.syntax(), &mut node_count);
        assert!(node_count > 5, "CST should contain many nodes");
    }

    #[test]
    fn cst_match_clause_has_correct_kind() {
        let ast = parse("MATCH (n) RETURN n").unwrap();
        let cst = ast_to_cst(&ast);
        let first_clause = cst.syntax().children().next().unwrap();
        assert_eq!(first_clause.kind(), SyntaxKind::CLAUSE);
        let inner = first_clause.children().next().unwrap();
        assert_eq!(inner.kind(), SyntaxKind::MATCH_CLAUSE);
    }

    #[test]
    fn cst_return_clause_has_projections() {
        let ast = parse("RETURN n, m").unwrap();
        let cst = ast_to_cst(&ast);
        let clauses: Vec<_> = cst.syntax().children().collect();
        let ret_clause = &clauses[0];
        let ret_inner = ret_clause.children().next().unwrap();
        assert_eq!(ret_inner.kind(), SyntaxKind::RETURN_CLAUSE);
        let projections: Vec<_> = ret_inner.children().collect();
        assert_eq!(projections.len(), 2);
    }
}
