//! Engine error type, mapped 1:1 onto the `EngineStatus` codes in `engine.h`.

use twill_storage::StorageError;

/// `EngineStatus` (see `include/engine.h`). The numeric values are part of the
/// stable C ABI and MUST NOT change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum EngineStatus {
    Ok = 0,
    ErrSql = 1,        // parse / plan / type error
    ErrConstraint = 2, // unique / fk / check violation
    ErrConflict = 3,   // serialization / write conflict
    ErrStorage = 4,    // backend I/O, CAS rejected, S3 fault
    ErrTxn = 5,        // illegal state-machine transition
    ErrMisuse = 6,     // null handle, use-after-free, bad arg
    ErrInternal = 7,   // bug; engine remains defined, never UB
}

#[derive(Debug)]
pub struct EngineError {
    pub status: EngineStatus,
    pub message: String,
}

impl EngineError {
    pub fn new(status: EngineStatus, message: impl Into<String>) -> EngineError {
        EngineError {
            status,
            message: message.into(),
        }
    }
    pub fn sql(msg: impl Into<String>) -> EngineError {
        EngineError::new(EngineStatus::ErrSql, msg)
    }
    pub fn constraint(msg: impl Into<String>) -> EngineError {
        EngineError::new(EngineStatus::ErrConstraint, msg)
    }
    pub fn txn(msg: impl Into<String>) -> EngineError {
        EngineError::new(EngineStatus::ErrTxn, msg)
    }
    pub fn misuse(msg: impl Into<String>) -> EngineError {
        EngineError::new(EngineStatus::ErrMisuse, msg)
    }
    pub fn internal(msg: impl Into<String>) -> EngineError {
        EngineError::new(EngineStatus::ErrInternal, msg)
    }
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for EngineError {}

impl From<StorageError> for EngineError {
    fn from(e: StorageError) -> EngineError {
        let status = match &e {
            StorageError::Fenced { .. } | StorageError::Contended => EngineStatus::ErrConflict,
            StorageError::Invalid(_) => EngineStatus::ErrMisuse,
            _ => EngineStatus::ErrStorage,
        };
        EngineError::new(status, e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, EngineError>;
