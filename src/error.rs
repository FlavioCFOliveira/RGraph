//! Crate-wide error enum and TCK error taxonomy mapping.
//!
//! [`RGraphError`] is the single error type returned by all public API
//! methods.  It captures source chains, maps to openCypher TCK error
//! classes, and records whether the failure occurred at compile time or
//! at runtime.

use std::fmt;
use std::io;
use thiserror::Error;

/// The top-level error type for every public operation in the crate.
#[derive(Debug, Error, Clone)]
pub enum RGraphError {
    /// I/O failure from the underlying file system.
    #[error("I/O error: {0}")]
    Io(String),

    /// Corruption detected in a page, WAL record, or superblock.
    #[error("corruption detected: {0}")]
    Corruption(String),

    /// A B+ tree operation failed.
    #[error("index error: {0}")]
    Index(String),

    /// A transaction operation failed (wound-wait, lock conflict, etc).
    #[error("transaction error: {0}")]
    Transaction(String),

    /// A storage engine operation failed.
    #[error("storage error: {0}")]
    Storage(String),

    /// Syntax error during query parsing.
    #[error("syntax error: {0}")]
    Syntax(String),

    /// Semantic error during query analysis (undefined variable, type mismatch, etc).
    #[error("semantic error: {0}")]
    Semantic(String),

    /// Type error detected at compile time or runtime.
    #[error("type error: {0}")]
    Type(String),

    /// Invalid argument supplied by the caller.
    #[error("argument error: {0}")]
    Argument(String),

    /// The requested entity was not found.
    #[error("not found: {0}")]
    NotFound(String),

    /// The entity already exists.
    #[error("already exists: {0}")]
    AlreadyExists(String),

    /// Resource exhausted (page full, memory limit, etc).
    #[error("resource exhausted: {0}")]
    ResourceExhausted(String),

    /// An invariant was violated (bug in the engine).
    #[error("internal invariant violated: {0}")]
    Internal(String),
}

/// Crate-wide result alias used by every public API.
pub type Result<T> = std::result::Result<T, RGraphError>;

impl From<io::Error> for RGraphError {
    fn from(err: io::Error) -> Self {
        RGraphError::Io(err.to_string())
    }
}

impl From<std::num::ParseIntError> for RGraphError {
    fn from(err: std::num::ParseIntError) -> Self {
        RGraphError::Argument(err.to_string())
    }
}

impl From<std::num::ParseFloatError> for RGraphError {
    fn from(err: std::num::ParseFloatError) -> Self {
        RGraphError::Argument(err.to_string())
    }
}

impl From<std::str::Utf8Error> for RGraphError {
    fn from(err: std::str::Utf8Error) -> Self {
        RGraphError::Corruption(err.to_string())
    }
}

impl From<std::string::FromUtf8Error> for RGraphError {
    fn from(err: std::string::FromUtf8Error) -> Self {
        RGraphError::Corruption(err.to_string())
    }
}

impl From<crate::cypher::executor::ExecError> for RGraphError {
    fn from(err: crate::cypher::executor::ExecError) -> Self {
        match err {
            crate::cypher::executor::ExecError::Semantic(msg) => RGraphError::Semantic(msg),
            crate::cypher::executor::ExecError::Eval(msg) => RGraphError::Type(msg),
            crate::cypher::executor::ExecError::Unsupported(msg) => RGraphError::Argument(msg),
        }
    }
}

