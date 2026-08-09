#!/usr/bin/env python3
"""Tests for exact GitHub Release asset reconciliation."""

from __future__ import annotations

import hashlib
import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "reconcile_github_release.py"
SPEC = importlib.util.spec_from_file_location("reconcile_github_release", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
release = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = release
SPEC.loader.exec_module(release)


def _write_assets(root: Path, names: tuple[str, ...]) -> dict[str, Any]:
    for index, name in enumerate(names, start=1):
        (root / name).write_bytes(f"asset-{index}\n".encode())
    return release.inventory_local_assets(root, len(names))


def _remote_asset(asset_id: int, asset: Any, **overrides: Any) -> dict[str, Any]:
    value = {
        "id": asset_id,
        "name": asset.name,
        "state": "uploaded",
        "size": asset.size,
        "digest": asset.digest,
    }
    value.update(overrides)
    return value


class FakeClient:
    def __init__(
        self,
        *,
        existing: bool,
        local_assets: dict[str, Any],
        target_commitish: str,
    ) -> None:
        self.local_assets = local_assets
        self.target_commitish = target_commitish
        self.commit_lookups: list[str] = []
        self.release = (
            {
                "id": 71,
                "tag_name": "v0.1.3",
                "draft": False,
                "prerelease": True,
                "published_at": "2026-08-08T01:02:03Z",
            }
            if existing
            else None
        )
        first = next(iter(local_assets.values()))
        self.assets: list[dict[str, Any]] = [
            _remote_asset(101, first),
            _remote_asset(102, first),
            {
                "id": 103,
                "name": "stale.txt",
                "state": "uploaded",
                "size": 5,
                "digest": f"sha256:{'0' * 64}",
            },
        ]
        self.created: list[tuple[str, str]] = []
        self.deleted: list[int] = []
        self.uploaded: list[str] = []
        self.published: list[int] = []
        self.next_asset_id = 1000

    def get_tag_commit_sha(self, tag: str) -> str:
        self.commit_lookups.append(tag)
        return self.target_commitish

    def get_release_by_tag(self, tag: str) -> dict[str, Any] | None:
        if self.release is None:
            return None
        return dict(self.release)

    def create_draft_release(self, tag: str, target_commitish: str) -> dict[str, Any]:
        self.created.append((tag, target_commitish))
        self.release = {
            "id": 71,
            "tag_name": tag,
            "draft": True,
            "prerelease": False,
            "published_at": None,
        }
        return dict(self.release)

    def list_release_assets(self, release_id: int) -> list[dict[str, Any]]:
        self.assert_release_id(release_id)
        return [dict(asset) for asset in self.assets]

    def delete_release_asset(self, asset_id: int) -> None:
        self.deleted.append(asset_id)
        self.assets = [asset for asset in self.assets if asset["id"] != asset_id]

    def upload_release_asset(self, release_id: int, asset: Any) -> None:
        self.assert_release_id(release_id)
        self.uploaded.append(asset.name)
        self.assets.append(_remote_asset(self.next_asset_id, asset))
        self.next_asset_id += 1

    def publish_release(self, release_id: int) -> dict[str, Any]:
        self.assert_release_id(release_id)
        assert self.release is not None
        self.published.append(release_id)
        self.release["draft"] = False
        self.release["prerelease"] = False
        self.release["published_at"] = "2026-08-09T03:04:05Z"
        return dict(self.release)

    def assert_release_id(self, release_id: int) -> None:
        if release_id != 71:
            raise AssertionError(release_id)


class GitHubReleaseReconciliationTests(unittest.TestCase):
    def test_exact_tag_ref_resolution_peels_annotated_tags_to_a_commit(self) -> None:
        client = release.GitHubReleaseClient(
            repository="HyunjoJung/rxls",
            token="test-token",
        )
        tag_object_sha = "a" * 40
        commit_sha = "b" * 40
        responses = {
            "/git/ref/tags/v0.1.3": {
                "ref": "refs/tags/v0.1.3",
                "object": {"type": "tag", "sha": tag_object_sha},
            },
            f"/git/tags/{tag_object_sha}": {
                "sha": tag_object_sha,
                "object": {"type": "commit", "sha": commit_sha},
            },
        }
        calls: list[str] = []

        def request_json(method: str, url: str, **_kwargs: Any) -> Any:
            self.assertEqual(method, "GET")
            suffix = url.removeprefix("https://api.github.com/repos/HyunjoJung/rxls")
            calls.append(suffix)
            return responses[suffix]

        client._request_json = request_json  # type: ignore[method-assign]

        self.assertEqual(client.get_tag_commit_sha("v0.1.3"), commit_sha)
        self.assertEqual(
            calls,
            ["/git/ref/tags/v0.1.3", f"/git/tags/{tag_object_sha}"],
        )

    def test_local_inventory_is_exact_flat_nonempty_and_hashed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            assets = _write_assets(root, ("b.txt", "a.bin"))

            self.assertEqual(list(assets), ["a.bin", "b.txt"])
            payload = (root / "a.bin").read_bytes()
            self.assertEqual(assets["a.bin"].size, len(payload))
            self.assertEqual(
                assets["a.bin"].digest,
                f"sha256:{hashlib.sha256(payload).hexdigest()}",
            )

            with self.assertRaisesRegex(
                release.ReleaseReconciliationError, "expected exactly 3"
            ):
                release.inventory_local_assets(root, 3)

            (root / "empty").write_bytes(b"")
            with self.assertRaisesRegex(release.ReleaseReconciliationError, "empty"):
                release.inventory_local_assets(root, 3)

    def test_local_inventory_rejects_directories_and_symlinks(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "nested").mkdir()
            with self.assertRaisesRegex(
                release.ReleaseReconciliationError, "regular non-symlink"
            ):
                release.inventory_local_assets(root, 1)

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target = root / "target"
            target.write_bytes(b"payload")
            (root / "alias").symlink_to(target)
            with self.assertRaisesRegex(
                release.ReleaseReconciliationError, "regular non-symlink"
            ):
                release.inventory_local_assets(root, 2)

    def test_existing_draft_deletes_stale_and_duplicate_assets_then_replaces_all(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            assets = _write_assets(Path(temporary), ("a", "b"))
            client = FakeClient(
                existing=True,
                local_assets=assets,
                target_commitish="a" * 40,
            )
            assert client.release is not None
            client.release["draft"] = True
            client.release["published_at"] = None

            release.reconcile_release(
                client,
                tag="v0.1.3",
                target_commitish="a" * 40,
                local_assets=assets,
                verify_attempts=1,
                verify_delay_seconds=0,
            )

            self.assertEqual(client.created, [])
            self.assertEqual(client.deleted, [101, 102, 103])
            self.assertEqual(client.uploaded, ["a", "b"])
            self.assertEqual(client.published, [71])
            self.assertFalse(client.release["draft"])
            self.assertFalse(client.release["prerelease"])
            self.assertEqual(client.commit_lookups, ["v0.1.3", "v0.1.3"])

    def test_existing_published_nonexact_release_fails_without_asset_mutation(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            assets = _write_assets(Path(temporary), ("a", "b"))
            client = FakeClient(
                existing=True,
                local_assets=assets,
                target_commitish="a" * 40,
            )
            assert client.release is not None
            client.release["prerelease"] = False

            with self.assertRaisesRegex(
                release.ReleaseReconciliationError,
                "published GitHub release does not already match",
            ):
                release.reconcile_release(
                    client,
                    tag="v0.1.3",
                    target_commitish="a" * 40,
                    local_assets=assets,
                    verify_attempts=1,
                    verify_delay_seconds=0,
                )

            self.assertEqual(client.deleted, [])
            self.assertEqual(client.uploaded, [])
            self.assertEqual(client.published, [])
            self.assertEqual(client.commit_lookups, ["v0.1.3"])

    def test_absent_release_is_created_as_explicit_draft_before_upload(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            assets = _write_assets(Path(temporary), ("a", "b"))
            client = FakeClient(
                existing=False,
                local_assets=assets,
                target_commitish="b" * 40,
            )
            client.assets = []

            release.reconcile_release(
                client,
                tag="v0.1.3",
                target_commitish="b" * 40,
                local_assets=assets,
                verify_attempts=1,
                verify_delay_seconds=0,
            )

            self.assertEqual(client.created, [("v0.1.3", "b" * 40)])
            self.assertEqual(client.uploaded, ["a", "b"])
            self.assertEqual(client.published, [71])

    def test_unusable_remote_metadata_fails_before_any_mutation(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            assets = _write_assets(Path(temporary), ("a", "b"))
            client = FakeClient(
                existing=True,
                local_assets=assets,
                target_commitish="c" * 40,
            )
            assert client.release is not None
            client.release["draft"] = True
            client.release["published_at"] = None
            client.assets[1]["id"] = 101

            with self.assertRaisesRegex(
                release.ReleaseReconciliationError, "IDs are duplicated"
            ):
                release.reconcile_release(
                    client,
                    tag="v0.1.3",
                    target_commitish="c" * 40,
                    local_assets=assets,
                    verify_attempts=1,
                    verify_delay_seconds=0,
                )

            self.assertEqual(client.deleted, [])
            self.assertEqual(client.uploaded, [])
            self.assertEqual(client.published, [])

    def test_exact_published_release_is_an_idempotent_noop_even_when_immutable(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            assets = _write_assets(Path(temporary), ("a", "b"))
            client = FakeClient(
                existing=True,
                local_assets=assets,
                target_commitish="d" * 40,
            )
            assert client.release is not None
            client.release["prerelease"] = False
            client.release["immutable"] = True
            client.assets = [
                _remote_asset(201, assets["a"]),
                _remote_asset(202, assets["b"]),
            ]

            release.reconcile_release(
                client,
                tag="v0.1.3",
                target_commitish="d" * 40,
                local_assets=assets,
                verify_attempts=1,
                verify_delay_seconds=0,
            )

            self.assertEqual(client.deleted, [])
            self.assertEqual(client.uploaded, [])
            self.assertEqual(client.published, [])
            self.assertEqual(client.commit_lookups, ["v0.1.3", "v0.1.3"])

    def test_inexact_immutable_release_fails_before_asset_mutation(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            assets = _write_assets(Path(temporary), ("a", "b"))
            client = FakeClient(
                existing=True,
                local_assets=assets,
                target_commitish="e" * 40,
            )
            assert client.release is not None
            client.release["immutable"] = True

            with self.assertRaisesRegex(
                release.ReleaseReconciliationError,
                "immutable GitHub release does not already match",
            ):
                release.reconcile_release(
                    client,
                    tag="v0.1.3",
                    target_commitish="e" * 40,
                    local_assets=assets,
                    verify_attempts=1,
                    verify_delay_seconds=0,
                )

            self.assertEqual(client.deleted, [])
            self.assertEqual(client.uploaded, [])
            self.assertEqual(client.published, [])

    def test_tag_must_resolve_to_exact_canonical_target_before_mutation(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            assets = _write_assets(Path(temporary), ("a", "b"))
            client = FakeClient(
                existing=True,
                local_assets=assets,
                target_commitish="f" * 40,
            )

            with self.assertRaisesRegex(
                release.ReleaseReconciliationError,
                "tag does not resolve to the expected commit",
            ):
                release.reconcile_release(
                    client,
                    tag="v0.1.3",
                    target_commitish="0" * 40,
                    local_assets=assets,
                    verify_attempts=1,
                    verify_delay_seconds=0,
                )
            with self.assertRaisesRegex(
                release.ReleaseReconciliationError,
                "canonical lowercase commit SHA",
            ):
                release.reconcile_release(
                    client,
                    tag="v0.1.3",
                    target_commitish="main",
                    local_assets=assets,
                    verify_attempts=1,
                    verify_delay_seconds=0,
                )

            self.assertEqual(client.deleted, [])
            self.assertEqual(client.uploaded, [])
            self.assertEqual(client.published, [])

    def test_published_metadata_requires_exact_names_state_sizes_and_digests(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            assets = _write_assets(Path(temporary), ("a", "b"))
            valid = [
                _remote_asset(1, assets["a"]),
                _remote_asset(2, assets["b"]),
            ]
            release.validate_published_assets(valid, assets)

            mutations = {
                "count": valid[:1],
                "duplicate name": [valid[0], {**valid[1], "name": "a"}],
                "unexpected name": [valid[0], {**valid[1], "name": "c"}],
                "state": [valid[0], {**valid[1], "state": "new"}],
                "size": [valid[0], {**valid[1], "size": valid[1]["size"] + 1}],
                "missing digest": [valid[0], {**valid[1], "digest": None}],
                "digest": [valid[0], {**valid[1], "digest": f"sha256:{'f' * 64}"}],
                "duplicate ID": [valid[0], {**valid[1], "id": 1}],
            }
            for name, remote in mutations.items():
                with self.subTest(name=name):
                    with self.assertRaises(release.ReleaseReconciliationError):
                        release.validate_published_assets(remote, assets)

    def test_final_release_identity_requires_tag_timestamp_and_normalized_flags(
        self,
    ) -> None:
        valid = {
            "id": 9,
            "tag_name": "v0.1.3",
            "draft": False,
            "prerelease": False,
            "published_at": "2026-08-09T00:00:00Z",
        }
        self.assertEqual(
            release.validate_release_identity(
                valid,
                tag="v0.1.3",
                expected_release_id=9,
                require_published=True,
            ),
            9,
        )
        mutations = {
            "tag_name": "v9",
            "draft": True,
            "prerelease": True,
            "published_at": None,
        }
        for field, value in mutations.items():
            with self.subTest(field=field):
                with self.assertRaises(release.ReleaseReconciliationError):
                    release.validate_release_identity(
                        {**valid, field: value},
                        tag="v0.1.3",
                        expected_release_id=9,
                        require_published=True,
                    )


if __name__ == "__main__":
    unittest.main()
