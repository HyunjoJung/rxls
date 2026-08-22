#!/usr/bin/env python3
"""Acquire and verify the pinned, OFL-only render-oracle font pack.

The checked-in lock records immutable upstream revisions, archive hashes,
selected members, and exact font/license hashes. Payloads remain below
``local/render-fonts`` and are never committed. ``--verify`` is offline.
"""

from __future__ import annotations

import argparse
import hashlib
from html import escape as xml_escape
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import tempfile
import time
from urllib.error import HTTPError, URLError
from urllib.parse import quote
from urllib.request import Request, urlopen
import zipfile


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_LOCK = ROOT / "scripts" / "render-fonts-lock.json"
DEFAULT_DEST = ROOT / "local" / "render-fonts" / "pack"
LOCAL_ROOT = (ROOT / "local" / "render-fonts").resolve()
LOCK_SCHEMA = "rxls.render-fonts-lock.v1"
MANIFEST_SCHEMA = "rxls.render-font-pack.v1"
HEX40 = re.compile(r"[0-9a-f]{40}\Z")
HEX64 = re.compile(r"[0-9a-f]{64}\Z")
MAX_DOWNLOAD_BYTES = 64 * 1024 * 1024
MAX_FONT_BYTES = 32 * 1024 * 1024
MAX_ARCHIVE_ENTRIES = 10_000
MAX_ARCHIVE_UNCOMPRESSED_BYTES = 256 * 1024 * 1024
MAX_PACK_BYTES = 128 * 1024 * 1024
MAX_ALIASES = 128
USER_AGENT = "rxls-render-font-fetcher/1"


class FontPackError(RuntimeError):
    """A stable font-pack contract failed."""


def canonical_json_bytes(value: object) -> bytes:
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode("utf-8")


