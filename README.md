# agent-history

Unified search and viewer for [Claude Code](https://docs.anthropic.com/en/docs/claude-code) and [Codex CLI](https://github.com/openai/codex) session history.

Browse, search, and review your AI coding sessions from one place — with syntax-highlighted code blocks, a ledger-style conversation layout, and an interactive TUI picker.

## Features

- **Unified search** across Claude Code and Codex CLI sessions
- **Interactive TUI** — fzf-style session picker with in-app pager
- **Syntax highlighting** — code blocks rendered with [syntect](https://github.com/trishume/syntect)
- **Ledger layout** — clean `You │` / `Claude │` column format
- **Smart tool display** — tool calls show key arguments (file paths, commands, patterns)
- **Text wrapping** — long paragraphs wrap to terminal width
- **Dark/light themes** — auto-detected from terminal background
- **Resume sessions** — jump back into a session in its original CLI

## Install

```
cargo install --path .
```

Or build from source:

```
cargo build --release
# binary at target/release/agent-history
```

## Usage

```
# Interactive picker (default when run in a terminal)
agent-history

# Search sessions
agent-history "auth flow"

# Filter by source, project, or time
agent-history --source claude --project my-app --since 7d

# Show a specific session (non-interactive)
agent-history --show <session-id>

# Resume a session in its CLI
agent-history --resume <session-id>

# Only sessions from current directory
agent-history --local
```

### TUI Keys

| Key | Action |
|-----|--------|
| `j`/`k`, `Up`/`Down` | Navigate |
| `Enter` | Open session |
| `Space`, `PgDn` | Page down |
| `PgUp` | Page up |
| `g`/`G` | Top / bottom |
| `r` | Refresh (re-read session file) |
| `q`, `Esc` | Back / quit |
| Type anything | Filter sessions |

## How It Works

agent-history reads the JSONL session files that Claude Code and Codex CLI write to disk:

- **Claude Code**: `~/.claude/projects/*/sessions/*.jsonl`
- **Codex CLI**: `~/.codex/sessions/*.jsonl`

Sessions are loaded in parallel, deduplicated, and sorted by timestamp. The viewer parses markdown with [pulldown-cmark](https://github.com/raphlinus/pulldown-cmark), highlights code with syntect, and renders everything with raw [crossterm](https://github.com/crossterm-rs/crossterm) calls.

## Acknowledgements

The rendering approach — syntect syntax highlighting, ledger-style columns, terminal theme detection — is inspired by [claude-history](https://github.com/raine/claude-history) by [@raine](https://github.com/raine).

## License

MIT
