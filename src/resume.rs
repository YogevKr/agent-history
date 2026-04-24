use crate::error::{AppError, Result};
use crate::history::{Conversation, SessionSource};
use crate::path::decode_project_dir_name_to_path;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

pub fn resume_session(conv: &Conversation) -> Result<()> {
    let err = build_resume_command(conv).exec();
    Err(AppError::CliExecutionError(err.to_string()))
}

fn build_resume_command(conv: &Conversation) -> Command {
    let mut command = match conv.source {
        SessionSource::Claude => {
            let mut command = Command::new("claude");
            command.args([
                "--dangerously-skip-permissions",
                "--resume",
                &conv.session_id,
            ]);
            command
        }
        SessionSource::Codex => {
            let mut command = Command::new("codex");
            command.args(["--yolo", "resume", &conv.session_id]);
            command
        }
    };

    if let Some(cwd) = resume_cwd(conv) {
        command.current_dir(cwd);
    }

    command
}

fn resume_cwd(conv: &Conversation) -> Option<PathBuf> {
    match conv.source {
        SessionSource::Claude => conv.cwd.clone().or_else(|| claude_project_path(conv)),
        SessionSource::Codex => None,
    }
}

fn claude_project_path(conv: &Conversation) -> Option<PathBuf> {
    if conv.source != SessionSource::Claude {
        return None;
    }

    let encoded_project = conv.path.parent()?.file_name()?.to_str()?;
    Some(decode_project_dir_name_to_path(encoded_project))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;
    use std::ffi::OsStr;

    fn conversation(source: SessionSource, path: PathBuf, cwd: Option<PathBuf>) -> Conversation {
        Conversation {
            path,
            source,
            session_id: "session-123".to_string(),
            timestamp: Local::now(),
            preview: String::new(),
            full_text: String::new(),
            project_name: None,
            cwd,
            message_count: 0,
            model: None,
            total_tokens: 0,
            duration_minutes: None,
            summary: None,
            custom_title: None,
            git_branch: None,
        }
    }

    #[test]
    fn claude_resume_uses_conversation_cwd() {
        let cwd = PathBuf::from("/Users/yogev/repos/app");
        let conv = conversation(
            SessionSource::Claude,
            PathBuf::from("/Users/yogev/.claude/projects/-Users-yogev-repos-other/session.jsonl"),
            Some(cwd.clone()),
        );

        let command = build_resume_command(&conv);

        assert_eq!(command.get_program(), OsStr::new("claude"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![
                OsStr::new("--dangerously-skip-permissions"),
                OsStr::new("--resume"),
                OsStr::new("session-123")
            ]
        );
        assert_eq!(command.get_current_dir(), Some(cwd.as_path()));
    }

    #[test]
    fn claude_resume_falls_back_to_project_directory() {
        let conv = conversation(
            SessionSource::Claude,
            PathBuf::from("/Users/yogev/.claude/projects/-Users-yogev-repos-app/session.jsonl"),
            None,
        );

        let command = build_resume_command(&conv);

        assert_eq!(
            command.get_current_dir(),
            Some(PathBuf::from("/Users/yogev/repos/app").as_path())
        );
    }

    #[test]
    fn codex_resume_keeps_existing_working_directory() {
        let cwd = PathBuf::from("/Users/yogev/repos/app");
        let conv = conversation(
            SessionSource::Codex,
            PathBuf::from("/Users/yogev/.codex/sessions/session.jsonl"),
            Some(cwd.clone()),
        );

        let command = build_resume_command(&conv);

        assert_eq!(command.get_program(), OsStr::new("codex"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![
                OsStr::new("--yolo"),
                OsStr::new("resume"),
                OsStr::new("session-123")
            ]
        );
        assert_eq!(command.get_current_dir(), None);
    }
}
