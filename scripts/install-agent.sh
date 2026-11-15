#!/bin/zsh
# Lets coding agents drive Collate: installs the `collate` skill for Claude Code and registers
# `collate --mcp` as an MCP server for it. Run after scripts/install.sh, which installs the
# `collate` command the server runs.
#
#   scripts/install-agent.sh            install the skill, register the server (user scope)
#   scripts/install-agent.sh --remove   take both away
set -euo pipefail

cd "$(dirname "$0")/.."
bin=$HOME/.local/bin/collate
skills=$HOME/.claude/skills

if [[ ${1:-} == --remove ]]; then
  rm -rf "$skills/collate"
  command -v claude >/dev/null && claude mcp remove --scope user collate >/dev/null 2>&1 || true
  echo "Removed the collate skill and MCP server"
  exit 0
fi

[[ -x $bin ]] || { echo "install-agent: $bin is missing: run scripts/install.sh first" >&2; exit 1; }

mkdir -p "$skills"
rm -rf "$skills/collate"
cp -R skills/collate "$skills/collate"
echo "Installed the skill in $skills/collate"

if command -v claude >/dev/null; then
  # Replaced, not added twice, when run again.
  claude mcp remove --scope user collate >/dev/null 2>&1 || true
  claude mcp add --scope user collate -- "$bin" --mcp
  echo "Registered the collate MCP server with Claude Code"
else
  echo "Claude Code isn't installed. For another MCP client, add:"
fi
cat <<JSON
  "collate": { "command": "$bin", "args": ["--mcp"] }
JSON
