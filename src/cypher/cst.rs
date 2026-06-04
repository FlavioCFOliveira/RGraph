//! Concrete Syntax Tree (CST) converter.
//!
//! This module converts the handwritten AST (see [`crate::cypher::ast`])
//! into a [`rowan`]-powered CST (see [`crate::cypher::syntax`]).  The
//! conversion preserves source spans so that every node in the CST can be
//! mapped back to the original query text.

use crate::cypher::ast;
use crate::cypher::syntax::{
    CstBuilder, StatementNode, SyntaxKind,
};
use text_size::TextRange;

/// Convert an AST [`ast::Statement`] into a [`rowan`] CST.
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
        ast::Clause::Match(m) | ast::Clause::OptionalMatch(m) => {
            b.start_node(SyntaxKind::MATCH_CLAUSE);
            b.token(SyntaxKind::KEYWORD, "MATCH");
            for np in &m.patterns {
                convert_pattern(b, &np.pattern);
            }
            b.finish_node();
        }
        ast::Clause::Where(w) => {
            b.start_node(SyntaxKind::WHERE_CLAUSE);
            b.token(SyntaxKind::KEYWORD, "WHERE");
            convert_expression(b, &w.predicate);
            b.finish_node();
        }
        ast::Clause::Delete(d) => {
            b.start_node(SyntaxKind::CLAUSE);
            b.token(SyntaxKind::KEYWORD, if d.detach { "DETACH DELETE" } else { "DELETE" });
            for expr in &d.expressions {
                convert_expression(b, expr);
            }
            b.finish_node();
        }
        ast::Clause::Set(s) => {
            b.start_node(SyntaxKind::CLAUSE);
            b.token(SyntaxKind::KEYWORD, "SET");
            for item in &s.items {
                convert_set_item(b, item);
            }
            b.finish_node();
        }
        ast::Clause::Remove(r) => {
            b.start_node(SyntaxKind::CLAUSE);
            b.token(SyntaxKind::KEYWORD, "REMOVE");
            for item in &r.items {
                convert_remove_item(b, item);
            }
            b.finish_node();
        }
        ast::Clause::Merge(m) => {
            b.start_node(SyntaxKind::CLAUSE);
            b.token(SyntaxKind::KEYWORD, "MERGE");
            convert_pattern(b, &m.pattern);
            b.finish_node();
        }
        ast::Clause::Return(r) => {
            b.start_node(SyntaxKind::RETURN_CLAUSE);
            b.token(SyntaxKind::KEYWORD, "RETURN");
            if r.distinct { b.token(SyntaxKind::KEYWORD, "DISTINCT"); }
            if r.star {
                b.token(SyntaxKind::STAR, "*");
            } else {
                for (i, proj) in r.projections.iter().enumerate() {
                    if i > 0 { b.token(SyntaxKind::PUNCT, ","); }
                    convert_projection(b, proj);
                }
            }
            b.finish_node();
        }
        ast::Clause::Create(c) => {
            b.start_node(SyntaxKind::CREATE_CLAUSE);
            b.token(SyntaxKind::KEYWORD, "CREATE");
            for np in &c.patterns {
                convert_pattern(b, &np.pattern);
            }
            b.finish_node();
        }
        ast::Clause::With(w) => {
            b.start_node(SyntaxKind::CLAUSE);
            b.token(SyntaxKind::KEYWORD, "WITH");
            if w.distinct { b.token(SyntaxKind::KEYWORD, "DISTINCT"); }
            if w.star {
                b.token(SyntaxKind::STAR, "*");
            } else {
                for (i, proj) in w.projections.iter().enumerate() {
                    if i > 0 { b.token(SyntaxKind::PUNCT, ","); }
                    convert_projection(b, proj);
                }
            }
            if let Some(pred) = &w.where_ {
                b.token(SyntaxKind::KEYWORD, "WHERE");
                convert_expression(b, pred);
            }
            b.finish_node();
        }
        ast::Clause::Unwind(u) => {
            b.start_node(SyntaxKind::CLAUSE);
            b.token(SyntaxKind::KEYWORD, "UNWIND");
            convert_expression(b, &u.expression);
            b.token(SyntaxKind::KEYWORD, "AS");
            b.token(SyntaxKind::IDENT, &u.variable);
            b.finish_node();
        }
        ast::Clause::Union(u) => {
            b.start_node(SyntaxKind::CLAUSE);
            b.token(SyntaxKind::KEYWORD, if u.all { "UNION ALL" } else { "UNION" });
            b.finish_node();
        }
        ast::Clause::Call(c) => {
            b.start_node(SyntaxKind::CLAUSE);
            b.token(SyntaxKind::KEYWORD, "CALL");
            if let Some(proc) = &c.procedure {
                b.token(SyntaxKind::IDENT, proc);
            }
            b.finish_node();
        }
        ast::Clause::Foreach(fe) => {
            b.start_node(SyntaxKind::CLAUSE);
            b.token(SyntaxKind::KEYWORD, "FOREACH");
            b.token(SyntaxKind::IDENT, &fe.variable);
            b.finish_node();
        }
    }
    b.finish_node();
}

