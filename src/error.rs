//! Crate-wide error enum and TCK error taxonomy mapping.
//!
//! [`RGraphError`] is the single error type returned by all public API
//! methods.  It captures source chains, maps to openCypher TCK error
//! classes, and records whether the failure occurred at compile time or
//! at runtime.
//!
//! # Source chaining
//!
//! Every variant carries a human-readable message *and* an optional
//! boxed source error ([`ErrorSource`]).  The underlying cause is
//! reachable through [`std::error::Error::source`], so the standard
//! `{:#}` alternate formatting and `anyhow`-style chain walkers print the
//! full causal chain:
//!
//! ```
//! use rgraph::error::RGraphError;
//! use std::error::Error;
//!
//! let io = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
//! let err: RGraphError = io.into();
//! // The original io::Error is reachable via `.source()`.
//! assert!(err.source().is_some());
//! ```
//!
//! # TCK taxonomy vs. engine failures
//!
//! The openCypher TCK defines exactly four error classes:
//! `SyntaxError`, `SemanticError`, `TypeError` and `ArgumentError`.  Those
//! classes describe problems with a *query*.  Failures originating in the
//! engine itself (I/O, corruption, index, storage, resource exhaustion,
//! internal invariant violations) are **not** query errors and must not be
//! reported under a TCK query class.  They map to the dedicated
//! [`TckErrorClass::DatabaseError`] class instead.

use std::error::Error as StdError;
use std::fmt;
use std::io;
use std::sync::Arc;

/// A boxed, shareable source error.
///
/// `Arc` (rather than `Box`) is used so that [`RGraphError`] can remain
/// `Clone` while still carrying a live `&dyn Error` reachable through
/// [`std::error::Error::source`].
pub type ErrorSource = Arc<dyn StdError + Send + Sync + 'static>;

/// A diagnostic message paired with an optional underlying cause.
///
/// This is the payload of every [`RGraphError`] variant.  It implements
/// `From<String>` and `From<&str>` so existing call sites that construct
/// errors from a plain message (`RGraphError::Io("disk full".into())`)
/// continue to compile unchanged, while richer call sites can attach a
/// source via [`Msg::with_source`].
#[derive(Debug, Clone)]
pub struct Msg {
    /// The human-readable description of the failure.
    text: String,
    /// The optional underlying cause, reachable via `source()`.
    source: Option<ErrorSource>,
}

impl Msg {
    /// Create a message with no underlying cause.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            source: None,
        }
    }

    /// Attach an underlying cause to this message.
    #[must_use]
    pub fn with_source(mut self, source: impl StdError + Send + Sync + 'static) -> Self {
        self.source = Some(Arc::new(source));
        self
    }

    /// The message text without the source chain.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The underlying cause, if any.
    pub fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.source
            .as_deref()
            .map(|s| s as &(dyn StdError + 'static))
    }
}

impl fmt::Display for Msg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl From<String> for Msg {
    fn from(text: String) -> Self {
        Msg::new(text)
    }
}

impl From<&str> for Msg {
    fn from(text: &str) -> Self {
        Msg::new(text)
    }
}

/// Two messages are equal when their text is equal.  The source chain is
/// intentionally excluded so that `RGraphError` equality is structural and
/// does not depend on a non-comparable `dyn Error`.
impl PartialEq for Msg {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
    }
}

impl Eq for Msg {}

/// The top-level error type for every public operation in the crate.
///
/// Each variant carries a [`Msg`] (message + optional source).  See the
/// [module documentation](self) for the source-chaining and TCK-mapping
/// contract.
#[derive(Debug, Clone)]
pub enum RGraphError {
    /// I/O failure from the underlying file system.
    Io(Msg),

    /// Corruption detected in a page, WAL record, or superblock.
    Corruption(Msg),

    /// A B+ tree operation failed.
    Index(Msg),

    /// A transaction operation failed (wound-wait, lock conflict, etc).
    Transaction(Msg),

    /// A storage engine operation failed.
    Storage(Msg),

    /// Syntax error during query parsing.
    Syntax(Msg),

    /// Semantic error during query analysis (undefined variable, type mismatch, etc).
    Semantic(Msg),

    /// Type error detected at compile time or runtime.
    Type(Msg),

    /// Invalid argument supplied by the caller.
    Argument(Msg),

    /// The requested entity was not found.
    NotFound(Msg),

    /// The entity already exists.
    AlreadyExists(Msg),

    /// Resource exhausted (page full, memory limit, etc).
    ResourceExhausted(Msg),

    /// An invariant was violated (bug in the engine).
    Internal(Msg),
}

