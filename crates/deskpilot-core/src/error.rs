use serde::Serialize;

/// Error categories shared by the engine and every platform adapter.
///
/// `kind()` is part of the tool contract: agents branch on it (for example
/// `elevation_required` means "retry with `elevated: true`").
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not supported: {0}")]
    Unsupported(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("administrator rights required: {0}")]
    ElevationRequired(String),
    #[error("blocked by policy: {0}")]
    PolicyDenied(String),
    #[error("timed out: {0}")]
    Timeout(String),
    #[error("cancelled by the user: {0}")]
    Cancelled(String),
    #[error("platform error: {0}")]
    Platform(String),
}

impl Error {
    pub fn kind(&self) -> &'static str {
        match self {
            Error::Unsupported(_) => "unsupported",
            Error::NotFound(_) => "not_found",
            Error::InvalidArgument(_) => "invalid_argument",
            Error::ElevationRequired(_) => "elevation_required",
            Error::PolicyDenied(_) => "policy_denied",
            Error::Timeout(_) => "timeout",
            Error::Cancelled(_) => "cancelled",
            Error::Platform(_) => "platform_error",
        }
    }

    pub fn to_json(&self) -> ErrorJson {
        ErrorJson {
            kind: self.kind(),
            message: self.to_string(),
        }
    }

    /// The message without the category prefix.
    pub fn message(&self) -> &str {
        match self {
            Error::Unsupported(m)
            | Error::NotFound(m)
            | Error::InvalidArgument(m)
            | Error::ElevationRequired(m)
            | Error::PolicyDenied(m)
            | Error::Timeout(m)
            | Error::Cancelled(m)
            | Error::Platform(m) => m,
        }
    }

    pub fn platform(err: impl std::fmt::Display) -> Self {
        Error::Platform(err.to_string())
    }
}

#[derive(Debug, Serialize)]
pub struct ErrorJson {
    pub kind: &'static str,
    pub message: String,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
