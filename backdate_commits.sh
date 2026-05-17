#!/usr/bin/env bash
# Script to backdate git commits from May 10, 2026 to today.
# Useful for private repositories.

set -euo pipefail

# Configuration
START_DATE="2026-05-10"
END_DATE=$(date +%Y-%m-%d)
NUM_COMMITS=${1:-50}

# 1. Clean workspace check
if ! git diff-index --quiet HEAD --; then
    echo "Error: Your working directory is not clean. Please commit or stash your changes before running this script."
    exit 1
fi

# 2. Get current branch name
original_branch=$(git symbolic-ref --short HEAD)
echo "Current branch: $original_branch"

# 3. Get commit list (oldest to newest)
commits=($(git log -n "$NUM_COMMITS" --reverse --format="%H"))
actual_commits_count=${#commits[@]}

if [ "$actual_commits_count" -eq 0 ]; then
    echo "Error: No commits found on branch $original_branch."
    exit 1
fi

echo "Found $actual_commits_count commits to backdate."

# 4. Convert start/end dates to unix timestamps
if date -d "today" >/dev/null 2>&1; then
    # GNU date (Linux / WSL / Git Bash)
    START_TS=$(date -d "$START_DATE 08:00:00" +%s)
    END_TS=$(date -d "$END_DATE 20:00:00" +%s)
else
    # BSD/macOS date
    START_TS=$(date -j -f "%Y-%m-%d %H:%M:%S" "$START_DATE 08:00:00" +%s)
    END_TS=$(date -j -f "%Y-%m-%d %H:%M:%S" "$END_DATE 20:00:00" +%s)
fi

if [ "$actual_commits_count" -le 1 ]; then
    STEP=0
else
    STEP=$(( (END_TS - START_TS) / (actual_commits_count - 1) ))
fi

# Progress bar function
draw_progress() {
    local current=$1
    local total=$2
    local percent=$(( current * 100 / total ))
    local filled=$(( percent / 2 ))
    local empty=$(( 50 - filled ))
    local bar=""
    for ((j=0; j<filled; j++)); do bar="${bar}#"; done
    for ((j=0; j<empty; j++)); do bar="${bar}-"; done
    printf "\rBackdating commits: [%s] %d/%d (%d%%)" "$bar" "$current" "$total" "$percent"
}

# 5. History rewriting
first_commit_hash=${commits[0]}
parent_commit=$(git rev-parse "$first_commit_hash^" 2>/dev/null || echo "")

echo "Starting backdating rewrite..."
if [ -n "$parent_commit" ]; then
    git checkout -q -b temp_backdate "$parent_commit"
else
    git checkout -q --orphan temp_backdate
    git rm -rf . -q
fi

# Disable GPG signing for rewriting speed
export GIT_COMMIT_GPGSIGN=false

for ((i=0; i<actual_commits_count; i++)); do
    commit_hash=${commits[i]}
    base_ts=$(( START_TS + i * STEP ))

    if date -d "@$base_ts" >/dev/null 2>&1; then
        base_date=$(date -d "@$base_ts" +%Y-%m-%d)
    else
        base_date=$(date -r "$base_ts" +%Y-%m-%d)
    fi

    # Random time variation (8am - 8pm)
    rand_hour=$(( 8 + RANDOM % 12 ))
    rand_min=$(( RANDOM % 60 ))
    rand_sec=$(( RANDOM % 60 ))
    final_date=$(printf "%s %02d:%02d:%02d" "$base_date" "$rand_hour" "$rand_min" "$rand_sec")

    if ! git cherry-pick --no-commit "$commit_hash" >/dev/null 2>&1; then
        echo "Error: Cherry-pick failed for commit $commit_hash. Aborting."
        git cherry-pick --abort
        git checkout -q "$original_branch"
        git branch -D temp_backdate >/dev/null 2>&1
        exit 1
    fi

    # Commit with backdated dates
    GIT_COMMITTER_DATE="$final_date" git commit -C "$commit_hash" --date="$final_date" --no-gpg-sign -q
    
    draw_progress "$((i+1))" "$actual_commits_count"
done
echo ""

# 6. Apply rewritten history to original branch
git checkout -q "$original_branch"
git reset --hard temp_backdate -q
git branch -D temp_backdate -q

echo "Backdating complete! The last $actual_commits_count commits have been rewritten."

# 7. Ask before pushing
echo -n "Would you like to force push the backdated commits to origin with --force-with-lease? (y/N): "
read -r response
if [[ "$response" =~ ^[Yy]$ ]]; then
    echo "Pushing rewritten history to origin..."
    git push origin "$original_branch" --force-with-lease
else
    echo "Push skipped. You can manually force push when ready."
fi
