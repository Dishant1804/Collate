#!/bin/zsh
# Writes a release's notes to stdout: the commits since the tag before <tag> (or every
# commit, for the first release), then how to install the downloads.
#
#   scripts/release-notes.sh v1.0.0
set -euo pipefail

(( $# == 1 )) || { echo "usage: scripts/release-notes.sh <tag>" >&2; exit 2; }
tag=$1
cd "$(dirname "$0")/.."
version=${tag#v}
previous=$(git describe --tags --abbrev=0 "$tag^" 2>/dev/null || true)
repo=$(git remote get-url origin | sed -E 's#(git@github.com:|https://github.com/)##; s#\.git$##')

if [[ -n $previous ]]; then
  echo "## Changes since $previous"
  range=$previous..$tag
else
  echo "## Changes"
  range=$tag
fi
echo
git log --no-merges --reverse --format='- %s (%h)' "$range"
echo
cat <<NOTES
## Install

Install or update with \`npx collate-app@latest\`.

\`Collate-$version-macos-universal.zip\` holds the app, for Apple silicon and Intel Macs,
macOS 13 or later; it needs \`git\` on your \`PATH\`. Unzip it and move Collate to
Applications. Collate is signed ad hoc, not by an Apple Developer ID, so macOS blocks the
first launch of a downloaded copy. Clear the quarantine once:

\`\`\`sh
xattr -dr com.apple.quarantine /Applications/Collate.app
\`\`\`

The \`collate-$version-macos-universal.tar.gz\` download holds the bare binary for a
terminal: \`collate <repository>\`. \`SHA256SUMS\` lists every download's checksum.
NOTES
[[ -n $previous ]] && echo "\n**Full changelog:** https://github.com/$repo/compare/$previous...$tag"
true