impl RGraphError {
    /// Borrow the [`Msg`] payload of this error.
    fn msg(&self) -> &Msg {
        match self {
            RGraphError::Io(m)
            | RGraphError::Corruption(m)
            | RGraphError::Index(m)
            | RGraphError::Transaction(m)
            | RGraphError::Storage(m)
            | RGraphError::Syntax(m)
            | RGraphError::Semantic(m)
            | RGraphError::Type(m)
            | RGraphError::Argument(m)
            | RGraphError::NotFound(m)
            | RGraphError::AlreadyExists(m)
            | RGraphError::ResourceExhausted(m)
            | RGraphError::Internal(m) => m,
        }
    }

    /// Consume this error and reconstruct the same variant with `source`
    /// attached to its message.
    ///
    /// This lets call sites preserve an underlying cause without changing
    /// the variant they construct:
    ///
    /// ```
    /// use rgraph::error::RGraphError;
    /// use std::error::Error;
    ///
    /// let io = std::io::Error::new(std::io::ErrorKind::Other, "boom");
    /// let err = RGraphError::Storage("write failed".into()).with_source(io);
    /// assert!(err.source().is_some());
    /// ```
    #[must_use]
    pub fn with_source(self, source: impl StdError + Send + Sync + 'static) -> Self {
        match self {
            RGraphError::Io(m) => RGraphError::Io(m.with_source(source)),
            RGraphError::Corruption(m) => RGraphError::Corruption(m.with_source(source)),
            RGraphError::Index(m) => RGraphError::Index(m.with_source(source)),
            RGraphError::Transaction(m) => RGraphError::Transaction(m.with_source(source)),
            RGraphError::Storage(m) => RGraphError::Storage(m.with_source(source)),
            RGraphError::Syntax(m) => RGraphError::Syntax(m.with_source(source)),
            RGraphError::Semantic(m) => RGraphError::Semantic(m.with_source(source)),
            RGraphError::Type(m) => RGraphError::Type(m.with_source(source)),
            RGraphError::Argument(m) => RGraphError::Argument(m.with_source(source)),
            RGraphError::NotFound(m) => RGraphError::NotFound(m.with_source(source)),
            RGraphError::AlreadyExists(m) => RGraphError::AlreadyExists(m.with_source(source)),
            RGraphError::ResourceExhausted(m) => {
                RGraphError::ResourceExhausted(m.with_source(source))
            }
            RGraphError::Internal(m) => RGraphError::Internal(m.with_source(source)),
        }
    }

    /// The static prefix used when rendering this variant.
    fn prefix(&self) -> &'static str {
        match self {
            RGraphError::Io(_) => "I/O error",
            RGraphError::Corruption(_) => "corruption detected",
            RGraphError::Index(_) => "index error",
            RGraphError::Transaction(_) => "transaction error",
            RGraphError::Storage(_) => "storage error",
            RGraphError::Syntax(_) => "syntax error",
            RGraphError::Semantic(_) => "semantic error",
            RGraphError::Type(_) => "type error",
            RGraphError::Argument(_) => "argument error",
            RGraphError::NotFound(_) => "not found",
            RGraphError::AlreadyExists(_) => "already exists",
            RGraphError::ResourceExhausted(_) => "resource exhausted",
            RGraphError::Internal(_) => "internal invariant violated",
        }
    }
}

impl fmt::Display for RGraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.prefix(), self.msg().text())
    }
}

impl StdError for RGraphError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.msg().source()
    }
}

/// Crate-wide result alias used by every public API.
pub type Result<T> = std::result::Result<T, RGraphError>;

impl From<io::Error> for RGraphError {
    fn from(err: io::Error) -> Self {
        RGraphError::Io(Msg::new(err.to_string()).with_source(err))
    }
}

impl From<std::num::ParseIntError> for RGraphError {
    fn from(err: std::num::ParseIntError) -> Self {
        RGraphError::Argument(Msg::new(err.to_string()).with_source(err))
    }
}

impl From<std::num::ParseFloatError> for RGraphError {
    fn from(err: std::num::ParseFloatError) -> Self {
        RGraphError::Argument(Msg::new(err.to_string()).with_source(err))
    }
}

impl From<std::str::Utf8Error> for RGraphError {
    fn from(err: std::str::Utf8Error) -> Self {
        RGraphError::Corruption(Msg::new(err.to_string()).with_source(err))
    }
}

impl From<std::string::FromUtf8Error> for RGraphError {
    fn from(err: std::string::FromUtf8Error) -> Self {
        RGraphError::Corruption(Msg::new(err.to_string()).with_source(err))
    }
}

