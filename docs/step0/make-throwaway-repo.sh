#!/usr/bin/env bash
# Makes the Step 0 throwaway repo: a git repo whose path and files hold the
# fake client name, with the Tacita guard hook in its .claude/settings.json.
# Usage: make-throwaway-repo.sh [<dir> [<absolute path to tacita>]]
#   <dir> defaults to ~/tmp/acmewidgets-test and must not exist yet.
#   <tacita> defaults to ./target/release/tacita made absolute.
set -euo pipefail

dir="${1:-$HOME/tmp/acmewidgets-test}"
tacita="${2:-$PWD/target/release/tacita}"

case "$tacita" in
  /*) ;;
  *) echo "make-throwaway-repo: the tacita path must be absolute: $tacita" >&2; exit 1 ;;
esac
case "$tacita" in
  *[\"\\$\`]* | *$'\n'* | *$'\t'*)
    echo "make-throwaway-repo: the tacita path must not contain quotes, backslashes, \$, backticks, tabs or newlines" >&2
    exit 1 ;;
esac
if [ ! -x "$tacita" ]; then
  echo "make-throwaway-repo: not an executable file: $tacita (run cargo build --release first)" >&2
  exit 1
fi
if [ -e "$dir" ] || [ -L "$dir" ]; then
  echo "make-throwaway-repo: $dir already exists; refusing to touch it" >&2
  exit 1
fi

mkdir -p "$(dirname "$dir")"
mkdir "$dir"
cd "$dir"

git init -q
git config user.name step0
git config user.email step0@example.invalid

printf '# AcmeWidgets portal\n\nInternal tool for acmewidgets. Contact: Jeanne Fictive.\n' > README.md
printf 'Meeting notes for acmewidgets.\n' > notes.md

mkdir .claude
# The tacita path is quoted inside the command, so a path with spaces works.
printf '{"hooks":{"PreToolUse":[{"matcher":"*","hooks":[{"type":"command","command":"\\"%s\\" hook pre-tool-use","timeout":10,"onFailure":"block"}]}]}}\n' \
  "$tacita" > .claude/settings.json

git add README.md notes.md .claude/settings.json
git -c commit.gpgsign=false commit -q -m "Initial commit"

echo "Created $dir"
echo "Hook command: \"$tacita\" hook pre-tool-use"
