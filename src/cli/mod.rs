//! Command-line data tooling: bulk import, export, and benchmarking.
//!
//! These routines drive the real [`Graph`](crate::graph::graph::Graph) engine
//! through its public API, inside the engine's own durability path (every
//! mutation is followed by an engine `sync`).  They back the `rgraph import`,
//! `rgraph export`, and `rgraph benchmark` CLI subcommands (Task 179).
//!
//! # Supported formats
//!
//! | Format   | Import | Export | Notes                                            |
//! |----------|--------|--------|--------------------------------------------------|
//! | CSV      | yes    | —      | Header-driven; node vs edge detected by columns. |
//! | JSONL    | yes    | yes    | One JSON object per line (`{"type":"node",…}`).   |
//! | Cypher   | —      | yes    | `CREATE` statements.                              |
//! | Turtle   | yes    | yes    | Basic `subject predicate object .` triples.       |
//!
//! Reserved property keys [`LABEL_KEY`] (`_label`) and [`TYPE_KEY`] (`_type`)
//! carry the textual node label / edge type so that a round-trip survives an
//! engine restart even though the in-memory catalog is not yet persisted.

pub mod benchmark;
pub mod export;
pub mod import;
pub mod turtle;

/// Property key under which a node's textual label is stored on disk.
pub const LABEL_KEY: &str = "_label";

/// Property key under which an edge's textual relationship type is stored.
pub const TYPE_KEY: &str = "_type";

/// Property key under which a node's caller-supplied logical import key is
/// stored, so that edges imported in a *later* run (or a separate file) can
/// resolve their endpoints across process boundaries.
pub const KEY_KEY: &str = "_key";

/// Errors raised by the CLI data-tooling routines.
#[derive(Debug)]
pub enum CliError {
    /// The input could not be parsed in the requested format.
    Parse(String),
    /// An unknown or unsupported format string was supplied.
    UnknownFormat(String),
    /// The underlying storage engine returned an error.
    Storage(crate::graph::engine::StorageError),
    /// An I/O error occurred reading or writing a file.
    Io(std::io::Error),
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliError::Parse(m) => write!(f, "parse error: {m}"),
            CliError::UnknownFormat(m) => write!(f, "unknown format: {m}"),
            CliError::Storage(e) => write!(f, "storage error: {e}"),
            CliError::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for CliError {}

impl From<crate::graph::engine::StorageError> for CliError {
    fn from(e: crate::graph::engine::StorageError) -> Self {
        CliError::Storage(e)
    }
}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> Self {
        CliError::Io(e)
    }
}

/// The data format of an import source or export sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Comma-separated values, header-driven.
    Csv,
    /// JSON Lines — one JSON object per line.
    Jsonl,
    /// Cypher `CREATE` statements (export only).
    Cypher,
    /// RDF Turtle triples.
    Turtle,
}

impl Format {
    /// Parse a format name (case-insensitive).  Accepts the canonical names
    /// plus common aliases (`ttl` for Turtle, `jsonlines`/`ndjson` for JSONL).
    pub fn parse(s: &str) -> Result<Self, CliError> {
        match s.trim().to_ascii_lowercase().as_str() {
            "csv" => Ok(Format::Csv),
            "jsonl" | "jsonlines" | "ndjson" => Ok(Format::Jsonl),
            "cypher" | "cql" => Ok(Format::Cypher),
            "turtle" | "ttl" => Ok(Format::Turtle),
            other => Err(CliError::UnknownFormat(other.to_owned())),
        }
    }

    /// Infer the format from a file extension, falling back to `None` when the
    /// extension is unrecognised.
    pub fn from_extension(path: &std::path::Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "csv" => Some(Format::Csv),
            "jsonl" | "ndjson" => Some(Format::Jsonl),
            "cypher" | "cql" => Some(Format::Cypher),
            "ttl" | "turtle" => Some(Format::Turtle),
            "json" => Some(Format::Jsonl),
            _ => None,
        }
    }
}

/// Outcome counters returned by an import run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportCounts {
    /// Number of nodes created.
    pub nodes: usize,
    /// Number of relationships created.
    pub edges: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_parse_accepts_aliases() {
        assert_eq!(Format::parse("CSV").unwrap(), Format::Csv);
        assert_eq!(Format::parse("ndjson").unwrap(), Format::Jsonl);
        assert_eq!(Format::parse("ttl").unwrap(), Format::Turtle);
        assert_eq!(Format::parse("Cypher").unwrap(), Format::Cypher);
        assert!(Format::parse("xml").is_err());
    }

    #[test]
    fn format_from_extension() {
        use std::path::Path;
        assert_eq!(Format::from_extension(Path::new("a.csv")), Some(Format::Csv));
        assert_eq!(Format::from_extension(Path::new("a.jsonl")), Some(Format::Jsonl));
        assert_eq!(Format::from_extension(Path::new("a.ttl")), Some(Format::Turtle));
        assert_eq!(Format::from_extension(Path::new("a.unknown")), None);
    }
}