impl From<crate::cypher::executor::ExecError> for RGraphError {
    fn from(err: crate::cypher::executor::ExecError) -> Self {
        match err {
            crate::cypher::executor::ExecError::Semantic(msg) => RGraphError::Semantic(msg.into()),
            crate::cypher::executor::ExecError::Eval(msg) => RGraphError::Type(msg.into()),
            crate::cypher::executor::ExecError::Unsupported(msg) => {
                RGraphError::Argument(msg.into())
            }
        }
    }
}

/// TCK error class.
///
/// The first four classes are the openCypher TCK query-error taxonomy.
/// [`TckErrorClass::DatabaseError`] is an RGraph extension used for
/// failures that originate inside the engine rather than in the query
/// (I/O, corruption, storage, index, resource exhaustion, internal
/// invariants).  Such failures are not part of the openCypher TCK and must
/// never be reported under a query-error class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
// The variant names are the canonical openCypher TCK error-class names; the
// shared `Error` suffix is mandated by the spec taxonomy, not incidental, so
// the `enum_variant_names` lint is suppressed.
#[allow(clippy::enum_variant_names)]
pub enum TckErrorClass {
    /// SyntaxError — malformed query.
    SyntaxError,
    /// SemanticError — well-formed but meaningless query.
    SemanticError,
    /// TypeError — operation on incompatible types.
    TypeError,
    /// ArgumentError — invalid parameter or argument.
    ArgumentError,
    /// DatabaseError — an engine-internal failure that is not a query
    /// error.  This is an RGraph extension outside the openCypher TCK
    /// taxonomy.
    DatabaseError,
}

/// Phase at which the error was detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorPhase {
    /// Detected while parsing or analysing the query.
    CompileTime,
    /// Detected while executing the query.
    Runtime,
}

/// Mapping from [`RGraphError`] to TCK class and phase.
pub struct ErrorRegistry;

impl ErrorRegistry {
    /// Return the TCK error class for `err`.
    ///
    /// Cypher query errors map to their proper TCK class; engine-internal
    /// failures map to [`TckErrorClass::DatabaseError`].
    pub fn class(err: &RGraphError) -> TckErrorClass {
        match err {
            // openCypher TCK query-error taxonomy.
            RGraphError::Syntax(_) => TckErrorClass::SyntaxError,
            RGraphError::Semantic(_) => TckErrorClass::SemanticError,
            RGraphError::Type(_) => TckErrorClass::TypeError,
            RGraphError::Argument(_) => TckErrorClass::ArgumentError,
            // Engine-internal failures are NOT query errors: they map to
            // the dedicated DatabaseError class, never to ArgumentError.
            RGraphError::Io(_)
            | RGraphError::Corruption(_)
            | RGraphError::Index(_)
            | RGraphError::Storage(_)
            | RGraphError::ResourceExhausted(_)
            | RGraphError::Transaction(_)
            | RGraphError::NotFound(_)
            | RGraphError::AlreadyExists(_)
            | RGraphError::Internal(_) => TckErrorClass::DatabaseError,
        }
    }

    /// Return the phase at which `err` was detected.
    pub fn phase(err: &RGraphError) -> ErrorPhase {
        match err {
            // Parsing and semantic analysis happen at compile time.
            RGraphError::Syntax(_) | RGraphError::Semantic(_) => ErrorPhase::CompileTime,
            // Type errors can be detected at compile time or runtime.
            // We default to compile time for now; runtime type errors
            // are created explicitly via a different constructor.
            RGraphError::Type(_) => ErrorPhase::CompileTime,
            RGraphError::Argument(_) => ErrorPhase::CompileTime,
            // Everything else is a runtime failure.
            RGraphError::Io(_)
            | RGraphError::Corruption(_)
            | RGraphError::Index(_)
            | RGraphError::Transaction(_)
            | RGraphError::Storage(_)
            | RGraphError::NotFound(_)
            | RGraphError::AlreadyExists(_)
            | RGraphError::ResourceExhausted(_)
            | RGraphError::Internal(_) => ErrorPhase::Runtime,
        }
    }
}

impl fmt::Display for TckErrorClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TckErrorClass::SyntaxError => write!(f, "SyntaxError"),
            TckErrorClass::SemanticError => write!(f, "SemanticError"),
            TckErrorClass::TypeError => write!(f, "TypeError"),
            TckErrorClass::ArgumentError => write!(f, "ArgumentError"),
            TckErrorClass::DatabaseError => write!(f, "DatabaseError"),
        }
    }
}

