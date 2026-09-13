#!/bin/sh
# One-step setup for update-agents: build and install the binary as a global
# command, install the bundled catalogue, and register the daily background
# cron runs (10:00, 12:00, 15:00, 18:00).
#
# Cron semantics match "run while the computer is on": a time missed because
# the machine was off or asleep is skipped, not replayed. Re-running this
# script is safe: existing update-agents cron lines are replaced, other
# crontab entries are kept.
set -eu

# Global command: cargo installs to ~/.cargo/bin, which rustup setups
# normally already have on PATH.
cargo install --locked --path .

# Bundled catalogue, at the built-in directory the program reads.
case "${XDG_DATA_HOME:-}" in
  /*) data_home="$XDG_DATA_HOME" ;;
  *)  data_home="$HOME/.local/share" ;;
esac
mkdir -p "$data_home/update-agents/agents.d"
install -m 644 agents.d/*.json "$data_home/update-agents/agents.d/"

# Cron: daily background runs at 10:00, 12:00, 15:00, 18:00. The command
# line sets PATH explicitly because cron's default PATH (/usr/bin:/bin)
# misses version-manager and Homebrew bin dirs, and /bin/sh -c expansion
# resolves $HOME per user. `--bg` detaches, so cron only records the
# startup acknowledgement.
marker="# update-agents scheduled runs"
entry='0 10,12,15,18 * * * PATH="$HOME/.local/bin:$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin:/opt/homebrew/bin" $HOME/.cargo/bin/update-agents --bg'

tmp="$(mktemp)"
crontab -l 2>/dev/null | grep -v "$marker" > "$tmp" || true
printf '%s\n%s\n' "$marker" "$entry" >> "$tmp"
crontab "$tmp"
rm -f "$tmp"

echo "installed: $(command -v update-agents)"
echo "catalogue: $data_home/update-agents/agents.d"
echo "cron:      daily background runs at 10:00, 12:00, 15:00, 18:00"
echo "verify:    crontab -l"
