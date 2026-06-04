//! RDF term types: IRIs, blank nodes, and typed/lang-tagged literals.
//!
//! An RDF *term* is the atomic value that occupies the subject, predicate,
//! object, or graph position of a triple/quad.  This module defines the
//! in-memory representation ([`Term`]) plus a canonical, order-preserving
//! byte encoding used by the term dictionary so that distinct terms map to
//! distinct dictionary keys.
//!
//! # Term kinds
//!
//! * [`Term::Iri`] — an absolute IRI / URI reference, e.g. `<http://x/a>`.
//! * [`Term::BlankNode`] — a local existential, written `_:id` in Turtle.
//! * [`Term::Literal`] — a lexical form plus a datatype IRI and an optional
//!   language tag (the language tag is only meaningful for
//!   `rdf:langString`).
//!
//! # Relationship to the Cypher value system
//!
//! RDF terms and Cypher [`Value`](crate::cypher::value::Value)s are distinct
//! type systems.  [`Term::Literal`] maps onto a Cypher value through
//! [`Term::to_cypher_value`] when the datatype is one of the common XSD
//! primitives (`xsd:integer`, `xsd:decimal`/`xsd:double`, `xsd:boolean`,
//! `xsd:string`, `xsd:dateTime`); other datatypes degrade to a Cypher
//! `String` carrying the lexical form, never breaking LPG equality.

use std::fmt;

/// Well-known XSD datatype IRIs used when mapping literals to Cypher values.
pub mod xsd {
    /// `xsd:string` — the default datatype for a plain literal.
    pub const STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
    /// `xsd:integer`.
    pub const INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
    /// `xsd:decimal`.
    pub const DECIMAL: &str = "http://www.w3.org/2001/XMLSchema#decimal";
    /// `xsd:double`.
    pub const DOUBLE: &str = "http://www.w3.org/2001/XMLSchema#double";
    /// `xsd:boolean`.
    pub const BOOLEAN: &str = "http://www.w3.org/2001/XMLSchema#boolean";
    /// `xsd:dateTime`.
    pub const DATE_TIME: &str = "http://www.w3.org/2001/XMLSchema#dateTime";
    /// `rdf:langString` — the datatype of a language-tagged string.
    pub const LANG_STRING: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#langString";
}

/// A typed or language-tagged RDF literal.
///
/// `lexical` is the literal's textual form exactly as written.  `datatype`
/// is the datatype IRI; for a language-tagged string it is
/// [`xsd::LANG_STRING`] and `language` carries the (lower-cased) BCP-47 tag.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RdfLiteral {
    /// The lexical form of the literal (unescaped).
    pub lexical: String,
    /// The datatype IRI.  Defaults to [`xsd::STRING`] for plain literals.
    pub datatype: String,
    /// The language tag, present only for `rdf:langString` literals.
    pub language: Option<String>,
}

impl RdfLiteral {
    /// Build a plain `xsd:string` literal.
    pub fn string(lexical: impl Into<String>) -> Self {
        Self {
            lexical: lexical.into(),
            datatype: xsd::STRING.to_string(),
            language: None,
        }
    }

    /// Build a typed literal with an explicit datatype IRI.
    pub fn typed(lexical: impl Into<String>, datatype: impl Into<String>) -> Self {
        Self {
            lexical: lexical.into(),
            datatype: datatype.into(),
            language: None,
        }
    }

    /// Build a language-tagged literal (`rdf:langString`).
    ///
    /// The language tag is normalised to lower-case to match RDF 1.1, which
    /// treats language tags case-insensitively for equality.
    pub fn lang(lexical: impl Into<String>, language: impl Into<String>) -> Self {
        Self {
            lexical: lexical.into(),
            datatype: xsd::LANG_STRING.to_string(),
            language: Some(language.into().to_ascii_lowercase()),
        }
    }
}

