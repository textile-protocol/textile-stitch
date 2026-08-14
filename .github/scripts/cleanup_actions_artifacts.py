#!/usr/bin/env python3
# Keep in sync with the monorepo .github/scripts/cleanup_actions_artifacts.py
"""Delete GitHub Actions artifacts that are only leftover scratch storage.

Workflow artifacts here are job-to-job transfers (sync tarballs, cargo-dist
scratch zips). Once the run finishes they are unused; GitHub still keeps them
for 90 days by default, which is what filled the org Actions storage quota.

Never deletes artifacts newer than --older-than-hours, so a live workflow
cannot lose the zip it is about to download-artifact.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any


def parse_github_time(value: str) -> datetime:
    if value.endswith("Z"):
        value = value[:-1] + "+00:00"
    parsed = datetime.fromisoformat(value)
    if parsed.tzinfo is None:
        return parsed.replace(tzinfo=timezone.utc)
    return parsed.astimezone(timezone.utc)


def is_old_enough(created_at: str, now: datetime, older_than: timedelta) -> bool:
    return now - parse_github_time(created_at) >= older_than


def artifact_size_bytes(artifact: dict[str, Any]) -> int:
    return int(artifact.get("size_in_bytes") or 0)


def select_deletable(
    artifacts: list[dict[str, Any]],
    now: datetime,
    older_than: timedelta,
) -> list[dict[str, Any]]:
    return [
        artifact
        for artifact in artifacts
        if not artifact.get("expired")
        and is_old_enough(str(artifact["created_at"]), now, older_than)
    ]


def format_mb(num_bytes: int) -> str:
    return f"{num_bytes / (1024 * 1024):.1f} MB"


def gh_json(args: list[str]) -> Any:
    result = subprocess.run(
        ["gh", "api", *args],
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(result.stdout) if result.stdout.strip() else None


def list_artifacts(repo: str) -> list[dict[str, Any]]:
    artifacts: list[dict[str, Any]] = []
    page = 1
    while True:
        data = gh_json(
            [f"repos/{repo}/actions/artifacts?per_page=100&page={page}"]
        )
        batch = data.get("artifacts") or []
        artifacts.extend(batch)
        if len(batch) < 100:
            return artifacts
        page += 1


def delete_artifact(repo: str, artifact_id: int) -> None:
    subprocess.run(
        ["gh", "api", "-X", "DELETE", f"repos/{repo}/actions/artifacts/{artifact_id}"],
        check=True,
        capture_output=True,
        text=True,
    )


def default_workflow_dirs(repo_root: Path) -> list[Path]:
    dirs = [repo_root / ".github" / "workflows"]
    stitch = repo_root / "packages" / "stitch-bot" / ".github" / "workflows"
    if stitch.is_dir():
        dirs.append(stitch)
    return [path for path in dirs if path.is_dir()]


def missing_retention_days(workflow_text: str) -> list[int]:
    """Line numbers of upload-artifact steps that omit retention-days."""
    lines = workflow_text.splitlines()
    missing: list[int] = []
    i = 0
    while i < len(lines):
        stripped = lines[i].lstrip()
        if "uses:" in stripped and "upload-artifact" in stripped:
            uses_line = i + 1
            indent = len(lines[i]) - len(stripped)
            j = i + 1
            block: list[str] = []
            while j < len(lines):
                line = lines[j]
                if not line.strip() or line.lstrip().startswith("#"):
                    j += 1
                    continue
                line_indent = len(line) - len(line.lstrip())
                if line_indent <= indent and line.lstrip().startswith("- "):
                    break
                block.append(line)
                j += 1
            if not any("retention-days:" in item for item in block):
                missing.append(uses_line)
            i = j
            continue
        i += 1
    return missing


def lint_workflows(workflow_dirs: list[Path]) -> int:
    failures = 0
    for directory in workflow_dirs:
        for path in sorted(directory.glob("*.yml")) + sorted(directory.glob("*.yaml")):
            missing = missing_retention_days(path.read_text())
            if not missing:
                continue
            failures += 1
            joined = ", ".join(str(line) for line in missing)
            print(f"{path}: upload-artifact missing retention-days on line(s) {joined}")
    if failures == 0:
        print("All upload-artifact steps set retention-days.")
    return failures


def cleanup(repo: str, older_than: timedelta, dry_run: bool, now: datetime) -> int:
    artifacts = list_artifacts(repo)
    deletable = select_deletable(artifacts, now, older_than)
    total = sum(artifact_size_bytes(a) for a in artifacts)
    reclaim = sum(artifact_size_bytes(a) for a in deletable)
    print(
        f"{repo}: {len(artifacts)} artifacts ({format_mb(total)}), "
        f"{len(deletable)} older than {int(older_than.total_seconds() // 3600)}h "
        f"({format_mb(reclaim)})"
    )
    if dry_run:
        for artifact in deletable[:20]:
            print(
                f"  would delete {artifact['name']} "
                f"{format_mb(artifact_size_bytes(artifact))} "
                f"created {artifact['created_at']}"
            )
        if len(deletable) > 20:
            print(f"  ... and {len(deletable) - 20} more")
        print("dry-run: nothing deleted")
        return 0

    deleted = 0
    for artifact in deletable:
        delete_artifact(repo, int(artifact["id"]))
        deleted += 1
        if deleted % 50 == 0:
            print(f"  deleted {deleted}/{len(deletable)}")
    print(f"deleted {deleted} artifacts, reclaimed ~{format_mb(reclaim)}")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repo",
        default="",
        help="owner/name (default: $GITHUB_REPOSITORY)",
    )
    parser.add_argument("--older-than-hours", type=int, default=6)
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument(
        "--lint-workflows",
        action="store_true",
        help="Fail if any upload-artifact step omits retention-days",
    )
    parser.add_argument(
        "--repo-root",
        default=".",
        help="Repo root for --lint-workflows",
    )
    args = parser.parse_args(argv)

    if args.lint_workflows:
        root = Path(args.repo_root).resolve()
        return lint_workflows(default_workflow_dirs(root))

    repo = args.repo.strip()
    if not repo:
        repo = os.environ.get("GITHUB_REPOSITORY", "").strip()
    if not repo:
        print("Pass --repo owner/name or set GITHUB_REPOSITORY", file=sys.stderr)
        return 2

    return cleanup(
        repo=repo,
        older_than=timedelta(hours=args.older_than_hours),
        dry_run=args.dry_run,
        now=datetime.now(timezone.utc),
    )


if __name__ == "__main__":
    sys.exit(main())
