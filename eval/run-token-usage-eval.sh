#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

prompt_path="eval/token-usage-prompt.txt"
jsonl_path="eval/token-usage-run.jsonl"
final_path="eval/token-usage-final.txt"

rm -f "$jsonl_path" "$final_path"

codex exec \
  --skip-git-repo-check \
  --json \
  -o "$final_path" \
  "$(cat "$prompt_path")" \
  > "$jsonl_path"

echo "JSONL: $repo_root/$jsonl_path"
echo "Final: $repo_root/$final_path"