/// An RDF term occupying a subject/predicate/object/graph position.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Term {
    /// An absolute IRI reference.
    Iri(String),
    /// A blank node identified by a local label (without the `_:` prefix).
    BlankNode(String),
    /// A typed or language-tagged literal.
    Literal(RdfLiteral),
}

/// Single-byte discriminants for the canonical term encoding.
///
/// Ordering of the discriminants is deliberate: it gives a stable, total
/// byte order over terms (blank nodes < IRIs < literals) which the term
/// dictionary relies on for prefix scans.
mod tag {
    pub const BLANK: u8 = 0x01;
    pub const IRI: u8 = 0x02;
    pub const LITERAL: u8 = 0x03;
}

impl Term {
    /// Construct an IRI term.
    pub fn iri(value: impl Into<String>) -> Self {
        Term::Iri(value.into())
    }

    /// Construct a blank-node term from its local label (no `_:` prefix).
    pub fn blank(label: impl Into<String>) -> Self {
        Term::BlankNode(label.into())
    }

    /// Construct a literal term.
    pub fn literal(lit: RdfLiteral) -> Self {
        Term::Literal(lit)
    }

    /// Is this term an IRI?
    pub fn is_iri(&self) -> bool {
        matches!(self, Term::Iri(_))
    }

    /// Is this term a literal?
    pub fn is_literal(&self) -> bool {
        matches!(self, Term::Literal(_))
    }

    /// Borrow the IRI string when this term is an IRI.
    pub fn as_iri(&self) -> Option<&str> {
        match self {
            Term::Iri(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Encode this term into a canonical, self-delimiting byte string.
    ///
    /// The encoding is injective: two terms produce equal byte strings iff
    /// they are equal.  This is the key form stored in the term→id index of
    /// the dictionary.  Layout:
    ///
    /// ```text
    /// IRI        : 0x02 | utf8(iri)
    /// BlankNode  : 0x01 | utf8(label)
    /// Literal    : 0x03 | len(datatype):u16 | utf8(datatype)
    ///                   | len(lang):u16     | utf8(lang)
    ///                   | utf8(lexical)
    /// ```
    ///
    /// All length prefixes are big-endian.  The lexical form is last and
    /// runs to the end of the buffer, so no trailing length is required.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Term::BlankNode(label) => {
                let mut out = Vec::with_capacity(1 + label.len());
                out.push(tag::BLANK);
                out.extend_from_slice(label.as_bytes());
                out
            }
            Term::Iri(iri) => {
                let mut out = Vec::with_capacity(1 + iri.len());
                out.push(tag::IRI);
                out.extend_from_slice(iri.as_bytes());
                out
            }
            Term::Literal(lit) => {
                let dt = lit.datatype.as_bytes();
                let lang = lit.language.as_deref().unwrap_or("").as_bytes();
                let lex = lit.lexical.as_bytes();
                let mut out = Vec::with_capacity(1 + 2 + dt.len() + 2 + lang.len() + lex.len());
                out.push(tag::LITERAL);
                out.extend_from_slice(&(dt.len() as u16).to_be_bytes());
                out.extend_from_slice(dt);
                out.extend_from_slice(&(lang.len() as u16).to_be_bytes());
                out.extend_from_slice(lang);
                out.extend_from_slice(lex);
                out
            }
        }
    }

    /// Decode a term previously produced by [`Term::encode`].
    ///
    /// Returns `None` if the bytes are malformed (bad tag, truncated length
    /// prefix, or invalid UTF-8).
    pub fn decode(bytes: &[u8]) -> Option<Term> {
        let (&tag, rest) = bytes.split_first()?;
        match tag {
            tag::BLANK => {
                let label = std::str::from_utf8(rest).ok()?.to_string();
                Some(Term::BlankNode(label))
            }
            tag::IRI => {
                let iri = std::str::from_utf8(rest).ok()?.to_string();
                Some(Term::Iri(iri))
            }
            tag::LITERAL => {
                let mut off = 0usize;
                let dt_len = read_u16(rest, &mut off)? as usize;
                let datatype = read_str(rest, &mut off, dt_len)?;
                let lang_len = read_u16(rest, &mut off)? as usize;
                let lang = read_str(rest, &mut off, lang_len)?;
                let lexical = std::str::from_utf8(rest.get(off..)?).ok()?.to_string();
                Some(Term::Literal(RdfLiteral {
                    lexical,
                    datatype,
                    language: if lang.is_empty() { None } else { Some(lang) },
                }))
            }
            _ => None,
        }
    }