impl fmt::Display for ErrorPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErrorPhase::CompileTime => write!(f, "CompileTime"),
            ErrorPhase::Runtime => write!(f, "Runtime"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syntax_error_maps_to_syntax_error_compile_time() {
        let err = RGraphError::Syntax("unexpected token".into());
        assert_eq!(ErrorRegistry::class(&err), TckErrorClass::SyntaxError);
        assert_eq!(ErrorRegistry::phase(&err), ErrorPhase::CompileTime);
    }

    #[test]
    fn semantic_error_maps_to_semantic_error_compile_time() {
        let err = RGraphError::Semantic("undefined variable".into());
        assert_eq!(ErrorRegistry::class(&err), TckErrorClass::SemanticError);
        assert_eq!(ErrorRegistry::phase(&err), ErrorPhase::CompileTime);
    }

    #[test]
    fn type_error_maps_to_type_error_compile_time() {
        let err = RGraphError::Type("expected Integer got String".into());
        assert_eq!(ErrorRegistry::class(&err), TckErrorClass::TypeError);
        assert_eq!(ErrorRegistry::phase(&err), ErrorPhase::CompileTime);
    }

    #[test]
    fn argument_error_maps_to_argument_error_compile_time() {
        let err = RGraphError::Argument("negative page size".into());
        assert_eq!(ErrorRegistry::class(&err), TckErrorClass::ArgumentError);
        assert_eq!(ErrorRegistry::phase(&err), ErrorPhase::CompileTime);
    }

    #[test]
    fn io_error_maps_to_database_error_runtime() {
        let err = RGraphError::Io("disk full".into());
        assert_eq!(ErrorRegistry::class(&err), TckErrorClass::DatabaseError);
        assert_eq!(ErrorRegistry::phase(&err), ErrorPhase::Runtime);
    }

    #[test]
    fn corruption_error_maps_to_database_error_runtime() {
        let err = RGraphError::Corruption("bad checksum".into());
        assert_eq!(ErrorRegistry::class(&err), TckErrorClass::DatabaseError);
        assert_eq!(ErrorRegistry::phase(&err), ErrorPhase::Runtime);
    }

    #[test]
    fn index_error_maps_to_database_error_runtime() {
        let err = RGraphError::Index("split failed".into());
        assert_eq!(ErrorRegistry::class(&err), TckErrorClass::DatabaseError);
        assert_eq!(ErrorRegistry::phase(&err), ErrorPhase::Runtime);
    }

    #[test]
    fn storage_error_maps_to_database_error_runtime() {
        let err = RGraphError::Storage("page write failed".into());
        assert_eq!(ErrorRegistry::class(&err), TckErrorClass::DatabaseError);
        assert_eq!(ErrorRegistry::phase(&err), ErrorPhase::Runtime);
    }

    #[test]
    fn resource_exhausted_maps_to_database_error_runtime() {
        let err = RGraphError::ResourceExhausted("page full".into());
        assert_eq!(ErrorRegistry::class(&err), TckErrorClass::DatabaseError);
        assert_eq!(ErrorRegistry::phase(&err), ErrorPhase::Runtime);
    }

    #[test]
    fn transaction_error_maps_to_database_error_runtime() {
        let err = RGraphError::Transaction("wound-wait abort".into());
        assert_eq!(ErrorRegistry::class(&err), TckErrorClass::DatabaseError);
        assert_eq!(ErrorRegistry::phase(&err), ErrorPhase::Runtime);
    }

    #[test]
    fn not_found_maps_to_database_error_runtime() {
        let err = RGraphError::NotFound("node 42".into());
        assert_eq!(ErrorRegistry::class(&err), TckErrorClass::DatabaseError);
        assert_eq!(ErrorRegistry::phase(&err), ErrorPhase::Runtime);
    }

    #[test]
    fn already_exists_maps_to_database_error_runtime() {
        let err = RGraphError::AlreadyExists("node 42".into());
        assert_eq!(ErrorRegistry::class(&err), TckErrorClass::DatabaseError);
        assert_eq!(ErrorRegistry::phase(&err), ErrorPhase::Runtime);
    }

    #[test]
    fn internal_error_maps_to_database_error_runtime() {
        let err = RGraphError::Internal("slot overflow".into());
        assert_eq!(ErrorRegistry::class(&err), TckErrorClass::DatabaseError);
        assert_eq!(ErrorRegistry::phase(&err), ErrorPhase::Runtime);
    }

    #[test]
    fn every_variant_is_mapped() {
        // Exhaustive check: instantiate every variant and verify it does not panic.
        let variants = vec![
            RGraphError::Io("x".into()),
            RGraphError::Corruption("x".into()),
            RGraphError::Index("x".into()),
            RGraphError::Transaction("x".into()),
            RGraphError::Storage("x".into()),
            RGraphError::Syntax("x".into()),
            RGraphError::Semantic("x".into()),
            RGraphError::Type("x".into()),
            RGraphError::Argument("x".into()),
            RGraphError::NotFound("x".into()),
            RGraphError::AlreadyExists("x".into()),
            RGraphError::ResourceExhausted("x".into()),
            RGraphError::Internal("x".into()),
        ];
        for err in variants {
            let _class = ErrorRegistry::class(&err);
            let _phase = ErrorRegistry::phase(&err);
        }
    }

    #[test]
    fn no_engine_variant_maps_to_a_query_class() {
        // Engine-internal failures must never masquerade as query errors.
        let engine = vec![
            RGraphError::Io("x".into()),
            RGraphError::Corruption("x".into()),
            RGraphError::Index("x".into()),
            RGraphError::Transaction("x".into()),
            RGraphError::Storage("x".into()),
            RGraphError::NotFound("x".into()),
            RGraphError::AlreadyExists("x".into()),
            RGraphError::ResourceExhausted("x".into()),
            RGraphError::Internal("x".into()),
        ];
        for err in engine {
            assert_eq!(
                ErrorRegistry::class(&err),
                TckErrorClass::DatabaseError,
                "{err} should be a DatabaseError, not a query error",
            );
        }
    }

    #[test]
    fn io_error_converts_to_rgraph_error() {
        let io_err = io::Error::new(io::ErrorKind::NotFound, "file missing");
        let err: RGraphError = io_err.into();
        assert!(matches!(err, RGraphError::Io(_)));
    }

    #[test]
    fn io_error_source_is_preserved() {
        // A wrapped io::Error must be reachable via std::error::Error::source.
        let io_err = io::Error::new(io::ErrorKind::PermissionDenied, "denied");
        let err: RGraphError = io_err.into();

        let source = StdError::source(&err).expect("source chain must be present");
        let io_source = source
            .downcast_ref::<io::Error>()
            .expect("source must be the original io::Error");
        assert_eq!(io_source.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn with_source_attaches_and_preserves_cause() {
        let io_err = io::Error::new(io::ErrorKind::Other, "boom");
        let err = RGraphError::Storage("write failed".into()).with_source(io_err);

        // The message is the variant's own message...
        assert!(err.to_string().contains("write failed"));
        // ...and the cause is reachable via source().
        let source = StdError::source(&err).expect("source must be present");
        assert!(source.downcast_ref::<io::Error>().is_some());
    }

    #[test]
    fn alternate_formatting_walks_the_chain() {
        // `{:#}` / manual chain walking should reach the underlying cause.
        let io_err = io::Error::new(io::ErrorKind::NotFound, "missing file");
        let err: RGraphError = io_err.into();

        let mut chain: Vec<String> = Vec::new();
        let mut current: Option<&dyn StdError> = Some(&err);
        while let Some(e) = current {
            chain.push(e.to_string());
            current = e.source();
        }
        assert!(chain.len() >= 2, "chain should contain error and its cause");
        assert!(chain.last().unwrap().contains("missing file"));
    }

    #[test]
    fn message_without_source_has_no_cause() {
        let err = RGraphError::Internal("invariant".into());
        assert!(StdError::source(&err).is_none());
    }

    #[test]
    fn parse_int_error_converts_to_rgraph_error() {
        let parse_err = "not_a_number".parse::<i32>().unwrap_err();
        let err: RGraphError = parse_err.into();
        assert!(matches!(err, RGraphError::Argument(_)));
        // The ParseIntError is preserved as the source.
        assert!(StdError::source(&err).is_some());
    }

    #[test]
    fn display_renders_prefix_and_message() {
        let err = RGraphError::Syntax("unexpected token".into());
        assert_eq!(err.to_string(), "syntax error: unexpected token");
    }

    #[test]
    fn clone_preserves_message_and_source() {
        let io_err = io::Error::new(io::ErrorKind::Other, "boom");
        let err = RGraphError::Storage("oops".into()).with_source(io_err);
        let cloned = err.clone();
        assert_eq!(cloned.to_string(), err.to_string());
        assert!(StdError::source(&cloned).is_some());
    }

    #[test]
    fn database_error_class_displays() {
        assert_eq!(TckErrorClass::DatabaseError.to_string(), "DatabaseError");
    }

    #[test]
    fn result_alias_compiles() {
        fn returns_result() -> Result<i32> {
            Ok(42)
        }
        assert_eq!(returns_result().unwrap(), 42);
    }
}
