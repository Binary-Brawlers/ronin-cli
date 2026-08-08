#!/usr/bin/env bash

set -euo pipefail

REPOSITORY="Binary-Brawlers/ronin-cli"
WORKFLOW="release.yml"

usage() {
  cat <<'EOF'
Usage: ./scripts/release.sh [--yes] <version>

Creates and merges a version-only PR, tags its merge commit, waits for CI to
build every platform artifact, and verifies the GitHub Release.

Re-run the same command after an interruption to resume an existing tag.
EOF
}

assume_yes=false
if [[ "${1:-}" == "--yes" ]]; then
  assume_yes=true
  shift
fi
if [[ $# -ne 1 || ! "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  usage
  exit 2
fi

version="$1"
tag="ronin-v${version}"
branch="release/${tag}"

for command_name in cargo gh git perl; do
  command -v "$command_name" >/dev/null 2>&1 || {
    echo "Required command not found: $command_name" >&2
    exit 1
  }
done

cd "$(git rev-parse --show-toplevel)"
if [[ "$(gh repo view --json nameWithOwner --jq .nameWithOwner)" != "$REPOSITORY" ]]; then
  echo "Run this script from the $REPOSITORY checkout." >&2
  exit 1
fi
gh auth status >/dev/null
git fetch origin main --tags

current_version="$(perl -ne 'if (/^name = "ronin-cli"$/) { $found=1 } elsif ($found && /^version = "([^"]+)"$/) { print $1; exit }' apps/cli/Cargo.toml)"
remote_tag_commit="$(git ls-remote origin "refs/tags/$tag^{}" | awk '{print $1}')"
if [[ -z "$remote_tag_commit" ]]; then
  remote_tag_commit="$(git ls-remote origin "refs/tags/$tag" | awk '{print $1}')"
fi

if [[ -n "$remote_tag_commit" ]]; then
  [[ "$current_version" == "$version" ]] || {
    echo "$tag exists, but Cargo.toml contains $current_version." >&2
    exit 1
  }
  echo "Found existing $tag; resuming its release."
  release_commit="$remote_tag_commit"
else
  [[ "$(git branch --show-current)" == "main" ]] || {
    echo "Start a new release from main." >&2
    exit 1
  }
  [[ -z "$(git status --porcelain)" ]] || {
    echo "The worktree must be clean." >&2
    exit 1
  }
  [[ "$(git rev-parse HEAD)" == "$(git rev-parse origin/main)" ]] || {
    echo "Local main must match origin/main." >&2
    exit 1
  }

  IFS=. read -r current_major current_minor current_patch <<<"$current_version"
  IFS=. read -r next_major next_minor next_patch <<<"$version"
  if (( 10#$next_major < 10#$current_major ||
        (10#$next_major == 10#$current_major && 10#$next_minor < 10#$current_minor) ||
        (10#$next_major == 10#$current_major && 10#$next_minor == 10#$current_minor && 10#$next_patch <= 10#$current_patch) )); then
    echo "$version must be greater than $current_version." >&2
    exit 1
  fi

  echo "Release Ronin CLI $current_version -> $version"
  if [[ "$assume_yes" != true ]]; then
    read -r -p "Create, merge, tag, and publish this release? [y/N] " answer
    [[ "$answer" =~ ^[Yy]$ ]] || exit 0
  fi

  git switch -c "$branch"
  RONIN_RELEASE_VERSION="$version" perl -0pi -e 's/(\[package\]\nname = "ronin-cli"\nversion = ")[^"]+("\n)/$1$ENV{RONIN_RELEASE_VERSION}$2/' apps/cli/Cargo.toml
  cargo check --workspace --all-targets
  cargo fmt --all -- --check
  git diff --check

  lock_version="$(perl -ne 'if (/^name = "ronin-cli"$/) { $found=1 } elsif ($found && /^version = "([^"]+)"$/) { print $1; exit }' Cargo.lock)"
  [[ "$lock_version" == "$version" ]] || {
    echo "Cargo.lock was not updated to $version." >&2
    exit 1
  }

  git add apps/cli/Cargo.toml Cargo.lock
  git commit -m "chore(release): Ronin CLI v${version}"
  git push -u origin "$branch"
  pr_url="$(gh pr create --base main --head "$branch" \
    --title "chore(release): Ronin CLI v${version}" \
    --body "Version-only release PR for \`$tag\`. Merged feature and fix PRs will be included in GitHub's generated release notes.")"
  pr_number="$(gh pr view "$pr_url" --json number --jq .number)"
  gh pr checks "$pr_number" --watch --fail-fast
  gh pr merge "$pr_number" --squash --delete-branch
  release_commit="$(gh pr view "$pr_number" --json mergeCommit --jq .mergeCommit.oid)"
  git switch main
  git pull --ff-only origin main
  git tag -a "$tag" "$release_commit" -m "Ronin CLI v${version}"
  git push origin "$tag"
fi

if ! gh release view "$tag" >/dev/null 2>&1; then
  echo "Waiting for the release workflow..."
  run_id=""
  for _ in $(seq 1 60); do
    run_id="$(gh run list --workflow "$WORKFLOW" --event push --limit 30 \
      --json databaseId,headSha \
      --jq ".[] | select(.headSha == \"$release_commit\") | .databaseId" | head -n 1)"
    [[ -n "$run_id" ]] && break
    sleep 2
  done
  [[ -n "$run_id" ]] || {
    echo "Could not find the release workflow for $tag." >&2
    exit 1
  }
  gh run watch "$run_id" --exit-status
fi

release_url="$(gh release view "$tag" --json url --jq .url)"
echo "Ronin CLI v${version} is published: $release_url"
