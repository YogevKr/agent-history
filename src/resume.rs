use crate::codex::{CodexLine, SessionMeta};
use crate::error::{AppError, Result};
use crate::history::{Conversation, SessionSource};
use crate::path::decode_project_dir_name_to_path;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn resume_session(conv: &Conversation) -> Result<()> {
    let err = build_resume_command(conv)?.exec();
    Err(AppError::CliExecutionError(err.to_string()))
}

fn build_resume_command(conv: &Conversation) -> Result<Command> {
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
            let teamcodex = codex_model_provider(&conv.path)?.as_deref() == Some("teamcodex");
            let mut command = Command::new(if teamcodex { "tcx" } else { "codex" });
            if teamcodex {
                // The shared daemon does not inherit tcx's provider settings or
                // proxy-token environment. Keep this resume in its own process.
                command.args(["run", "--", "--no-daemon"]);
            }
            command.args(["--yolo", "resume", &conv.session_id]);
            command
        }
    };

    if let Some(cwd) = resume_cwd(conv) {
        command.current_dir(cwd);
    }

    Ok(command)
}

fn codex_model_provider(path: &Path) -> Result<Option<String>> {
    // Only session metadata is needed. Bound the read even for a damaged file.
    let mut line = String::new();
    BufReader::new(std::fs::File::open(path)?)
        .take(4 * 1024 * 1024)
        .read_line(&mut line)?;
    let record: CodexLine = serde_json::from_str(&line)?;
    if record.line_type != "session_meta" {
        return Ok(None);
    }
    let meta: SessionMeta = serde_json::from_str(record.payload.get())?;
    Ok(meta.model_provider)
}

fn resume_cwd(conv: &Conversation) -> Option<PathBuf> {
    match conv.source {
        SessionSource::Claude => conv.cwd.clone().or_else(|| claude_directory_path(conv)),
        SessionSource::Codex => None,
    }
}

fn claude_directory_path(conv: &Conversation) -> Option<PathBuf> {
    if conv.source != SessionSource::Claude {
        return None;
    }

    let encoded_directory = conv.path.parent()?.file_name()?.to_str()?;
    Some(decode_project_dir_name_to_path(encoded_directory))
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
            directory_name: None,
            cwd,
            message_count: 0,
            model: None,
            total_tokens: 0,
            duration_minutes: None,
            summary: None,
            custom_title: None,
            git_branch: None,
            subagent_name: None,
            hierarchy_root_id: None,
            hierarchy_has_children: false,
            hierarchy_has_next_sibling: false,
            hierarchy_marker: None,
            hierarchy_depth: 0,
            hierarchy_order: 0,
            hierarchy_sort_timestamp: Local::now(),
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

        let command = build_resume_command(&conv).unwrap();

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
    fn claude_resume_falls_back_to_directory() {
        let conv = conversation(
            SessionSource::Claude,
            PathBuf::from("/Users/yogev/.claude/projects/-Users-yogev-repos-app/session.jsonl"),
            None,
        );

        let command = build_resume_command(&conv).unwrap();

        assert_eq!(
            command.get_current_dir(),
            Some(PathBuf::from("/Users/yogev/repos/app").as_path())
        );
    }

    #[test]
    fn teamcodex_resume_uses_proxy_and_bypasses_shared_daemon() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), r#"{"timestamp":"2026-09-26","type":"session_meta","payload":{"id":"session-123","model_provider":"teamcodex"}}
this later line must not be parsed"#).unwrap();
        let conv = conversation(SessionSource::Codex, file.path().to_path_buf(), None);
        let command = build_resume_command(&conv).unwrap();
        assert_eq!(command.get_program(), OsStr::new("tcx"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                "run",
                "--",
                "--no-daemon",
                "--yolo",
                "resume",
                "session-123"
            ]
        );
        assert_eq!(command.get_current_dir(), None);
    }

    #[test]
    fn malformed_codex_metadata_reports_an_error() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "broken metadata").unwrap();
        let conv = conversation(SessionSource::Codex, file.path().to_path_buf(), None);
        assert!(build_resume_command(&conv).is_err());
    }

    #[test]
    fn codex_resume_keeps_existing_working_directory() {
        let cwd = PathBuf::from("/Users/yogev/repos/app");
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), r#"{"timestamp":"2026-09-26","type":"session_meta","payload":{"id":"session-123","model_provider":"openai"}}"#).unwrap();
        let conv = conversation(SessionSource::Codex, file.path().to_path_buf(), Some(cwd));

        let command = build_resume_command(&conv).unwrap();

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
