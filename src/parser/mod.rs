//! Error parsing and classification

mod classifier;
mod error;

pub use classifier::{
    classify_error, explain, ErrorClassification, ErrorSeverity, CANCELLED_MARKER, CATEGORIES,
    POST_INSTALL_MARKER, TIMEOUT_MARKER,
};
pub use error::ParsedError;