    /// Map this term to a Cypher [`Value`](crate::cypher::value::Value).
    ///
    /// IRIs and blank nodes become Cypher strings (their N-Triples form).
    /// Literals map by datatype: `xsd:integer` → `Integer`,
    /// `xsd:decimal`/`xsd:double` → `Float`, `xsd:boolean` → `Boolean`,
    /// everything else (including `xsd:string`, `xsd:dateTime`, and
    /// language-tagged strings) → `String` carrying the lexical form.  A
    /// numeric/boolean literal whose lexical form does not parse degrades to
    /// a `String` rather than erroring, so a malformed datatype annotation
    /// can never poison query evaluation.
    pub fn to_cypher_value(&self) -> crate::cypher::value::Value {
        use crate::cypher::value::Value;
        use crate::graph::property::OrderedF64;
        match self {
            Term::Iri(_) | Term::BlankNode(_) => Value::String(self.to_n_triples()),
            Term::Literal(lit) => match lit.datatype.as_str() {
                xsd::INTEGER => lit
                    .lexical
                    .parse::<i64>()
                    .map(Value::Integer)
                    .unwrap_or_else(|_| Value::String(lit.lexical.clone())),
                xsd::DECIMAL | xsd::DOUBLE => lit
                    .lexical
                    .parse::<f64>()
                    .map(|f| Value::Float(OrderedF64(f)))
                    .unwrap_or_else(|_| Value::String(lit.lexical.clone())),
                xsd::BOOLEAN => match lit.lexical.as_str() {
                    "true" | "1" => Value::Boolean(true),
                    "false" | "0" => Value::Boolean(false),
                    _ => Value::String(lit.lexical.clone()),
                },
                _ => Value::String(lit.lexical.clone()),
            },
        }
    }

    /// Render this term in N-Triples / Turtle term syntax.
    ///
    /// * IRI → `<iri>`
    /// * BlankNode → `_:label`
    /// * plain string literal → `"lexical"`
    /// * language literal → `"lexical"@lang`
    /// * typed literal → `"lexical"^^<datatype>`
    pub fn to_n_triples(&self) -> String {
        match self {
            Term::Iri(iri) => format!("<{}>", iri),
            Term::BlankNode(label) => format!("_:{}", label),
            Term::Literal(lit) => {
                let escaped = escape_literal(&lit.lexical);
                if let Some(lang) = &lit.language {
                    format!("\"{}\"@{}", escaped, lang)
                } else if lit.datatype == xsd::STRING {
                    format!("\"{}\"", escaped)
                } else {
                    format!("\"{}\"^^<{}>", escaped, lit.datatype)
                }
            }
        }
    }
}

impl fmt::Display for Term {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_n_triples())
    }
}

/// Escape a literal lexical form for N-Triples output (`\`, `"`, newline,
/// carriage return, tab).
fn escape_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out
}

/// Read a big-endian `u16` length prefix, advancing `off`.
fn read_u16(buf: &[u8], off: &mut usize) -> Option<u16> {
    let bytes = buf.get(*off..*off + 2)?;
    *off += 2;
    Some(u16::from_be_bytes([bytes[0], bytes[1]]))
}

