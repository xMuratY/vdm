#!/bin/sh
# Usage: attach-to-release.sh <tag> <file>...
# Uploads files to an existing GitHub release (replacing same-named ones) and
# lists their SHA-256 in the table that ends its notes. Needs GH_TOKEN.
set -eu
tag=$1
shift
gh release upload "$tag" "$@" --clobber

notes=$(mktemp)
gh release view "$tag" --json body --jq .body >"$notes"
rows=""
for file in "$@"; do
  rows="$rows$(basename "$file") $(sha256sum "$file" | cut -d' ' -f1)
"
done
python3 - "$notes" "$rows" <<'EOF'
import sys

path, rows = sys.argv[1], sys.argv[2]
lines = open(path, encoding="utf-8").read().rstrip("\n").split("\n")
for row in filter(None, rows.split("\n")):
    name, digest = row.split(" ")
    # Replace an earlier row for the same file; otherwise add one.
    lines = [l for l in lines if not l.startswith(f"| {name} |")]
    lines.append(f"| {name} | `{digest}` |")
open(path, "w", encoding="utf-8").write("\n".join(lines) + "\n")
EOF
gh release edit "$tag" --notes-file "$notes"