fn convert_set_item(b: &mut CstBuilder, item: &ast::SetItem) {
    b.start_node(SyntaxKind::CLAUSE);
    match item {
        ast::SetItem::Property { target, value } => {
            convert_expression(b, target);
            b.token(SyntaxKind::PUNCT, "=");
            convert_expression(b, value);
        }
        ast::SetItem::Label { variable, labels } => {
            b.token(SyntaxKind::IDENT, variable);
            for label in labels {
                b.token(SyntaxKind::PUNCT, ":");
                b.token(SyntaxKind::IDENT, label);
            }
        }
        ast::SetItem::Merge { variable, value } => {
            b.token(SyntaxKind::IDENT, variable);
            b.token(SyntaxKind::PUNCT, "+=");
            convert_expression(b, value);
        }
        ast::SetItem::Replace { variable, value } => {
            b.token(SyntaxKind::IDENT, variable);
            b.token(SyntaxKind::PUNCT, "=");
            convert_expression(b, value);
        }
    }
    b.finish_node();
}

fn convert_remove_item(b: &mut CstBuilder, item: &ast::RemoveItem) {
    b.start_node(SyntaxKind::CLAUSE);
    match item {
        ast::RemoveItem::Property { target } => convert_expression(b, target),
        ast::RemoveItem::Label { variable, labels } => {
            b.token(SyntaxKind::IDENT, variable);
            for label in labels {
                b.token(SyntaxKind::PUNCT, ":");
                b.token(SyntaxKind::IDENT, label);
            }
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
                if let Some(v) = &node.variable { b.token(SyntaxKind::IDENT, v); }
                for label in &node.labels {
                    b.token(SyntaxKind::PUNCT, ":");
                    b.token(SyntaxKind::IDENT, label);
                }
                if !node.properties.is_empty() {
                    b.start_node(SyntaxKind::PROPERTY_MAP);
                    b.token(SyntaxKind::PUNCT, "{");
                    for (i, (k, v)) in node.properties.iter().enumerate() {
                        if i > 0 { b.token(SyntaxKind::PUNCT, ","); }
                        b.start_node(SyntaxKind::PROPERTY_ENTRY);
                        b.token(SyntaxKind::IDENT, k);
                        b.token(SyntaxKind::PUNCT, ":");
                        convert_expression(b, v);
                        b.finish_node();
                    }
                    b.token(SyntaxKind::PUNCT, "}");
                    b.finish_node();
                }
                b.token(SyntaxKind::PUNCT, ")");
                b.finish_node();
            }
            ast::PatternElement::Relationship(rel) => {
                b.start_node(SyntaxKind::REL_PATTERN);
                let left = match rel.direction { ast::Direction::Incoming => "<", _ => "" };
                let right = match rel.direction { ast::Direction::Outgoing => ">", _ => "" };
                if !left.is_empty() { b.token(SyntaxKind::PUNCT, left); }
                b.token(SyntaxKind::PUNCT, "-");
                b.token(SyntaxKind::PUNCT, "[");
                if let Some(v) = &rel.variable { b.token(SyntaxKind::IDENT, v); }
                for t in &rel.types {
                    b.token(SyntaxKind::PUNCT, ":");
                    b.token(SyntaxKind::IDENT, t);
                }
                b.token(SyntaxKind::PUNCT, "]");
                b.token(SyntaxKind::PUNCT, "-");
                if !right.is_empty() { b.token(SyntaxKind::PUNCT, right); }
                b.finish_node();
            }
        }
    }
    b.finish_node();
}

