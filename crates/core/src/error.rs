use std::io;

/// Everything that can go wrong in yoyomi, as one type that crosses crates.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A format or feature yoyomi does not read yet.
    #[error("{0}")]
    Unsupported(String),
    /// A newer request or a closed book stopped the work.
    #[error("cancelled")]
    Cancelled,
    /// The input exceeds a safety limit, such as the size of a page.
    #[error("{0}")]
    Limit(String),
    #[error(transparent)]
    Io(#[from] io::Error),
    /// Image data that does not decode.
    #[error("{0}")]
    Decode(String),
    /// An external tool failed, such as pdftoppm or tesseract.
    #[error("{tool}: {message}")]
    Tool { tool: String, message: String },
    /// A file whose contents break its format.
    #[error("{0}")]
    Format(String),
}

/// The result type of every fallible yoyomi function.
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// An `Unsupported` error with the given message.
    pub fn unsupported(message: impl Into<String>) -> Self {
        Error::Unsupported(message.into())
    }

    /// Whether the error only reports cancelled work.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Error::Cancelled)
    }
}

// Requests waiting on one failed decode each get the failure, and an I/O
// error keeps its kind and message in the copy.
impl Clone for Error {
    fn clone(&self) -> Self {
        match self {
            Error::Unsupported(message) => Error::Unsupported(message.clone()),
            Error::Cancelled => Error::Cancelled,
            Error::Limit(message) => Error::Limit(message.clone()),
            Error::Io(error) => Error::Io(io::Error::new(error.kind(), error.to_string())),
            Error::Decode(message) => Error::Decode(message.clone()),
            Error::Tool { tool, message } => Error::Tool { tool: tool.clone(), message: message.clone() },
            Error::Format(message) => Error::Format(message.clone()),
        }
    }
}

impl From<image::ImageError> for Error {
    fn from(error: image::ImageError) -> Self {
        match error {
            image::ImageError::Limits(limit) => Error::Limit(limit.to_string()),
            image::ImageError::IoError(error) => Error::Io(error),
            other => Error::Decode(other.to_string()),
        }
    }
}
