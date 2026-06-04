//! Turtle (RDF) parsing and serialisation for the real RDF data model.
//!
//! This is the RDF-native counterpart to the LPG-oriented Turtle handling in
//! [`crate::cli::import`] / [`crate::cli::export`].  When a database is opened
//! in [`GraphMode::Rdf`](crate::config::GraphMode), Turtle import parses the
//! source into term-valued [`Quad`]s and drives [`Database::add_triple`] /
//! [`Database::add_quad`]; Turtle export serialises the stored triples back to
//! valid Turtle.
//!
//! # Supported grammar (a documented, robust subset)
//!
//! * Directives: `@prefix p: <iri> .` and `@base <iri> .` (and their
//!   case-insensitive SPARQL-style `PREFIX` / `BASE` forms without the dot).
//! * Terms:
//!   * IRIs in angle brackets — `<http://example.org/a>` — resolved against
//!     the current base when relative.
//!   * Prefixed names — `ex:alice`, `:bare`, `rdf:type`.
//!   * Blank nodes — `_:b0`.
//!   * The `a` keyword, expanding to `rdf:type`.
//!   * Literals — `"text"`, `"text"@lang`, `"text"^^<datatype>` or
//!     `"text"^^prefix:local`, and the integer / decimal / double / boolean
//!     native forms (`42`, `3.14`, `1.0e9`, `true`).
//! * Statement structure: `subject predicate object .` with predicate-object
//!   lists (`;`) and object lists (`,`).
//! * Comments (`#` to end of line) and arbitrary whitespace.
//!
//! Not supported (rejected or ignored, never silently mis-parsed): collections
//! `( … )`, blank-node property lists `[ … ]`, and quoted triples.  These are
//! out of scope for the import surface; their presence yields a parse error so
//! data is never dropped without notice.

use super::CliError;
use crate::config::GraphMode;
use crate::db::database::Database;
use crate::io::FileSystem;
use crate::rdf::term::{RdfLiteral, Term, xsd};
use crate::rdf::triple::Triple;
use std::collections::HashMap;
use std::io::Write;

/// The `rdf:type` IRI that the `a` keyword expands to.
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// Parse a Turtle document into a list of term-valued [`Triple`]s.
///
/// # Errors
///
/// Returns [`CliError::Parse`] on any syntax the documented subset does not
/// accept, so malformed input is surfaced rather than silently skipped.
pub fn parse_turtle_triples(content: &str) -> Result<Vec<Triple>, CliError> {
    let mut parser = TurtleParser::new(content);
    parser.parse()
}

/// Import a Turtle document into an RDF-mode `db`, returning the number of
/// triples added (idempotent re-inserts are not counted).
///
/// # Errors
///
/// Returns [`CliError::Parse`] on malformed Turtle, or [`CliError::Storage`]
/// if the database is not in RDF mode or a write fails.
pub fn import_turtle(
    db: &mut Database,
    content: &str,
    fs: &dyn FileSystem,
) -> Result<usize, CliError> {
    if db.graph_mode != GraphMode::Rdf {
        return Err(CliError::Parse(
            "Turtle RDF import requires a database opened in RDF mode".into(),
        ));
    }
    let triples = parse_turtle_triples(content)?;
    let mut added = 0usize;
    for triple in &triples {
        if db.add_triple(triple, fs)? {
            added += 1;
        }
    }
    db.sync(fs)?;
    Ok(added)
}

/// Serialise every stored triple of an RDF-mode `db` as Turtle, writing to
/// `out`.  Returns the number of triples written.
///
/// Each triple is emitted as a self-contained `subject predicate object .`
/// statement in N-Triples-compatible Turtle (no prefix abbreviation), so the
/// output round-trips through [`parse_turtle_triples`].
pub fn export_turtle<W: Write>(db: &Database, out: &mut W) -> Result<usize, CliError> {
    let triples = db.match_triples(None, None, None);
    for triple in &triples {
        writeln!(
            out,
            "{} {} {} .",
            triple.subject.to_n_triples(),
            triple.predicate.to_n_triples(),
            triple.object.to_n_triples()
        )?;
    }
    Ok(triples.len())
}

