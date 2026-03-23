use crate::error::{AppError, Result};
use crate::history::{Conversation, SessionSource};
use std::os::unix::process::CommandExt;
use std::process::Command;

pub fn resume_session(conv: &Conversation) -> Result<()> {
    let err = match conv.source {
        SessionSource::Claude => Command::new("claude")
            .args(["--resume", &conv.session_id])
            .exec(),
        SessionSource::Codex => Command::new("codex")
            .args(["resume", &conv.session_id])
            .exec(),
    };
    Err(AppError::CliExecutionError(err.to_string()))
}
