use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON parsing error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Claude projects directory not found at {0}")]
    ProjectsDirNotFound(String),

    #[error("No conversation history found")]
    NoHistoryFound,

    #[error("Session not found: {0}")]
    SessionNotFound(String),

    #[error("Failed to execute CLI: {0}")]
    CliExecutionError(String),
}

pub type Result<T> = std::result::Result<T, AppError>;