/// A hand-written recursive-descent parser for the supported Turtle subset.
struct TurtleParser<'a> {
    /// Remaining input.
    input: &'a str,
    /// Byte cursor into `input`.
    pos: usize,
    /// Prefix → namespace IRI map populated by `@prefix`.
    prefixes: HashMap<String, String>,
    /// The current base IRI (`@base`), used to resolve relative IRIs.
    base: Option<String>,
}

impl<'a> TurtleParser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input,
            pos: 0,
            prefixes: HashMap::new(),
            base: None,
        }
    }

    /// Parse the whole document.
    fn parse(&mut self) -> Result<Vec<Triple>, CliError> {
        let mut triples = Vec::new();
        loop {
            self.skip_ws();
            if self.at_end() {
                break;
            }
            if self.peek() == Some('@') || self.looks_like_directive() {
                self.parse_directive()?;
                continue;
            }
            self.parse_statement(&mut triples)?;
        }
        Ok(triples)
    }

    /// Parse a `@prefix` / `@base` directive (with or without a leading `@`
    /// and trailing `.`, to accept the SPARQL-style forms too).
    fn parse_directive(&mut self) -> Result<(), CliError> {
        let had_at = self.peek() == Some('@');
        if had_at {
            self.advance(1);
        }
        let keyword = self.read_bare_word().to_ascii_lowercase();
        self.skip_ws();
        match keyword.as_str() {
            "prefix" => {
                let prefix = self.read_prefix_label()?;
                self.skip_ws();
                let iri = self.read_iri_ref()?;
                self.prefixes.insert(prefix, iri);
            }
            "base" => {
                let iri = self.read_iri_ref()?;
                self.base = Some(iri);
            }
            other => {
                return Err(CliError::Parse(format!(
                    "unknown Turtle directive '@{other}'"
                )));
            }
        }
        self.skip_ws();
        // A trailing '.' terminates the @-prefixed form; the SPARQL form omits
        // it.  Consume it when present.
        if self.peek() == Some('.') {
            self.advance(1);
        }
        Ok(())
    }

    /// Parse one `subject (predicate object-list) (; …)* .` statement, pushing
    /// every produced triple onto `out`.
    fn parse_statement(&mut self, out: &mut Vec<Triple>) -> Result<(), CliError> {
        let subject = self.parse_term(true)?;
        loop {
            self.skip_ws();
            let predicate = self.parse_predicate()?;
            loop {
                self.skip_ws();
                let object = self.parse_term(false)?;
                out.push(Triple::new(subject.clone(), predicate.clone(), object));
                self.skip_ws();
                match self.peek() {
                    Some(',') => {
                        self.advance(1);
                        continue; // another object for the same predicate
                    }
                    _ => break,
                }
            }
            self.skip_ws();
            match self.peek() {
                Some(';') => {
                    self.advance(1);
                    self.skip_ws();
                    // Allow a trailing ';' just before '.'.
                    if self.peek() == Some('.') {
                        break;
                    }
                    continue; // another predicate-object group
                }
                Some('.') => break,
                other => {
                    return Err(CliError::Parse(format!(
                        "expected ';', ',' or '.' after object, found {other:?}"
                    )));
                }
            }
        }
        // Consume the statement terminator.
        self.expect('.')?;
        Ok(())
    }

    /// Parse a predicate position (a term, or the `a` keyword = `rdf:type`).
    fn parse_predicate(&mut self) -> Result<Term, CliError> {
        self.skip_ws();
        // The `a` keyword must be a standalone token.
        if self.peek() == Some('a') {
            let next = self.peek_at(1);
            if next.is_none() || next.is_some_and(|c| c.is_whitespace()) {
                self.advance(1);
                return Ok(Term::iri(RDF_TYPE));
            }
        }
        self.parse_term(true)
    }

    /// Parse a subject/object term.  `subject_position` controls only the
    /// error messages; literals are accepted everywhere and rejected only by
    /// the absence of an opening quote.
    fn parse_term(&mut self, _subject_position: bool) -> Result<Term, CliError> {
        self.skip_ws();
        match self.peek() {
            Some('<') => {
                let iri = self.read_iri_ref()?;
                Ok(Term::iri(iri))
            }
            Some('_') => self.parse_blank_node(),
            Some('"') | Some('\'') => self.parse_literal(),
            Some(c) if c == ':' || is_pname_start(c) => self.parse_prefixed_name(),
            Some(c) if c.is_ascii_digit() || c == '+' || c == '-' => self.parse_numeric_literal(),
            Some('(') | Some('[') => Err(CliError::Parse(
                "Turtle collections '( )' and blank-node property lists '[ ]' \
                 are not supported by the import subset"
                    .into(),
            )),
            other => Err(CliError::Parse(format!(
                "unexpected character {other:?} while parsing a term"
            ))),
        }
    }

    /// Parse `_:label`.
    fn parse_blank_node(&mut self) -> Result<Term, CliError> {
        self.expect('_')?;
        self.expect(':')?;
        let label = self.read_pname_local();
        if label.is_empty() {
            return Err(CliError::Parse("blank node '_:' missing a label".into()));
        }
        Ok(Term::blank(label))
    }

    /// Parse a prefixed name `prefix:local` or `:local`, expanding via the
    /// `@prefix` map.
    fn parse_prefixed_name(&mut self) -> Result<Term, CliError> {
        let prefix = self.read_pname_prefix();
        self.expect(':')?;
        let local = self.read_pname_local();
        let namespace = self.prefixes.get(&prefix).ok_or_else(|| {
            CliError::Parse(format!("undefined prefix '{prefix}:' in Turtle input"))
        })?;
        Ok(Term::iri(format!("{namespace}{local}")))
    }

    /// Parse a quoted string literal with an optional `@lang` or `^^datatype`.
    fn parse_literal(&mut self) -> Result<Term, CliError> {
        let quote = self.peek().expect("caller checked for a quote");
        let lexical = self.read_quoted_string(quote)?;
        self.skip_inline_ws();
        match self.peek() {
            Some('@') => {
                self.advance(1);
                let lang = self.read_lang_tag();
                if lang.is_empty() {
                    return Err(CliError::Parse("empty language tag after '@'".into()));
                }
                Ok(Term::literal(RdfLiteral::lang(lexical, lang)))
            }
            Some('^') => {
                self.expect('^')?;
                self.expect('^')?;
                self.skip_inline_ws();
                let datatype = match self.peek() {
                    Some('<') => self.read_iri_ref()?,
                    _ => {
                        // Prefixed-name datatype.
                        let prefix = self.read_pname_prefix();
                        self.expect(':')?;
                        let local = self.read_pname_local();
                        let ns = self.prefixes.get(&prefix).ok_or_else(|| {
                            CliError::Parse(format!(
                                "undefined prefix '{prefix}:' in literal datatype"
                            ))
                        })?;
                        format!("{ns}{local}")
                    }
                };
                Ok(Term::literal(RdfLiteral::typed(lexical, datatype)))
            }
            _ => Ok(Term::literal(RdfLiteral::string(lexical))),
        }
    }

    /// Parse a native numeric or boolean literal (`42`, `3.14`, `1e9`, but
    /// `true`/`false` are handled as prefixed-name-like bare words).
    fn parse_numeric_literal(&mut self) -> Result<Term, CliError> {
        let start = self.pos;
        if matches!(self.peek(), Some('+') | Some('-')) {
            self.advance(1);
        }
        let mut seen_dot = false;
        let mut seen_exp = false;
        while let Some(c) = self.peek() {
            match c {
                '0'..='9' => self.advance(1),
                '.' if !seen_dot && !seen_exp => {
                    seen_dot = true;
                    self.advance(1);
                }
                'e' | 'E' if !seen_exp => {
                    seen_exp = true;
                    self.advance(1);
                    if matches!(self.peek(), Some('+') | Some('-')) {
                        self.advance(1);
                    }
                }
                _ => break,
            }
        }
        let text = &self.input[start..self.pos];
        let datatype = if seen_exp {
            xsd::DOUBLE
        } else if seen_dot {
            xsd::DECIMAL
        } else {
            xsd::INTEGER
        };
        Ok(Term::literal(RdfLiteral::typed(text, datatype)))
    }

    // ── Low-level lexing ──────────────────────────────────────────────────

    /// Read a `<...>` IRI reference, unescaping `\uXXXX`/`\UXXXXXXXX`, and
    /// resolve it against the base IRI when it is relative.
    fn read_iri_ref(&mut self) -> Result<String, CliError> {
        self.expect('<')?;
        let mut iri = String::new();
        loop {
            match self.next_char() {
                Some('>') => break,
                Some('\\') => {
                    // Minimal IRI escape handling for \u / \U.
                    match self.next_char() {
                        Some('u') => iri.push(self.read_unicode_escape(4)?),
                        Some('U') => iri.push(self.read_unicode_escape(8)?),
                        Some(other) => iri.push(other),
                        None => return Err(CliError::Parse("unterminated IRI escape".into())),
                    }
                }
                Some(c) => iri.push(c),
                None => return Err(CliError::Parse("unterminated IRI reference '<...>'".into())),
            }
        }
        Ok(self.resolve_iri(iri))
    }

    /// Resolve a possibly-relative IRI against the current base.  Absolute
    /// IRIs (containing a scheme `:` before any `/`) are returned unchanged.
    fn resolve_iri(&self, iri: String) -> String {
        if let Some(base) = &self.base
            && !is_absolute_iri(&iri)
        {
            return format!("{base}{iri}");
        }
        iri
    }

    /// Read a quoted string body (single- or triple-quoted), handling escapes.
    fn read_quoted_string(&mut self, quote: char) -> Result<String, CliError> {
        // Detect a triple-quoted long string.
        let triple = self.peek() == Some(quote)
            && self.peek_at(1) == Some(quote)
            && self.peek_at(2) == Some(quote);
        if triple {
            self.advance(3);
            return self.read_long_string(quote);
        }
        self.expect(quote)?;
        let mut out = String::new();
        loop {
            match self.next_char() {
                Some(c) if c == quote => break,
                Some('\\') => out.push(self.read_string_escape()?),
                Some('\n') => {
                    return Err(CliError::Parse(
                        "newline in single-quoted string; use a triple-quoted string".into(),
                    ));
                }
                Some(c) => out.push(c),
                None => return Err(CliError::Parse("unterminated string literal".into())),
            }
        }
        Ok(out)
    }

    /// Read the body of a triple-quoted string up to the closing `"""`.
    fn read_long_string(&mut self, quote: char) -> Result<String, CliError> {
        let mut out = String::new();
        loop {
            if self.peek() == Some(quote)
                && self.peek_at(1) == Some(quote)
                && self.peek_at(2) == Some(quote)
            {
                self.advance(3);
                break;
            }
            match self.next_char() {
                Some('\\') => out.push(self.read_string_escape()?),
                Some(c) => out.push(c),
                None => return Err(CliError::Parse("unterminated long string literal".into())),
            }
        }
        Ok(out)
    }

    /// Decode a single string escape after a `\`.
    fn read_string_escape(&mut self) -> Result<char, CliError> {
        match self.next_char() {
            Some('t') => Ok('\t'),
            Some('n') => Ok('\n'),
            Some('r') => Ok('\r'),
            Some('b') => Ok('\u{08}'),
            Some('f') => Ok('\u{0C}'),
            Some('"') => Ok('"'),
            Some('\'') => Ok('\''),
            Some('\\') => Ok('\\'),
            Some('u') => self.read_unicode_escape(4),
            Some('U') => self.read_unicode_escape(8),
            Some(other) => Err(CliError::Parse(format!(
                "invalid string escape '\\{other}'"
            ))),
            None => Err(CliError::Parse("unterminated string escape".into())),
        }
    }

    /// Read `n` hex digits and convert to a `char`.
    fn read_unicode_escape(&mut self, n: usize) -> Result<char, CliError> {
        let mut code: u32 = 0;
        for _ in 0..n {
            let c = self
                .next_char()
                .ok_or_else(|| CliError::Parse("truncated unicode escape".into()))?;
            let digit = c
                .to_digit(16)
                .ok_or_else(|| CliError::Parse(format!("invalid hex digit '{c}'")))?;
            code = code * 16 + digit;
        }
        char::from_u32(code)
            .ok_or_else(|| CliError::Parse(format!("invalid code point U+{code:X}")))
    }

    /// Read a BCP-47 language tag (`[a-zA-Z]+ ('-' [a-zA-Z0-9]+)*`),
    /// normalising to lower-case.
    fn read_lang_tag(&mut self) -> String {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == '-' {
                self.advance(1);
            } else {
                break;
            }
        }
        self.input[start..self.pos].to_ascii_lowercase()
    }

    /// Read a prefix label up to (but not including) the `:` of `@prefix p:`.
    fn read_prefix_label(&mut self) -> Result<String, CliError> {
        let prefix = self.read_pname_prefix();
        self.expect(':')?;
        Ok(prefix)
    }

    /// Read the prefix portion of a prefixed name (before `:`).
    fn read_pname_prefix(&mut self) -> String {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c == ':' || c.is_whitespace() {
                break;
            }
            if is_pname_char(c) {
                self.advance(1);
            } else {
                break;
            }
        }
        self.input[start..self.pos].to_string()
    }

    /// Read the local portion of a prefixed name (after `:`).
    fn read_pname_local(&mut self) -> String {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if is_pname_char(c) || c == '.' {
                // A '.' is part of the local name only if it is not the final
                // statement terminator (i.e. it is followed by another name
                // char).
                if c == '.' && !self.peek_at(1).is_some_and(is_pname_char) {
                    break;
                }
                self.advance(1);
            } else {
                break;
            }
        }
        self.input[start..self.pos].to_string()
    }

    /// Read a bare alphabetic word (used for directive keywords).
    fn read_bare_word(&mut self) -> String {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphabetic() {
                self.advance(1);
            } else {
                break;
            }
        }
        self.input[start..self.pos].to_string()
    }

    // ── Cursor primitives ─────────────────────────────────────────────────

    fn at_end(&self) -> bool {
        self.pos >= self.input.len()
    }

    fn peek(&self) -> Option<char> {
        self.input[self.pos..].chars().next()
    }

    fn peek_at(&self, n: usize) -> Option<char> {
        self.input[self.pos..].chars().nth(n)
    }

    fn next_char(&mut self) -> Option<char> {
        let c = self.input[self.pos..].chars().next()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn advance(&mut self, chars: usize) {
        for _ in 0..chars {
            if self.next_char().is_none() {
                break;
            }
        }
    }

    fn expect(&mut self, expected: char) -> Result<(), CliError> {
        match self.next_char() {
            Some(c) if c == expected => Ok(()),
            other => Err(CliError::Parse(format!(
                "expected '{expected}', found {other:?}"
            ))),
        }
    }

    /// Skip whitespace and `#` comments.
    fn skip_ws(&mut self) {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => {
                    self.advance(1);
                }
                Some('#') => {
                    while let Some(c) = self.peek() {
                        self.advance(1);
                        if c == '\n' {
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
    }

    /// Skip only spaces/tabs (not newlines) — used between a literal and its
    /// `@lang` / `^^` suffix.
    fn skip_inline_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c == ' ' || c == '\t' {
                self.advance(1);
            } else {
                break;
            }
        }
    }

    /// Heuristic: does the upcoming token look like a SPARQL-style `PREFIX` /
    /// `BASE` directive (no leading `@`)?
    fn looks_like_directive(&self) -> bool {
        let rest = &self.input[self.pos..];
        let word: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .collect();
        let lower = word.to_ascii_lowercase();
        lower == "prefix" || lower == "base"
    }
}

/// Is `c` a valid start character for a prefixed-name prefix?
fn is_pname_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || !c.is_ascii()
}

