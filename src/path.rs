//! Path encoding/decoding utilities for Claude project directories.

use std::path::{Path, PathBuf};

/// Convert the current working directory into Claude's project directory name.
pub fn convert_path_to_project_dir_name(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Format a path into a short display name.
///
/// For worktree paths, returns `project/worktree`. For regular paths, returns the folder name.
pub fn format_short_name_from_path(path: &Path) -> String {
    let path_str = path.to_string_lossy();

    if let Some(wt_pos) = path_str
        .find("__worktrees/")
        .or_else(|| path_str.find("/.worktrees/"))
    {
        let is_hidden = path_str[wt_pos..].starts_with("/.");
        let separator_len = if is_hidden {
            "/.worktrees/".len()
        } else {
            "__worktrees/".len()
        };

        let before = &path_str[..wt_pos];
        let main_project = Path::new(before)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        let after = &path_str[wt_pos + separator_len..];
        let worktree = after.split('/').next().unwrap_or("");

        if !main_project.is_empty() && !worktree.is_empty() {
            return format!("{}/{}", main_project, worktree);
        }
    }

    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path_str.into_owned())
}

/// Decode a project directory name back to a path (simple heuristic fallback).
pub fn decode_project_dir_name_to_path(encoded: &str) -> PathBuf {
    PathBuf::from(decode_with_double_dash_as(encoded, "__"))
}

/// Decode with a specific replacement for double dashes
fn decode_with_double_dash_as(encoded: &str, double_dash_replacement: &str) -> String {
    let mut result = String::with_capacity(encoded.len());
    let mut chars = encoded.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '-' {
            let mut count = 1;
            while chars.peek() == Some(&'-') {
                chars.next();
                count += 1;
            }

            match count {
                1 => result.push('/'),
                2 => result.push_str(double_dash_replacement),
                n => {
                    result.push('/');
                    for _ in 0..((n - 1) / 2) {
                        result.push_str(double_dash_replacement);
                    }
                    if (n - 1) % 2 == 1 {
                        result.push('/');
                    }
                }
            }
        } else {
            result.push(c);
        }
    }

    result
}

/// Decode a project directory name back to a readable path (for display purposes).
pub fn decode_project_dir_name(encoded: &str) -> String {
    let mut result = String::with_capacity(encoded.len());
    let mut chars = encoded.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '-' {
            let mut count = 1;
            while chars.peek() == Some(&'-') {
                chars.next();
                count += 1;
            }

            if count % 2 == 1 {
                result.push('/');
                for _ in 0..(count - 1) {
                    result.push('_');
                }
            } else {
                for _ in 0..count {
                    result.push('_');
                }
            }
        } else {
            result.push(c);
        }
    }

    result
}