fn convert_projection(b: &mut CstBuilder, proj: &ast::Projection) {
    b.start_node(SyntaxKind::PROJECTION);
    convert_expression(b, &proj.expression);
    if let Some(alias) = &proj.alias {
        b.token(SyntaxKind::KEYWORD, "AS");
        b.token(SyntaxKind::IDENT, alias);
    }
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
                _ => SyntaxKind::STRING, // temporal literals as strings
            };
            b.token(kind, &text);
        }
        ast::Expression::Variable(v) => { b.token(SyntaxKind::IDENT, v); }
        ast::Expression::Parameter(p) => {
            b.token(SyntaxKind::PUNCT, "$");
            b.token(SyntaxKind::IDENT, p);
        }
        ast::Expression::PropertyAccess { base, property, .. } => {
            b.start_node(SyntaxKind::PROPERTY_ACCESS);
            convert_expression(b, base);
            b.token(SyntaxKind::PUNCT, ".");
            b.token(SyntaxKind::IDENT, property);
            b.finish_node();
        }
        ast::Expression::DynamicPropertyAccess { base, index, .. } => {
            b.start_node(SyntaxKind::PROPERTY_ACCESS);
            convert_expression(b, base);
            b.token(SyntaxKind::PUNCT, "[");
            convert_expression(b, index);
            b.token(SyntaxKind::PUNCT, "]");
            b.finish_node();
        }
        ast::Expression::Slice { base, from, to, .. } => {
            b.start_node(SyntaxKind::EXPRESSION);
            convert_expression(b, base);
            b.token(SyntaxKind::PUNCT, "[");
            if let Some(f) = from { convert_expression(b, f); }
            b.token(SyntaxKind::PUNCT, "..");
            if let Some(t) = to { convert_expression(b, t); }
            b.token(SyntaxKind::PUNCT, "]");
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
            b.token(SyntaxKind::KEYWORD, match expr {
                ast::Expression::And { .. } => "AND",
                ast::Expression::Or { .. }  => "OR",
                ast::Expression::Xor { .. } => "XOR",
                _ => unreachable!(),
            });
            convert_expression(b, right);
            b.finish_node();
        }
        ast::Expression::Not { expr, .. } => {
            b.start_node(SyntaxKind::UNARY_EXPR);
            b.token(SyntaxKind::KEYWORD, "NOT");
            convert_expression(b, expr);
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
            b.token(SyntaxKind::KEYWORD, "IS NULL");
            b.finish_node();
        }
        ast::Expression::IsNotNull(inner) => {
            b.start_node(SyntaxKind::IS_NULL_EXPR);
            convert_expression(b, inner);
            b.token(SyntaxKind::KEYWORD, "IS NOT NULL");
            b.finish_node();
        }
        ast::Expression::List(items) => {
            b.start_node(SyntaxKind::LIST_LITERAL);
            b.token(SyntaxKind::PUNCT, "[");
            for (i, item) in items.iter().enumerate() {
                if i > 0 { b.token(SyntaxKind::PUNCT, ","); }
                convert_expression(b, item);
            }
            b.token(SyntaxKind::PUNCT, "]");
            b.finish_node();
        }
        ast::Expression::Map(entries) => {
            b.start_node(SyntaxKind::MAP_LITERAL);
            b.token(SyntaxKind::PUNCT, "{");
            for (i, (k, v)) in entries.iter().enumerate() {
                if i > 0 { b.token(SyntaxKind::PUNCT, ","); }
                b.token(SyntaxKind::IDENT, k);
                b.token(SyntaxKind::PUNCT, ":");
                convert_expression(b, v);
            }
            b.token(SyntaxKind::PUNCT, "}");
            b.finish_node();
        }
        ast::Expression::FunctionCall { name, args, distinct, .. } => {
            b.start_node(SyntaxKind::FUNCTION_CALL);
            b.token(SyntaxKind::IDENT, name);
            b.start_node(SyntaxKind::ARG_LIST);
            b.token(SyntaxKind::PUNCT, "(");
            if *distinct { b.token(SyntaxKind::KEYWORD, "DISTINCT"); }
            for (i, arg) in args.iter().enumerate() {
                if i > 0 { b.token(SyntaxKind::PUNCT, ","); }
                convert_expression(b, arg);
            }
            b.token(SyntaxKind::PUNCT, ")");
            b.finish_node();
            b.finish_node();
        }
        ast::Expression::Wildcard => { b.token(SyntaxKind::STAR, "*"); }
        ast::Expression::Case { subject, alternatives, default, .. } => {
            b.start_node(SyntaxKind::EXPRESSION);
            b.token(SyntaxKind::KEYWORD, "CASE");
            if let Some(s) = subject { convert_expression(b, s); }
            for alt in alternatives {
                b.token(SyntaxKind::KEYWORD, "WHEN");
                convert_expression(b, &alt.condition);
                b.token(SyntaxKind::KEYWORD, "THEN");
                convert_expression(b, &alt.result);
            }
            if let Some(d) = default {
                b.token(SyntaxKind::KEYWORD, "ELSE");
                convert_expression(b, d);
            }
            b.token(SyntaxKind::KEYWORD, "END");
            b.finish_node();
        }
        // For other complex expressions, emit a synthetic token.
        ast::Expression::ListComprehension { .. }
        | ast::Expression::PatternComprehension { .. }
        | ast::Expression::Reduce { .. }
        | ast::Expression::Quantifier { .. }
        | ast::Expression::Exists { .. } => {
            b.token(SyntaxKind::EXPRESSION, &expr.to_string());
        }
    }
    b.finish_node();
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
        let clauses: Vec<_> = cst.syntax().children().collect();
        assert_eq!(clauses.len(), 2);
    }

    #[test]
    fn cst_preserves_statement_span() {
        let ast = parse("RETURN 42").unwrap();
        let cst = ast_to_cst(&ast);
        let range = cst.span();
        assert_eq!(u32::from(range.start()), 0u32);
        // End should be close to 9 (length of "RETURN 42").
        assert!(u32::from(range.end()) >= 8u32);
    }

    #[test]
    fn partial_tree_with_stub_error() {
        let ast = parse("MATCH (n) RETURN n").unwrap();
        let cst = ast_to_cst(&ast);
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
        assert!(node_count > 5);
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