/// TCK error class as defined by the openCypher TCK.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TckErrorClass {
    /// SyntaxError — malformed query.
    SyntaxError,
    /// SemanticError — well-formed but meaningless query.
    SemanticError,
    /// TypeError — operation on incompatible types.
    TypeError,
    /// ArgumentError — invalid parameter or argument.
    ArgumentError,
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
    pub fn class(err: &RGraphError) -> TckErrorClass {
        match err {
            RGraphError::Syntax(_) => TckErrorClass::SyntaxError,
            RGraphError::Semantic(_) => TckErrorClass::SemanticError,
            RGraphError::Type(_) => TckErrorClass::TypeError,
            RGraphError::Argument(_) => TckErrorClass::ArgumentError,
            // All others default to the closest TCK class.
            RGraphError::Io(_)
            | RGraphError::Corruption(_)
            | RGraphError::Index(_)
            | RGraphError::Storage(_)
            | RGraphError::ResourceExhausted(_) => TckErrorClass::ArgumentError,
            RGraphError::Transaction(_) => TckErrorClass::SemanticError,
            RGraphError::NotFound(_) | RGraphError::AlreadyExists(_) => TckErrorClass::SemanticError,
            RGraphError::Internal(_) => TckErrorClass::SemanticError,
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
        let err = RGraphError::Syntax("unexpected token".to_string());
        assert_eq!(ErrorRegistry::class(&err), TckErrorClass::SyntaxError);
        assert_eq!(ErrorRegistry::phase(&err), ErrorPhase::CompileTime);
    }

    #[test]
    fn semantic_error_maps_to_semantic_error_compile_time() {
        let err = RGraphError::Semantic("undefined variable".to_string());
        assert_eq!(ErrorRegistry::class(&err), TckErrorClass::SemanticError);
        assert_eq!(ErrorRegistry::phase(&err), ErrorPhase::CompileTime);
    }

    #[test]
    fn type_error_maps_to_type_error_compile_time() {
        let err = RGraphError::Type("expected Integer got String".to_string());
        assert_eq!(ErrorRegistry::class(&err), TckErrorClass::TypeError);
        assert_eq!(ErrorRegistry::phase(&err), ErrorPhase::CompileTime);
    }

    #[test]
    fn argument_error_maps_to_argument_error_compile_time() {
        let err = RGraphError::Argument("negative page size".to_string());
        assert_eq!(ErrorRegistry::class(&err), TckErrorClass::ArgumentError);
        assert_eq!(ErrorRegistry::phase(&err), ErrorPhase::CompileTime);
    }

    #[test]
    fn io_error_maps_to_argument_error_runtime() {
        let err = RGraphError::Io("disk full".to_string());
        assert_eq!(ErrorRegistry::class(&err), TckErrorClass::ArgumentError);
        assert_eq!(ErrorRegistry::phase(&err), ErrorPhase::Runtime);
    }

    #[test]
    fn transaction_error_maps_to_semantic_error_runtime() {
        let err = RGraphError::Transaction("wound-wait abort".to_string());
        assert_eq!(ErrorRegistry::class(&err), TckErrorClass::SemanticError);
        assert_eq!(ErrorRegistry::phase(&err), ErrorPhase::Runtime);
    }

    #[test]
    fn not_found_maps_to_semantic_error_runtime() {
        let err = RGraphError::NotFound("node 42".to_string());
        assert_eq!(ErrorRegistry::class(&err), TckErrorClass::SemanticError);
        assert_eq!(ErrorRegistry::phase(&err), ErrorPhase::Runtime);
    }

    #[test]
    fn already_exists_maps_to_semantic_error_runtime() {
        let err = RGraphError::AlreadyExists("node 42".to_string());
        assert_eq!(ErrorRegistry::class(&err), TckErrorClass::SemanticError);
        assert_eq!(ErrorRegistry::phase(&err), ErrorPhase::Runtime);
    }

    #[test]
    fn internal_error_maps_to_semantic_error_runtime() {
        let err = RGraphError::Internal("slot overflow".to_string());
        assert_eq!(ErrorRegistry::class(&err), TckErrorClass::SemanticError);
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
    fn io_error_converts_to_rgraph_error() {
        let io_err = io::Error::new(io::ErrorKind::NotFound, "file missing");
        let err: RGraphError = io_err.into();
        assert!(matches!(err, RGraphError::Io(_)));
    }

    #[test]
    fn parse_int_error_converts_to_rgraph_error() {
        let parse_err = "not_a_number".parse::<i32>().unwrap_err();
        let err: RGraphError = parse_err.into();
        assert!(matches!(err, RGraphError::Argument(_)));
    }

    #[test]
    fn result_alias_compiles() {
        fn returns_result() -> Result<i32> {
            Ok(42)
        }
        assert_eq!(returns_result().unwrap(), 42);
    }
}
