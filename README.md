# amty

Lightweight scriptable AI chat & agent desktop client built with Rust and eframe (egui).

## Features

- **Multi-Provider Support**: Anthropic (with prompt caching), OpenAI-compatible APIs, and local LLM runners.
- **Dynamic Skills & Agent Workflows**: Re-indexes tools and prompt skills dynamically from standard paths (`~/.claude/skills`, `~/.agents/skills`, etc.).
- **Auto-Compaction**: Automatic token estimation and session history condensation to fit within context budgets.
- **MCP Integration**: Model Context Protocol client support for dynamic tool invocation.
- **Modern Desktop GUI**: Fast, native cross-platform UI built on eframe / egui with Tokyo Night dark aesthetics and drag-and-drop file support.

## Building

```bash
cargo build --release
```