def sha256_bytes(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def sha256_file(path: Path, limit: int = MAX_PACK_BYTES) -> str:
    digest = hashlib.sha256()
    total = 0
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            total += len(chunk)
            if total > limit:
                raise FontPackError("file_limit")
            digest.update(chunk)
    return digest.hexdigest()


def safe_relative(value: object, *, basename: bool = False) -> str:
    if not isinstance(value, str) or not value or "\0" in value or "\\" in value:
        raise FontPackError("unsafe_path")
    path = PurePosixPath(value)
    if path.is_absolute() or ".." in path.parts or value != path.as_posix():
        raise FontPackError("unsafe_path")
    if basename and len(path.parts) != 1:
        raise FontPackError("unsafe_output_name")
    return value


def validate_lock(document: object) -> dict:
    if not isinstance(document, dict) or document.get("schema") != LOCK_SCHEMA:
        raise FontPackError("lock_schema")
    if document.get("license") != "SIL-OFL-1.1":
        raise FontPackError("lock_license")
    sources = document.get("sources")
    if not isinstance(sources, list) or not sources:
        raise FontPackError("lock_sources")
    ids: list[str] = []
    outputs: set[str] = set()
    available_families: set[str] = set()
    for source in sources:
        if not isinstance(source, dict):
            raise FontPackError("lock_source")
        source_id = source.get("id")
        repo = source.get("repo")
        commit = source.get("commit")
        if not isinstance(source_id, str) or not re.fullmatch(
            r"[a-z0-9][a-z0-9-]*", source_id
        ):
            raise FontPackError("source_id")
        if not isinstance(repo, str) or not re.fullmatch(r"[^/\s]+/[^/\s]+", repo):
            raise FontPackError("source_repo")
        if not isinstance(commit, str) or not HEX40.fullmatch(commit):
            raise FontPackError("source_commit")
        ids.append(source_id)
        archive = source.get("archive")
        if archive is not None:
            _validate_payload(archive, "archive", MAX_DOWNLOAD_BYTES)
            if not str(archive["url"]).startswith("https://github.com/"):
                raise FontPackError("archive_url")
        fonts = source.get("fonts")
        if not isinstance(fonts, list) or not fonts:
            raise FontPackError("source_fonts")
        source_outputs: list[str] = []
        for font in fonts:
            if not isinstance(font, dict):
                raise FontPackError("font_row")
            _validate_payload(font, "font", MAX_FONT_BYTES, require_url=False)
            source_path = safe_relative(font.get("source_path"))
            output = safe_relative(font.get("output"), basename=True)
            if Path(source_path).suffix.lower() not in {".otf", ".ttf"}:
                raise FontPackError("font_extension")
            if Path(output).suffix.lower() not in {".otf", ".ttf"}:
                raise FontPackError("font_extension")
            if not isinstance(font.get("family"), str) or not font["family"]:
                raise FontPackError("font_family")
            available_families.add(normalize_family(font["family"]))
            if font.get("style") not in {"normal", "italic"}:
                raise FontPackError("font_style")
            if not isinstance(font.get("weight"), int) or not 1 <= font["weight"] <= 1000:
                raise FontPackError("font_weight")
            source_outputs.append(output)
            if output in outputs:
                raise FontPackError("duplicate_output")
            outputs.add(output)
        if source_outputs != sorted(source_outputs):
            raise FontPackError("font_order")
        license_row = source.get("license")
        if not isinstance(license_row, dict):
            raise FontPackError("source_license")
        _validate_payload(license_row, "license", 1024 * 1024, require_url=True)
        safe_relative(license_row.get("source_path"))
        license_output = safe_relative(license_row.get("output"), basename=True)
        if license_output in outputs:
            raise FontPackError("duplicate_output")
        outputs.add(license_output)
        if commit not in str(license_row["url"]):
            raise FontPackError("license_url_unpinned")
    if ids != sorted(set(ids)):
        raise FontPackError("source_order")
    aliases = document.get("aliases", [])
    if not isinstance(aliases, list) or len(aliases) > MAX_ALIASES:
        raise FontPackError("lock_aliases")
    normalized_aliases: list[str] = []
    for alias in aliases:
        if not isinstance(alias, dict) or set(alias) != {"family", "substitute"}:
            raise FontPackError("alias_row")
        family = alias.get("family")
        substitute = alias.get("substitute")
        if not valid_alias_family(family) or not valid_alias_family(substitute):
            raise FontPackError("alias_family")
        normalized = normalize_family(family)
        if normalize_family(substitute) not in available_families:
            raise FontPackError("alias_substitute")
        normalized_aliases.append(normalized)
    if normalized_aliases != sorted(set(normalized_aliases)):
        raise FontPackError("alias_order")
    safe_relative(document.get("default_output"))
    return document


def normalize_family(value: str) -> str:
    return value.strip().lower()


def valid_alias_family(value: object) -> bool:
    return (
        isinstance(value, str)
        and 0 < len(value) <= 128
        and value == value.strip()
        and value.isascii()
        and all(character.isprintable() for character in value)
    )


def _validate_payload(
    row: object,
    kind: str,
    maximum: int,
    *,
    require_url: bool = True,
) -> None:
    if not isinstance(row, dict):
        raise FontPackError(f"{kind}_row")
    size = row.get("bytes")
    digest = row.get("sha256")
    if not isinstance(size, int) or not 0 < size <= maximum:
        raise FontPackError(f"{kind}_bytes")
    if not isinstance(digest, str) or not HEX64.fullmatch(digest):
        raise FontPackError(f"{kind}_sha256")
    if require_url and (
        not isinstance(row.get("url"), str)
        or not row["url"].startswith("https://")
    ):
        raise FontPackError(f"{kind}_url")


def load_lock(path: Path = DEFAULT_LOCK) -> tuple[dict, bytes]:
    try:
        payload = path.read_bytes()
        document = json.loads(payload)
    except (OSError, json.JSONDecodeError) as error:
        raise FontPackError("lock_unreadable") from error
    return validate_lock(document), payload


def raw_url(repo: str, commit: str, source_path: str) -> str:
    encoded = "/".join(quote(part, safe="") for part in source_path.split("/"))
    return f"https://raw.githubusercontent.com/{repo}/{commit}/{encoded}"


def _matches(path: Path, row: dict, limit: int) -> bool:
    try:
        return (
            path.is_file()
            and not path.is_symlink()
            and path.stat().st_size == row["bytes"]
            and sha256_file(path, limit) == row["sha256"]
        )
    except (OSError, FontPackError):
        return False


def _download(url: str, row: dict, output: Path, limit: int) -> None:
    if row["bytes"] > limit:
        raise FontPackError("download_limit")
    output.parent.mkdir(parents=True, exist_ok=True)
    last_error: Exception | None = None
    for attempt in range(3):
        temporary: Path | None = None
        try:
            request = Request(url, headers={"User-Agent": USER_AGENT})
            digest = hashlib.sha256()
            total = 0
            with urlopen(request, timeout=90) as response:
                descriptor, name = tempfile.mkstemp(
                    prefix=f".{output.name}.", suffix=".part", dir=output.parent
                )
                temporary = Path(name)
                with os.fdopen(descriptor, "wb") as target:
                    while True:
                        chunk = response.read(1024 * 1024)
                        if not chunk:
                            break
                        total += len(chunk)
                        if total > row["bytes"] or total > limit:
                            raise FontPackError("download_limit")
                        digest.update(chunk)
                        target.write(chunk)
            if total != row["bytes"] or digest.hexdigest() != row["sha256"]:
                raise FontPackError("download_identity")
            os.replace(temporary, output)
            temporary = None
            return
        except (HTTPError, URLError, OSError, FontPackError) as error:
            last_error = error
            if temporary is not None:
                temporary.unlink(missing_ok=True)
            if attempt < 2:
                time.sleep(0.25 * (attempt + 1))
    raise FontPackError("download_failed") from last_error


def ensure_cached(url: str, row: dict, output: Path, limit: int) -> Path:
    if not _matches(output, row, limit):
        output.unlink(missing_ok=True)
        _download(url, row, output, limit)
    return output


def _read_zip_member(archive: zipfile.ZipFile, source_path: str, row: dict) -> bytes:
    infos = archive.infolist()
    if len(infos) > MAX_ARCHIVE_ENTRIES or sum(info.file_size for info in infos) > (
        MAX_ARCHIVE_UNCOMPRESSED_BYTES
    ):
        raise FontPackError("archive_expansion_limit")
    if any(info.flag_bits & 1 for info in infos):
        raise FontPackError("archive_encrypted")
    names = [info.filename for info in infos]
    if len(names) != len(set(names)):
        raise FontPackError("archive_duplicate_member")
    for name in names:
        pure = PurePosixPath(name.replace("\\", "/"))
        if pure.is_absolute() or ".." in pure.parts:
            raise FontPackError("archive_unsafe_member")
    by_name = {info.filename: info for info in infos}
    info = by_name.get(source_path)
    if info is None or info.is_dir() or info.file_size != row["bytes"]:
        raise FontPackError("archive_member_identity")
    if info.file_size > MAX_FONT_BYTES:
        raise FontPackError("archive_member_limit")
    with archive.open(info, "r") as source:
        payload = source.read(info.file_size + 1)
    if len(payload) != row["bytes"] or sha256_bytes(payload) != row["sha256"]:
        raise FontPackError("archive_member_identity")
    return payload


def _materialize_source(source: dict, cache: Path) -> tuple[list[tuple[dict, bytes]], bytes]:
    archive_row = source.get("archive")
    if archive_row is not None:
        archive_path = ensure_cached(
            archive_row["url"],
            archive_row,
            cache / f"{source['id']}-{archive_row['sha256'][:16]}.zip",
            MAX_DOWNLOAD_BYTES,
        )
        try:
            with zipfile.ZipFile(archive_path) as archive:
                fonts = [
                    (font, _read_zip_member(archive, font["source_path"], font))
                    for font in source["fonts"]
                ]
                license_payload = _read_zip_member(
                    archive, source["license"]["source_path"], source["license"]
                )
        except (OSError, zipfile.BadZipFile, RuntimeError) as error:
            raise FontPackError("archive_invalid") from error
        return fonts, license_payload

    fonts = []
    for font in source["fonts"]:
        url = raw_url(source["repo"], source["commit"], font["source_path"])
        path = ensure_cached(
            url,
            font,
            cache / f"{source['id']}-{font['output']}",
            MAX_FONT_BYTES,
        )
        fonts.append((font, path.read_bytes()))
    license_row = source["license"]
    license_path = ensure_cached(
        license_row["url"],
        license_row,
        cache / f"{source['id']}-{license_row['output']}",
        1024 * 1024,
    )
    return fonts, license_path.read_bytes()


def fonts_conf(lock: dict) -> bytes:
    available = {
        font["family"]
        for source in lock["sources"]
        for font in source["fonts"]
    }
    first_family = lock["sources"][0]["fonts"][0]["family"]
    lines = [
        '<?xml version="1.0"?>',
        '<!DOCTYPE fontconfig SYSTEM "fonts.dtd">',
        "<fontconfig>",
        '  <dir prefix="relative">fonts</dir>',
        '  <cachedir prefix="xdg">fontconfig</cachedir>',
        "  <config><rescan><int>0</int></rescan></config>",
    ]
    for alias in lock.get("aliases", []):
        lines.extend(
            [
                '  <alias binding="strong">',
                f"    <family>{xml_escape(alias['family'])}</family>",
                "    <prefer>",
                f"      <family>{xml_escape(alias['substitute'])}</family>",
                "    </prefer>",
                "  </alias>",
            ]
        )
    generic_fallbacks = [
        (
            "sans-serif",
            ["Arimo", "Carlito", "Noto Sans CJK KR", "Noto Sans Arabic", "Noto Sans Hebrew"],
        ),
        (
            "serif",
            ["Tinos", "Caladea", "Noto Sans CJK KR", "Noto Sans Arabic", "Noto Sans Hebrew"],
        ),
        (
            "monospace",
            ["Cousine", "Noto Sans CJK KR", "Noto Sans Arabic", "Noto Sans Hebrew"],
        ),
    ]
    for generic, families in generic_fallbacks:
        families = [family for family in families if family in available]
        if not families:
            families = [first_family]
        lines.extend(
            [
                '  <alias binding="strong">',
                f"    <family>{generic}</family>",
                "    <prefer>",
                *(f"      <family>{family}</family>" for family in families),
                "    </prefer>",
                "  </alias>",
            ]
        )
    lines.append("</fontconfig>")
    return ("\n".join(lines) + "\n").encode("utf-8")


def expected_identity(lock: dict) -> dict:
    font_rows = []
    license_rows = []
    for source in lock["sources"]:
        for font in source["fonts"]:
            font_rows.append(
                {
                    "bytes": font["bytes"],
                    "commit": source["commit"],
                    "family": font["family"],
                    "output": f"fonts/{font['output']}",
                    "repo": source["repo"],
                    "sha256": font["sha256"],
                    "source_id": source["id"],
                    "source_path": font["source_path"],
                    "style": font["style"],
                    "weight": font["weight"],
                }
            )
        license_row = source["license"]
        license_rows.append(
            {
                "bytes": license_row["bytes"],
                "output": f"licenses/{license_row['output']}",
                "sha256": license_row["sha256"],
                "source_id": source["id"],
                "url": license_row["url"],
            }
        )
    identity = {
        "fonts": font_rows,
        "fonts_conf_sha256": sha256_bytes(fonts_conf(lock)),
        "licenses": license_rows,
    }
    if "aliases" in lock:
        identity["aliases"] = [dict(alias) for alias in lock["aliases"]]
    return identity


def _write(path: Path, payload: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("xb") as target:
        target.write(payload)


def acquire(lock: dict, lock_bytes: bytes, destination: Path) -> dict:
    destination.parent.mkdir(parents=True, exist_ok=True)
    cache = destination.parent / "cache"
    cache.mkdir(parents=True, exist_ok=True)
    stage = Path(tempfile.mkdtemp(prefix=f".{destination.name}.stage-", dir=destination.parent))
    backup: Path | None = None
    try:
        font_rows = []
        license_rows = []
        total = 0
        for source in lock["sources"]:
            fonts, license_payload = _materialize_source(source, cache)
            for font, payload in fonts:
                output = f"fonts/{font['output']}"
                _write(stage / output, payload)
                total += len(payload)
                font_rows.append(
                    {
                        "bytes": font["bytes"],
                        "commit": source["commit"],
                        "family": font["family"],
                        "output": output,
                        "repo": source["repo"],
                        "sha256": font["sha256"],
                        "source_id": source["id"],
                        "source_path": font["source_path"],
                        "style": font["style"],
                        "weight": font["weight"],
                    }
                )
            license_row = source["license"]
            output = f"licenses/{license_row['output']}"
            _write(stage / output, license_payload)
            total += len(license_payload)
            license_rows.append(
                {
                    "bytes": license_row["bytes"],
                    "output": output,
                    "sha256": license_row["sha256"],
                    "source_id": source["id"],
                    "url": license_row["url"],
                }
            )
        configuration = fonts_conf(lock)
        _write(stage / "fonts.conf", configuration)
        total += len(configuration)
        if total > MAX_PACK_BYTES:
            raise FontPackError("pack_limit")
        identity = expected_identity(lock)
        if identity["fonts"] != font_rows or identity["licenses"] != license_rows:
            raise FontPackError("materialized_identity")
        manifest = {
            **identity,
            "license": lock["license"],
            "lock_sha256": sha256_bytes(lock_bytes),
            "pack_sha256": sha256_bytes(canonical_json_bytes(identity)),
            "schema": MANIFEST_SCHEMA,
            "total_bytes": total,
        }
        _write(stage / "manifest.json", canonical_json_bytes(manifest))
        if destination.exists():
            if not destination.is_dir() or destination.is_symlink():
                raise FontPackError("destination_invalid")
            backup = destination.parent / f".{destination.name}.backup-{os.getpid()}"
            if backup.exists():
                raise FontPackError("backup_exists")
            os.replace(destination, backup)
        try:
            os.replace(stage, destination)
        except BaseException:
            if backup is not None and backup.exists() and not destination.exists():
                os.replace(backup, destination)
            raise
        if backup is not None:
            shutil.rmtree(backup)
        return manifest
    finally:
        if stage.exists():
            shutil.rmtree(stage)
        if backup is not None and backup.exists():
            shutil.rmtree(backup)


def verify(lock: dict, lock_bytes: bytes, destination: Path) -> dict:
    manifest_path = destination / "manifest.json"
    try:
        if manifest_path.stat().st_size > 4 * 1024 * 1024:
            raise FontPackError("manifest_limit")
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise FontPackError("manifest_unreadable") from error
    if not isinstance(manifest, dict) or manifest.get("schema") != MANIFEST_SCHEMA:
        raise FontPackError("manifest_schema")
    if manifest.get("lock_sha256") != sha256_bytes(lock_bytes):
        raise FontPackError("manifest_lock_identity")
    identity = expected_identity(lock)
    for field, expected in identity.items():
        if manifest.get(field) != expected:
            raise FontPackError(f"manifest_{field}")
    if manifest.get("license") != lock["license"]:
        raise FontPackError("manifest_license")
    expected_paths = {"manifest.json", "fonts.conf"}
    total = 0
    for source in lock["sources"]:
        for font in source["fonts"]:
            output = f"fonts/{font['output']}"
            expected_paths.add(output)
            path = destination / output
            if not _matches(path, font, MAX_FONT_BYTES):
                raise FontPackError("font_identity")
            total += font["bytes"]
        license_row = source["license"]
        output = f"licenses/{license_row['output']}"
        expected_paths.add(output)
        path = destination / output
        if not _matches(path, license_row, 1024 * 1024):
            raise FontPackError("license_identity")
        total += license_row["bytes"]
    configuration = fonts_conf(lock)
    config_path = destination / "fonts.conf"
    if not config_path.is_file() or config_path.read_bytes() != configuration:
        raise FontPackError("fontconfig_identity")
    total += len(configuration)
    all_paths = list(destination.rglob("*"))
    if any(path.is_symlink() for path in all_paths):
        raise FontPackError("pack_symlink")
    actual_paths = {
        path.relative_to(destination).as_posix()
        for path in all_paths
        if path.is_file()
    }
    if actual_paths != expected_paths:
        raise FontPackError("pack_file_set")
    if manifest.get("pack_sha256") != sha256_bytes(canonical_json_bytes(identity)):
        raise FontPackError("pack_identity")
    if manifest.get("total_bytes") != total or total > MAX_PACK_BYTES:
        raise FontPackError("pack_bytes")
    return manifest


def resolve_destination(value: str | None) -> Path:
    candidate = Path(value) if value else DEFAULT_DEST
    if not candidate.is_absolute():
        candidate = ROOT / candidate
    if candidate.is_symlink():
        raise FontPackError("destination_symlink")
    destination = candidate.resolve()
    try:
        relative = destination.relative_to(LOCAL_ROOT)
    except ValueError as error:
        raise FontPackError("destination_outside_local") from error
    if not relative.parts:
        raise FontPackError("destination_is_local_root")
    return destination


def summary(manifest: dict, destination: Path | None = None) -> str:
    suffix = f" output={destination}" if destination is not None else ""
    return (
        f"fonts={len(manifest['fonts'])} sources={len(manifest['licenses'])} "
        f"bytes={manifest['total_bytes']} pack_sha256={manifest['pack_sha256']}{suffix}"
    )


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--list", action="store_true")
    action.add_argument("--acquire", action="store_true")
    action.add_argument("--verify", action="store_true")
    parser.add_argument("--lock", type=Path, default=DEFAULT_LOCK)
    parser.add_argument("--dest")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        lock, lock_bytes = load_lock(args.lock)
        if args.list:
            plan = {
                "fonts": [font for source in lock["sources"] for font in source["fonts"]],
                "aliases": lock.get("aliases", []),
                "license": lock["license"],
                "schema": lock["schema"],
                "sources": [source["id"] for source in lock["sources"]],
            }
            print(canonical_json_bytes(plan).decode("utf-8"), end="")
            return 0
        destination = resolve_destination(args.dest)
        if args.acquire:
            manifest = acquire(lock, lock_bytes, destination)
            print(f"acquired: {summary(manifest, destination)}")
            return 0
        manifest = verify(lock, lock_bytes, destination)
        print(f"verified: {summary(manifest, destination)}")
        return 0
    except FontPackError as error:
        print(f"fetch-render-fonts: {error}", file=os.sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
