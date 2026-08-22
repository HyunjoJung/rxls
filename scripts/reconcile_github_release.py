#!/usr/bin/env python3
"""Reconcile one GitHub Release against an exact local asset directory."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import ssl
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import Any


API_VERSION = "2022-11-28"
MAX_JSON_BYTES = 8 * 1024 * 1024
REPOSITORY_RE = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+\Z")
SHA_RE = re.compile(r"[0-9a-f]{40}\Z")
DIGEST_RE = re.compile(r"sha256:[0-9a-f]{64}\Z")


class ReleaseReconciliationError(RuntimeError):
    """Raised when local or hosted release state is not safe to reconcile."""


@dataclass(frozen=True)
class LocalAsset:
    """Immutable local metadata used for upload and hosted verification."""

    name: str
    path: Path
    size: int
    digest: str


def _is_positive_int(value: object) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value > 0


def _reject_duplicate_json_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ReleaseReconciliationError(
                f"GitHub returned duplicate JSON object key {key!r}"
            )
        result[key] = value
    return result


def _load_json(payload: bytes, context: str) -> Any:
    try:
        return json.loads(
            payload.decode("utf-8"), object_pairs_hook=_reject_duplicate_json_keys
        )
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ReleaseReconciliationError(
            f"{context} was not valid duplicate-free UTF-8 JSON"
        ) from error


def _safe_asset_name(name: object, context: str) -> str:
    if not isinstance(name, str) or not name:
        raise ReleaseReconciliationError(f"{context} has no usable asset name")
    if name in {".", ".."} or Path(name).name != name:
        raise ReleaseReconciliationError(f"{context} has an unsafe asset name")
    if any(ord(character) < 32 or ord(character) == 127 for character in name):
        raise ReleaseReconciliationError(f"{context} asset name contains control bytes")
    return name


def inventory_local_assets(
    directory: Path, expected_files: int
) -> dict[str, LocalAsset]:
    """Hash an exact, flat, symlink-free local release directory."""

    if not _is_positive_int(expected_files):
        raise ReleaseReconciliationError("expected file count must be positive")
    if not directory.is_dir() or directory.is_symlink():
        raise ReleaseReconciliationError(f"release directory is unusable: {directory}")

    assets: dict[str, LocalAsset] = {}
    entries = sorted(directory.iterdir(), key=lambda entry: entry.name.encode("utf-8"))
    if len(entries) != expected_files:
        raise ReleaseReconciliationError(
            f"expected exactly {expected_files} local release files, found {len(entries)}"
        )
    for path in entries:
        name = _safe_asset_name(path.name, f"local entry {path}")
        if path.is_symlink() or not path.is_file():
            raise ReleaseReconciliationError(
                f"local release entry must be a regular non-symlink file: {path}"
            )
        payload = path.read_bytes()
        if not payload:
            raise ReleaseReconciliationError(f"local release asset is empty: {path}")
        assets[name] = LocalAsset(
            name=name,
            path=path,
            size=len(payload),
            digest=f"sha256:{hashlib.sha256(payload).hexdigest()}",
        )
    if len(assets) != expected_files:
        raise ReleaseReconciliationError("local release asset names are not unique")
    return assets


def validate_release_identity(
    release: object,
    *,
    tag: str,
    expected_release_id: int | None = None,
    require_published: bool = False,
) -> int:
    """Validate release identity and, optionally, final publication state."""

    if not isinstance(release, dict):
        raise ReleaseReconciliationError("GitHub release metadata is not an object")
    release_id = release.get("id")
    if not _is_positive_int(release_id):
        raise ReleaseReconciliationError("GitHub release has no usable numeric ID")
    if expected_release_id is not None and release_id != expected_release_id:
        raise ReleaseReconciliationError(
            "GitHub release ID changed during reconciliation"
        )
    if release.get("tag_name") != tag:
        raise ReleaseReconciliationError(
            "GitHub release tag does not match the requested tag"
        )
    if require_published:
        if release.get("draft") is not False:
            raise ReleaseReconciliationError("GitHub release is still a draft")
        if release.get("prerelease") is not False:
            raise ReleaseReconciliationError("GitHub release is still a prerelease")
        published_at = release.get("published_at")
        if not isinstance(published_at, str) or not published_at:
            raise ReleaseReconciliationError(
                "GitHub release has no publishedAt timestamp"
            )
        try:
            parsed = datetime.fromisoformat(published_at.replace("Z", "+00:00"))
        except ValueError as error:
            raise ReleaseReconciliationError(
                "GitHub release publishedAt timestamp is invalid"
            ) from error
        if parsed.tzinfo is None:
            raise ReleaseReconciliationError(
                "GitHub release publishedAt timestamp has no timezone"
            )
    return release_id


def validate_reconcilable_remote_assets(assets: object) -> list[int]:
    """Validate all current assets before returning IDs safe to delete."""

    if not isinstance(assets, list):
        raise ReleaseReconciliationError("GitHub release assets are not an array")
    ids: list[int] = []
    seen_ids: set[int] = set()
    for index, raw in enumerate(assets):
        if not isinstance(raw, dict):
            raise ReleaseReconciliationError(
                f"GitHub release asset {index} is not an object"
            )
        asset_id = raw.get("id")
        if not _is_positive_int(asset_id):
            raise ReleaseReconciliationError(
                f"GitHub release asset {index} has no usable numeric ID"
            )
        if asset_id in seen_ids:
            raise ReleaseReconciliationError("GitHub release asset IDs are duplicated")
        _safe_asset_name(raw.get("name"), f"GitHub release asset {index}")
        seen_ids.add(asset_id)
        ids.append(asset_id)
    return ids


def validate_published_assets(
    remote_assets: object, local_assets: dict[str, LocalAsset]
) -> None:
    """Require an exact hosted name, state, byte-size, and SHA-256 match."""

    if not isinstance(remote_assets, list):
        raise ReleaseReconciliationError("GitHub release assets are not an array")
    if len(remote_assets) != len(local_assets):
        raise ReleaseReconciliationError(
            "GitHub release asset count does not match the local inventory"
        )
    seen_names: set[str] = set()
    seen_ids: set[int] = set()
    for index, raw in enumerate(remote_assets):
        if not isinstance(raw, dict):
            raise ReleaseReconciliationError(
                f"GitHub release asset {index} is not an object"
            )
        asset_id = raw.get("id")
        if not _is_positive_int(asset_id) or asset_id in seen_ids:
            raise ReleaseReconciliationError(
                "GitHub release asset IDs are missing, invalid, or duplicated"
            )
        name = _safe_asset_name(raw.get("name"), f"GitHub release asset {index}")
        if name in seen_names:
            raise ReleaseReconciliationError(
                f"GitHub release contains duplicate asset name {name!r}"
            )
        local = local_assets.get(name)
        if local is None:
            raise ReleaseReconciliationError(
                f"GitHub release contains unexpected asset {name!r}"
            )
        if raw.get("state") != "uploaded":
            raise ReleaseReconciliationError(
                f"GitHub release asset {name!r} is not in uploaded state"
            )
        size = raw.get("size")
        if not isinstance(size, int) or isinstance(size, bool) or size != local.size:
            raise ReleaseReconciliationError(
                f"GitHub release asset {name!r} byte size differs from local"
            )
        digest = raw.get("digest")
        if not isinstance(digest, str) or DIGEST_RE.fullmatch(digest) is None:
            raise ReleaseReconciliationError(
                f"GitHub release asset {name!r} has no canonical SHA-256 digest"
            )
        if digest != local.digest:
            raise ReleaseReconciliationError(
                f"GitHub release asset {name!r} SHA-256 differs from local"
            )
        seen_ids.add(asset_id)
        seen_names.add(name)
    if seen_names != set(local_assets):
        raise ReleaseReconciliationError(
            "GitHub release asset-name set does not match the local inventory"
        )


class GitHubReleaseClient:
    """Small GitHub REST client restricted to one repository."""

    def __init__(
        self,
        *,
        repository: str,
        token: str,
        api_url: str = "https://api.github.com",
        uploads_url: str = "https://uploads.github.com",
    ) -> None:
        if REPOSITORY_RE.fullmatch(repository) is None:
            raise ReleaseReconciliationError("repository must be an owner/name pair")
        if not token:
            raise ReleaseReconciliationError("GitHub token is empty")
        self.repository = repository
        self.token = token
        self.api_url = self._validated_origin(api_url, "GitHub API URL")
        self.uploads_url = self._validated_origin(uploads_url, "GitHub uploads URL")
        self.context = ssl.create_default_context()

    @staticmethod
    def _validated_origin(value: str, context: str) -> str:
        parsed = urllib.parse.urlsplit(value)
        if (
            parsed.scheme != "https"
            or not parsed.hostname
            or parsed.username is not None
            or parsed.password is not None
            or parsed.query
            or parsed.fragment
        ):
            raise ReleaseReconciliationError(f"{context} must be a plain HTTPS origin")
        path = parsed.path.rstrip("/")
        return urllib.parse.urlunsplit((parsed.scheme, parsed.netloc, path, "", ""))

    def _request_json(
        self,
        method: str,
        url: str,
        *,
        body: bytes | None = None,
        content_type: str | None = None,
        allow_not_found: bool = False,
        expect_no_content: bool = False,
    ) -> Any:
        headers = {
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {self.token}",
            "User-Agent": "rxls-release-reconciler",
            "X-GitHub-Api-Version": API_VERSION,
        }
        if content_type is not None:
            headers["Content-Type"] = content_type
        request = urllib.request.Request(url, data=body, headers=headers, method=method)
        try:
            with urllib.request.urlopen(
                request, context=self.context, timeout=60
            ) as response:
                payload = response.read(MAX_JSON_BYTES + 1)
                if len(payload) > MAX_JSON_BYTES:
                    raise ReleaseReconciliationError(
                        "GitHub JSON response exceeded size limit"
                    )
                if expect_no_content:
                    if response.status != 204 or payload:
                        raise ReleaseReconciliationError(
                            f"GitHub {method} response was not empty HTTP 204"
                        )
                    return None
                if response.status < 200 or response.status >= 300:
                    raise ReleaseReconciliationError(
                        f"GitHub {method} returned unexpected HTTP {response.status}"
                    )
                return _load_json(payload, f"GitHub {method} response")
        except urllib.error.HTTPError as error:
            if allow_not_found and error.code == 404:
                return None
            raise ReleaseReconciliationError(
                f"GitHub {method} request failed with HTTP {error.code}"
            ) from error
        except urllib.error.URLError as error:
            raise ReleaseReconciliationError(
                f"GitHub {method} request failed before receiving a response"
            ) from error

    def _api(self, suffix: str) -> str:
        repository = "/".join(
            urllib.parse.quote(part, safe="") for part in self.repository.split("/")
        )
        return f"{self.api_url}/repos/{repository}{suffix}"

    def get_release_by_tag(self, tag: str) -> dict[str, Any] | None:
        encoded_tag = urllib.parse.quote(tag, safe="")
        result = self._request_json(
            "GET", self._api(f"/releases/tags/{encoded_tag}"), allow_not_found=True
        )
        if result is not None and not isinstance(result, dict):
            raise ReleaseReconciliationError("GitHub release lookup was not an object")
        return result

    def create_draft_release(self, tag: str, target_commitish: str) -> dict[str, Any]:
        payload = json.dumps(
            {
                "tag_name": tag,
                "target_commitish": target_commitish,
                "name": tag,
                "draft": True,
                "prerelease": False,
                "generate_release_notes": True,
            },
            separators=(",", ":"),
        ).encode("utf-8")
        result = self._request_json(
            "POST",
            self._api("/releases"),
            body=payload,
            content_type="application/json",
        )
        if not isinstance(result, dict):
            raise ReleaseReconciliationError(
                "GitHub release creation was not an object"
            )
        return result

    def get_tag_commit_sha(self, tag: str) -> str:
        """Resolve one exact hosted tag ref through bounded annotated tags."""

        encoded_tag = urllib.parse.quote(tag, safe="")
        result = self._request_json("GET", self._api(f"/git/ref/tags/{encoded_tag}"))
        if not isinstance(result, dict) or result.get("ref") != f"refs/tags/{tag}":
            raise ReleaseReconciliationError(
                "GitHub tag lookup did not return the exact requested ref"
            )
        target = result.get("object")
        for _depth in range(16):
            if not isinstance(target, dict):
                raise ReleaseReconciliationError("GitHub tag target is not an object")
            target_type = target.get("type")
            target_sha = target.get("sha")
            if not isinstance(target_sha, str) or SHA_RE.fullmatch(target_sha) is None:
                raise ReleaseReconciliationError(
                    "GitHub tag target has no canonical SHA"
                )
            if target_type == "commit":
                return target_sha
            if target_type != "tag":
                raise ReleaseReconciliationError(
                    "GitHub tag does not ultimately target a commit"
                )
            result = self._request_json("GET", self._api(f"/git/tags/{target_sha}"))
            if not isinstance(result, dict) or result.get("sha") != target_sha:
                raise ReleaseReconciliationError(
                    "GitHub annotated-tag lookup returned inconsistent metadata"
                )
            target = result.get("object")
        raise ReleaseReconciliationError(
            "GitHub annotated-tag chain exceeded the safety limit"
        )

    def list_release_assets(self, release_id: int) -> list[dict[str, Any]]:
        assets: list[dict[str, Any]] = []
        for page in range(1, 102):
            result = self._request_json(
                "GET",
                self._api(f"/releases/{release_id}/assets?per_page=100&page={page}"),
            )
            if not isinstance(result, list):
                raise ReleaseReconciliationError("GitHub asset page was not an array")
            assets.extend(result)
            if len(result) < 100:
                return assets
        raise ReleaseReconciliationError(
            "GitHub asset pagination exceeded safety limit"
        )

    def delete_release_asset(self, asset_id: int) -> None:
        self._request_json(
            "DELETE",
            self._api(f"/releases/assets/{asset_id}"),
            expect_no_content=True,
        )

    def upload_release_asset(self, release_id: int, asset: LocalAsset) -> None:
        payload = asset.path.read_bytes()
        if len(payload) != asset.size or (
            f"sha256:{hashlib.sha256(payload).hexdigest()}" != asset.digest
        ):
            raise ReleaseReconciliationError(
                f"local release asset changed after inventory: {asset.path}"
            )
        repository = "/".join(
            urllib.parse.quote(part, safe="") for part in self.repository.split("/")
        )
        name = urllib.parse.quote(asset.name, safe="")
        result = self._request_json(
            "POST",
            f"{self.uploads_url}/repos/{repository}/releases/{release_id}/assets?name={name}",
            body=payload,
            content_type="application/octet-stream",
        )
        if not isinstance(result, dict):
            raise ReleaseReconciliationError("GitHub asset upload was not an object")
        if not _is_positive_int(result.get("id")) or result.get("name") != asset.name:
            raise ReleaseReconciliationError(
                f"GitHub upload response did not identify asset {asset.name!r}"
            )

    def publish_release(self, release_id: int) -> dict[str, Any]:
        payload = json.dumps(
            {"draft": False, "prerelease": False}, separators=(",", ":")
        ).encode("utf-8")
        result = self._request_json(
            "PATCH",
            self._api(f"/releases/{release_id}"),
            body=payload,
            content_type="application/json",
        )
        if not isinstance(result, dict):
            raise ReleaseReconciliationError("GitHub release update was not an object")
        return result


def reconcile_release(
    client: GitHubReleaseClient,
    *,
    tag: str,
    target_commitish: str,
    local_assets: dict[str, LocalAsset],
    verify_attempts: int = 12,
    verify_delay_seconds: float = 5.0,
) -> None:
    """Replace the complete hosted asset set, publish, and verify exact metadata."""

    if not tag:
        raise ReleaseReconciliationError("tag must be non-empty")
    if SHA_RE.fullmatch(target_commitish) is None:
        raise ReleaseReconciliationError(
            "target commit must be a canonical lowercase commit SHA"
        )
    if not _is_positive_int(verify_attempts):
        raise ReleaseReconciliationError("verification attempts must be positive")

    if client.get_tag_commit_sha(tag) != target_commitish:
        raise ReleaseReconciliationError(
            "GitHub release tag does not resolve to the expected commit"
        )

    release = client.get_release_by_tag(tag)
    if release is None:
        release = client.create_draft_release(tag, target_commitish)
        if release.get("draft") is not True or release.get("prerelease") is not False:
            raise ReleaseReconciliationError(
                "new GitHub release was not created as an explicit non-prerelease draft"
            )
    release_id = validate_release_identity(release, tag=tag)

    current_assets = client.list_release_assets(release_id)
    already_exact = False
    try:
        validate_release_identity(
            release,
            tag=tag,
            expected_release_id=release_id,
            require_published=True,
        )
        validate_published_assets(current_assets, local_assets)
    except ReleaseReconciliationError:
        pass
    else:
        already_exact = True
    if already_exact:
        if client.get_tag_commit_sha(tag) != target_commitish:
            raise ReleaseReconciliationError(
                "GitHub release tag changed during idempotent verification"
            )
        return
    immutable = release.get("immutable")
    if immutable is not None and type(immutable) is not bool:
        raise ReleaseReconciliationError(
            "GitHub release immutable state is not boolean"
        )
    if immutable is True:
        raise ReleaseReconciliationError(
            "immutable GitHub release does not already match the local release"
        )
    if release.get("draft") is not True:
        raise ReleaseReconciliationError(
            "published GitHub release does not already match the exact local release; "
            "refusing destructive asset replacement"
        )

    asset_ids = validate_reconcilable_remote_assets(current_assets)
    for asset_id in asset_ids:
        client.delete_release_asset(asset_id)
    remaining_assets = client.list_release_assets(release_id)
    if remaining_assets != []:
        raise ReleaseReconciliationError(
            "GitHub release still has assets after replacement preflight"
        )

    for name in sorted(local_assets, key=lambda value: value.encode("utf-8")):
        client.upload_release_asset(release_id, local_assets[name])

    updated = client.publish_release(release_id)
    validate_release_identity(updated, tag=tag, expected_release_id=release_id)

    last_error: ReleaseReconciliationError | None = None
    for attempt in range(verify_attempts):
        try:
            published = client.get_release_by_tag(tag)
            if published is None:
                raise ReleaseReconciliationError(
                    "published GitHub release disappeared during verification"
                )
            validate_release_identity(
                published,
                tag=tag,
                expected_release_id=release_id,
                require_published=True,
            )
            if client.get_tag_commit_sha(tag) != target_commitish:
                raise ReleaseReconciliationError(
                    "GitHub release tag changed during reconciliation"
                )
            validate_published_assets(
                client.list_release_assets(release_id), local_assets
            )
            return
        except ReleaseReconciliationError as error:
            last_error = error
            if attempt + 1 < verify_attempts:
                time.sleep(verify_delay_seconds)
    assert last_error is not None
    raise ReleaseReconciliationError(
        f"hosted GitHub release did not converge: {last_error}"
    ) from last_error


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--target-commitish", required=True)
    parser.add_argument("--dist", type=Path, required=True)
    parser.add_argument("--expected-files", type=int, required=True)
    parser.add_argument("--token-env", default="GH_TOKEN")
    parser.add_argument(
        "--api-url", default=os.environ.get("GITHUB_API_URL", "https://api.github.com")
    )
    parser.add_argument(
        "--uploads-url",
        default=os.environ.get("GITHUB_UPLOADS_URL", "https://uploads.github.com"),
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    token = os.environ.get(args.token_env, "")
    try:
        local_assets = inventory_local_assets(args.dist, args.expected_files)
        client = GitHubReleaseClient(
            repository=args.repository,
            token=token,
            api_url=args.api_url,
            uploads_url=args.uploads_url,
        )
        reconcile_release(
            client,
            tag=args.tag,
            target_commitish=args.target_commitish,
            local_assets=local_assets,
        )
    except ReleaseReconciliationError as error:
        print(f"GitHub release reconciliation failed: {error}", file=sys.stderr)
        return 1
    print(
        f"GitHub release verified: {args.tag} has exactly "
        f"{len(local_assets)} uploaded assets with matching SHA-256 digests"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
