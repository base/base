#!/bin/bash
# ~/.dmux/hooks/ — hooks para integração com Safe-Core

# on-worktree-create.sh
mkdir -p "$1/.safe-core"
export SAFE_CORE_DB="$1/.safe-core/state.db"

# pre-merge.sh
RESULT=$(safe-core-mcp-client call enforce_action \
    --action "merge_branch" \
    --context "{\"branch\": \"$2\", \"worktree\": \"$1\"}")

if echo "$RESULT" | grep -q '"verdict":"Block"'; then
    echo "❌ Merge bloqueado: $(echo "$RESULT" | jq -r '.reason')"
    exit 1
fi

# post-merge.sh
safe-core-mcp-client call audit_log \
    --event merge_completed \
    --branch "$2" \
    --worktree "$1"