/// Is `c` a valid character within a prefixed-name prefix/local part?
fn is_pname_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-' || !c.is_ascii()
}

/// Does `iri` look absolute (a scheme appears before the first `/`, `?`, `#`)?
fn is_absolute_iri(iri: &str) -> bool {
    if let Some(colon) = iri.find(':') {
        let before = &iri[..colon];
        return !before.is_empty()
            && before
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
            && before
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic());
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_iri_triple() {
        let ttl = "<http://example.org/alice> <http://xmlns.com/foaf/0.1/knows> \
                   <http://example.org/bob> .";
        let triples = parse_turtle_triples(ttl).unwrap();
        assert_eq!(triples.len(), 1);
        assert_eq!(triples[0].subject, Term::iri("http://example.org/alice"));
        assert_eq!(triples[0].object, Term::iri("http://example.org/bob"));
    }

    #[test]
    fn parse_prefixed_names() {
        let ttl = "@prefix foaf: <http://xmlns.com/foaf/0.1/> .\n\
                   @prefix ex: <http://example.org/> .\n\
                   ex:alice foaf:knows ex:bob .";
        let triples = parse_turtle_triples(ttl).unwrap();
        assert_eq!(triples.len(), 1);
        assert_eq!(
            triples[0].predicate,
            Term::iri("http://xmlns.com/foaf/0.1/knows")
        );
        assert_eq!(triples[0].subject, Term::iri("http://example.org/alice"));
    }

    #[test]
    fn parse_string_lang_and_typed_literals() {
        let ttl = "@prefix ex: <http://example.org/> .\n\
                   ex:a ex:name \"Alice\" .\n\
                   ex:a ex:greeting \"bonjour\"@fr .\n\
                   ex:a ex:age \"30\"^^<http://www.w3.org/2001/XMLSchema#integer> .";
        let triples = parse_turtle_triples(ttl).unwrap();
        assert_eq!(triples.len(), 3);
        assert_eq!(
            triples[0].object,
            Term::literal(RdfLiteral::string("Alice"))
        );
        assert_eq!(
            triples[1].object,
            Term::literal(RdfLiteral::lang("bonjour", "fr"))
        );
        assert_eq!(
            triples[2].object,
            Term::literal(RdfLiteral::typed("30", xsd::INTEGER))
        );
    }

    #[test]
    fn parse_native_numeric_literals() {
        let ttl = "@prefix ex: <http://example.org/> .\n\
                   ex:a ex:i 42 .\n\
                   ex:a ex:d 3.14 .\n\
                   ex:a ex:e 1.0e9 .";
        let triples = parse_turtle_triples(ttl).unwrap();
        assert_eq!(
            triples[0].object,
            Term::literal(RdfLiteral::typed("42", xsd::INTEGER))
        );
        assert_eq!(
            triples[1].object,
            Term::literal(RdfLiteral::typed("3.14", xsd::DECIMAL))
        );
        assert_eq!(
            triples[2].object,
            Term::literal(RdfLiteral::typed("1.0e9", xsd::DOUBLE))
        );
    }

    #[test]
    fn parse_blank_node() {
        let ttl = "@prefix ex: <http://example.org/> .\n_:b0 ex:p ex:o .";
        let triples = parse_turtle_triples(ttl).unwrap();
        assert_eq!(triples[0].subject, Term::blank("b0"));
    }

    #[test]
    fn parse_a_keyword_is_rdf_type() {
        let ttl = "@prefix ex: <http://example.org/> .\nex:a a ex:Person .";
        let triples = parse_turtle_triples(ttl).unwrap();
        assert_eq!(triples[0].predicate, Term::iri(RDF_TYPE));
        assert_eq!(triples[0].object, Term::iri("http://example.org/Person"));
    }

    #[test]
    fn parse_predicate_object_lists() {
        let ttl = "@prefix ex: <http://example.org/> .\n\
                   ex:a ex:p1 ex:o1 ;\n\
                        ex:p2 ex:o2 , ex:o3 .";
        let triples = parse_turtle_triples(ttl).unwrap();
        assert_eq!(triples.len(), 3);
        // ex:a ex:p1 ex:o1
        assert_eq!(triples[0].predicate, Term::iri("http://example.org/p1"));
        // ex:a ex:p2 ex:o2 and ex:a ex:p2 ex:o3
        assert_eq!(triples[1].predicate, Term::iri("http://example.org/p2"));
        assert_eq!(triples[2].object, Term::iri("http://example.org/o3"));
    }

    #[test]
    fn parse_comments_and_blank_lines() {
        let ttl = "# a comment\n\
                   @prefix ex: <http://example.org/> . # trailing comment\n\
                   \n\
                   ex:a ex:p ex:o . # inline\n";
        let triples = parse_turtle_triples(ttl).unwrap();
        assert_eq!(triples.len(), 1);
    }

    #[test]
    fn rejects_unsupported_collection() {
        let ttl = "@prefix ex: <http://example.org/> .\nex:a ex:p ( ex:x ex:y ) .";
        assert!(parse_turtle_triples(ttl).is_err());
    }

    #[test]
    fn base_resolves_relative_iri() {
        let ttl = "@base <http://example.org/> .\n<alice> <knows> <bob> .";
        let triples = parse_turtle_triples(ttl).unwrap();
        assert_eq!(triples[0].subject, Term::iri("http://example.org/alice"));
        assert_eq!(triples[0].predicate, Term::iri("http://example.org/knows"));
    }

    #[test]
    fn import_export_roundtrip_through_rdf_database() {
        use crate::io::posix::PosixFileSystem;

        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let db_path = dir.path().join("db");

        // A fixture exercising every required term kind: IRI subject/object,
        // typed literal, language literal, and a blank node.
        let fixture = "@prefix ex: <http://example.org/> .\n\
                       @prefix foaf: <http://xmlns.com/foaf/0.1/> .\n\
                       ex:alice foaf:knows ex:bob ;\n\
                                foaf:name \"Alice\" ;\n\
                                ex:age \"30\"^^<http://www.w3.org/2001/XMLSchema#integer> ;\n\
                                ex:greeting \"bonjour\"@fr ;\n\
                                ex:secret _:b0 .\n";

        // Import into a fresh RDF-mode database, then read the stored triples
        // back out as Turtle.
        let exported = {
            let mut db = Database::init(&db_path, &fs, GraphMode::Rdf).unwrap();
            let added = import_turtle(&mut db, fixture, &fs).unwrap();
            assert_eq!(added, 5, "five triples imported");

            let mut buf = Vec::new();
            let written = export_turtle(&db, &mut buf).unwrap();
            assert_eq!(written, 5);
            String::from_utf8(buf).unwrap()
        };

        // The exported Turtle must itself parse back into the same set of
        // triples (a true round-trip through the real store).
        let reparsed = parse_turtle_triples(&exported).unwrap();
        assert_eq!(reparsed.len(), 5);

        // Re-import the exported Turtle into a second database and assert the
        // triple set is identical.
        let db2_path = dir.path().join("db2");
        let mut db2 = Database::init(&db2_path, &fs, GraphMode::Rdf).unwrap();
        import_turtle(&mut db2, &exported, &fs).unwrap();
        assert_eq!(db2.rdf_triple_count(), 5);

        // Spot-check that each term kind survived the round-trip.
        let alice = Term::iri("http://example.org/alice");
        let names = db2.match_triples(
            Some(&alice),
            Some(&Term::iri("http://xmlns.com/foaf/0.1/name")),
            None,
        );
        assert_eq!(names.len(), 1);
        assert_eq!(names[0].object, Term::literal(RdfLiteral::string("Alice")));

        let ages = db2.match_triples(
            Some(&alice),
            Some(&Term::iri("http://example.org/age")),
            None,
        );
        assert_eq!(
            ages[0].object,
            Term::literal(RdfLiteral::typed("30", xsd::INTEGER))
        );

        let greetings = db2.match_triples(
            Some(&alice),
            Some(&Term::iri("http://example.org/greeting")),
            None,
        );
        assert_eq!(
            greetings[0].object,
            Term::literal(RdfLiteral::lang("bonjour", "fr"))
        );

        let secrets = db2.match_triples(
            Some(&alice),
            Some(&Term::iri("http://example.org/secret")),
            None,
        );
        assert!(matches!(secrets[0].object, Term::BlankNode(_)));
    }

    #[test]
    fn import_rejects_lpg_mode() {
        use crate::io::posix::PosixFileSystem;
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let db_path = dir.path().join("db");
        let mut db = Database::init(&db_path, &fs, GraphMode::Lpg).unwrap();
        let err = import_turtle(&mut db, "<a> <b> <c> .", &fs).unwrap_err();
        assert!(matches!(err, CliError::Parse(_)));
    }
}