/// Read a UTF-8 string of `len` bytes, advancing `off`.
fn read_str(buf: &[u8], off: &mut usize, len: usize) -> Option<String> {
    let bytes = buf.get(*off..*off + len)?;
    *off += len;
    Some(std::str::from_utf8(bytes).ok()?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cypher::value::Value;

    #[test]
    fn iri_encode_decode_roundtrip() {
        let t = Term::iri("http://example.org/alice");
        let bytes = t.encode();
        assert_eq!(Term::decode(&bytes), Some(t));
    }

    #[test]
    fn blank_node_encode_decode_roundtrip() {
        let t = Term::blank("b0");
        let bytes = t.encode();
        assert_eq!(Term::decode(&bytes), Some(t));
    }

    #[test]
    fn plain_literal_roundtrip() {
        let t = Term::literal(RdfLiteral::string("hello world"));
        let bytes = t.encode();
        assert_eq!(Term::decode(&bytes), Some(t));
    }

    #[test]
    fn typed_literal_roundtrip() {
        let t = Term::literal(RdfLiteral::typed("42", xsd::INTEGER));
        let bytes = t.encode();
        assert_eq!(Term::decode(&bytes), Some(t));
    }

    #[test]
    fn lang_literal_roundtrip() {
        let t = Term::literal(RdfLiteral::lang("bonjour", "FR"));
        let bytes = t.encode();
        let decoded = Term::decode(&bytes).unwrap();
        // Language tag is normalised to lower-case.
        assert_eq!(decoded, Term::literal(RdfLiteral::lang("bonjour", "fr")));
    }

    #[test]
    fn encoding_is_injective_across_kinds() {
        // An IRI and a blank node with the same payload text must not collide.
        let iri = Term::iri("x").encode();
        let blank = Term::blank("x").encode();
        assert_ne!(iri, blank);
    }

    #[test]
    fn literal_lexical_with_embedded_separators_roundtrips() {
        // Lexical forms containing bytes that look like length prefixes must
        // still round-trip because the datatype/lang lengths are explicit.
        let t = Term::literal(RdfLiteral::typed("a\u{0}b\u{1}c", "http://example.org/dt"));
        let bytes = t.encode();
        assert_eq!(Term::decode(&bytes), Some(t));
    }

    #[test]
    fn integer_literal_maps_to_cypher_integer() {
        let t = Term::literal(RdfLiteral::typed("42", xsd::INTEGER));
        assert_eq!(t.to_cypher_value(), Value::Integer(42));
    }

    #[test]
    fn boolean_literal_maps_to_cypher_boolean() {
        let t = Term::literal(RdfLiteral::typed("true", xsd::BOOLEAN));
        assert_eq!(t.to_cypher_value(), Value::Boolean(true));
    }

    #[test]
    fn malformed_numeric_literal_degrades_to_string() {
        // A datatype claims integer but the lexical form is not — must not
        // panic or error; falls back to a String value.
        let t = Term::literal(RdfLiteral::typed("not-a-number", xsd::INTEGER));
        assert_eq!(t.to_cypher_value(), Value::String("not-a-number".into()));
    }

    #[test]
    fn n_triples_rendering() {
        assert_eq!(Term::iri("http://x/a").to_n_triples(), "<http://x/a>");
        assert_eq!(Term::blank("b1").to_n_triples(), "_:b1");
        assert_eq!(
            Term::literal(RdfLiteral::string("hi")).to_n_triples(),
            "\"hi\""
        );
        assert_eq!(
            Term::literal(RdfLiteral::lang("hi", "en")).to_n_triples(),
            "\"hi\"@en"
        );
        assert_eq!(
            Term::literal(RdfLiteral::typed("42", xsd::INTEGER)).to_n_triples(),
            "\"42\"^^<http://www.w3.org/2001/XMLSchema#integer>"
        );
    }

    #[test]
    fn decode_rejects_bad_tag() {
        assert_eq!(Term::decode(&[0xFF, b'x']), None);
    }

    #[test]
    fn decode_rejects_truncated_literal() {
        // Tag says literal but there is no datatype length prefix.
        assert_eq!(Term::decode(&[tag::LITERAL]), None);
    }
}
