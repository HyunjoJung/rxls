#!/usr/bin/env python3
"""Bounded visual parity runner for rxls-render and LibreOffice Calc.

The harness treats a LibreOffice ``SinglePageSheets`` PDF export as the visual
oracle for one workbook at a time.  It validates the deterministic
``rxls-render bundle`` contract, rasterizes both sides when optional local
dependencies are available, and emits path-neutral JSON evidence.

No dependency is downloaded by this script.  Visual comparison requires
Pillow, Poppler ``pdftotext``, either PyMuPDF (``fitz``) or Poppler
(``pdfinfo`` + ``pdftoppm``), and either CairoSVG or an explicitly configured
SVG rasterizer command.  Required font packs additionally use pinned Poppler
``pdffonts`` to attest every LibreOffice PDF font object without retaining its
name.  A bit-exact NumPy integer implementation accelerates
the reference metric; unlocked runs fall back to the tested pure-Python
implementation, while locked oracle profiles require the recorded NumPy
version.
``--dry-run`` is dependency-tolerant and performs corpus, tool, command, and
limit preflight without executing either renderer.

The visual evidence deliberately uses fixed integer heuristics rather than
OCR.  A separate semantic-content check compares the renderer's bounded,
path-outline visible labels with pinned Poppler ``pdftotext`` output.  The
full ARIA labels remain required and are used to validate that the visible
labels cannot inject content.  Only counts and scores are retained, never
workbook text:

* foreground means that at least one RGB channel is below 248;
* an edge/grid/border candidate has an orthogonal luma contrast of at least
  32 (the mask can also include glyph and drawing edges); and
* conservative text-like ink is a pixel with luma at most 192 and an
  orthogonal neighbour at least 32 luma levels lighter.  This rejects broad
  dark fills but can include borders and does not claim to recognize text.

Mask matching permits a one-pixel Chebyshev displacement.  Blurred-luma
similarity uses integer BT.601-style luma followed by a separable, rounded
three-pixel box blur.  Page and aggregate ratios are always derived from raw
counts; page ratios are never averaged.  Bounding boxes use inclusive pixel
coordinates.  Signed centroid deltas are ``rxls - LibreOffice`` in thousandths
of a pixel.  Aggregate geometry virtually stacks pages top-to-bottom in
workbook order.  Matched-color error pairs each rxls foreground pixel with its
lowest-error LibreOffice foreground candidate inside the one-pixel window;
this directional diagnostic does not claim a one-to-one assignment.
When both compared masks are empty, precision, recall, and F1 are one million;
when exactly one is empty, all three are zero.  Raw blank-page geometry remains
in each file result, but wholly blank pairs and one-sided semantic output are
excluded from per-format/per-feature fidelity-score cohorts.
"""

from __future__ import annotations

import argparse
from collections import Counter
from dataclasses import dataclass
from fractions import Fraction
import hashlib
import importlib.metadata
import importlib.util
import json
import math
import os
import platform
from pathlib import Path, PurePosixPath
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from typing import Any, Protocol, Sequence
import unicodedata
import xml.etree.ElementTree as ET
import zipfile


ROOT = Path(__file__).resolve().parents[1]
ORACLE_PROFILE_PATH = (
    ROOT
    / "scripts"
    / "render-oracle-container"
    / "profile"
    / "registrymodifications.xcu"
)
EVIDENCE_SCHEMA = "rxls.libreoffice-render-parity.v1"
RENDER_MANIFEST_SCHEMA = "rxls.render.bundle.v1"
CONTAINER_OUTPUT_SCHEMA = "rxls.render-oracle-container-output.v2"
CONTAINER_EXECUTION_SCHEMA = "rxls.render-oracle-container-execution.v2"
CONTAINER_IDENTITY_SCHEMA = "rxls.render-oracle-container-identity.v1"
CONTAINER_LIBREOFFICE_ARTIFACT_SHA256 = (
    "18838cb9d028b664a9d0e966cd4c8ca47ca3ea363c393b41d1b5124740b121a5"
)
FIXED_UNITS_PER_PIXEL = 1024
SUPPORTED_EXTENSIONS = {
    ".xls",
    ".xlsx",
    ".xlsm",
    ".xlsb",
    ".ods",
}
MAX_ARTIFACT_FILES = 4096
PDF_FILTER = (
    'pdf:calc_pdf_Export:{"SinglePageSheets":{"type":"boolean","value":"true"}}'
)
AUTHORED_PDF_FILTER = "pdf:calc_pdf_Export"
PRINT_MODE_SINGLE_PAGE = "single-page-sheets"
PRINT_MODE_AUTHORED = "authored"
PRINT_MODES = frozenset({PRINT_MODE_SINGLE_PAGE, PRINT_MODE_AUTHORED})
SHA256_RE = re.compile(r"[0-9a-f]{64}\Z")
SAFE_LABEL_PART_RE = re.compile(r"[^A-Za-z0-9._+@()\[\]{} -]+")
LOCALE_RE = re.compile(r"[A-Za-z0-9_.@-]{1,64}\Z")
SVG_LENGTH_RE = re.compile(
    r"\s*([+-]?(?:\d+(?:\.\d*)?|\.\d+))(px|pt|pc|in|cm|mm|q)?\s*\Z",
    re.IGNORECASE,
)
SVG_URL_RE = re.compile(r"url\(\s*(['\"]?)(.*?)\1\s*\)", re.IGNORECASE)
SVG_PATH_TOKEN_RE = re.compile(
    r"[A-Za-z]|[+-]?(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][+-]?\d+)?"
)
SVG_CLIP_REFERENCE_RE = re.compile(
    r"url\(\s*(['\"]?)#([A-Za-z_][A-Za-z0-9_.:-]*)\1\s*\)\Z"
)
SVG_PATH_COMMANDS = frozenset("MmLlHhVvCcSsQqTtZz")
MAX_SVG_PATH_TOKENS = 2_000_000
MAX_TEXT_BOX_MATCH_WORK = 25_000_000
# Poppler can report glyph bounds outside a tightly cropped Calc page.  A
# locked-font 40-workbook pilot measured valid chart/RTL crop overhangs up to
# 5.923697 points; six points is the absolute clamp allowance and larger
# escapes are malformed evidence.
BBOX_COORDINATE_EPSILON_POINTS = 6.0
PDFTOTEXT_XHTML_DOCTYPE = (
    b'<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Transitional//EN" '
    b'"http://www.w3.org/TR/xhtml1/DTD/xhtml1-transitional.dtd">'
)
PDFFONTS_HEADER = (
    "name                                 type              encoding         "
    "emb sub uni object ID"
)
PDFFONTS_SEPARATOR = (
    "------------------------------------ ----------------- ---------------- "
    "--- --- --- ---------"
)
PDF_SUBSET_PREFIX_RE = re.compile(r"[A-Z]{6}\+")
PDF_FONT_NAME_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9._,+-]{0,127}\Z")
PDF_FONT_ENCODING_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9._+-]{0,15}\Z")
PDF_FONT_TYPES = frozenset(
    {
        "Type 1",
        "Type 1C",
        "Type 1C (OT)",
        "Type 3",
        "TrueType",
        "TrueType (OT)",
        "CID Type 0",
        "CID Type 0C",
        "CID Type 0C (OT)",
        "CID TrueType",
        "CID TrueType (OT)",
    }
)
FOREGROUND_CHANNEL_THRESHOLD = 248
EDGE_LUMA_DELTA = 32
TEXT_INK_MAX_LUMA = 192
METRIC_WORK_UNITS_PER_PIXEL = 128
SEMANTIC_IGNORED_CODEPOINTS = frozenset(
    {
        "\u00ad",  # soft hyphen inserted for layout
        "\u061c",  # Arabic letter mark
        "\u200e",  # left-to-right mark
        "\u200f",  # right-to-left mark
        "\u202a",  # bidi embeddings/overrides/pop directional formatting
        "\u202b",
        "\u202c",
        "\u202d",
        "\u202e",
        "\u2066",  # bidi isolates/pop directional isolate
        "\u2067",
        "\u2068",
        "\u2069",
        "\ufeff",  # byte-order mark
    }
)
INVALID_VISIBLE_LABEL_CATEGORIES = frozenset({"Cc", "Cs"})


class HarnessError(RuntimeError):
    """A deterministic harness contract failed."""


@dataclass(frozen=True)
class Caps:
    max_input_bytes: int = 64 * 1024 * 1024
    max_total_input_bytes: int = 2 * 1024 * 1024 * 1024
    max_command_output_bytes: int = 1024 * 1024
    max_artifact_bytes: int = 256 * 1024 * 1024
    max_svg_bytes: int = 64 * 1024 * 1024
    max_pages: int = 64
    max_page_pixels: int = 40_000_000
    max_total_pixels: int = 200_000_000
    max_metric_work_units: int = 25_600_000_000
    max_semantic_codepoints: int = 1_000_000
    max_semantic_tokens: int = 250_000
    timeout_seconds: float = 60.0


@dataclass(frozen=True)
class InputCase:
    path: Path
    label: str
    size: int | None
    expected_sha256: str | None = None
    expected_bytes: int | None = None
    rights_tier: str | None = None
    features: tuple[str, ...] = ()


@dataclass(frozen=True)
class SemanticTextBox:
    """One in-memory text label and its page-space box in PDF points."""

    tokens: tuple[str, ...]
    bbox_points: tuple[float, float, float, float]


@dataclass(frozen=True)
class SvgSemanticEvidence:
    """Content is transient; reports retain only aggregate numeric evidence."""

    tokens: tuple[str, ...]
    boxes: tuple[SemanticTextBox, ...]
    unbounded_items: int
    path_tokens: int = 0


@dataclass(frozen=True)
class PdfTextPage:
    """One transient Poppler page; text never crosses the report boundary."""

    width_points: float
    height_points: float
    words: tuple[SemanticTextBox, ...]


@dataclass(frozen=True)
class PdfFontRecord:
    """One transient, validated row from pinned Poppler ``pdffonts``."""

    normalized_identity: str
    embedded: bool
    subset: bool
    unicode_map: bool


@dataclass(frozen=True)
class CommandResult:
    status: str
    returncode: int | None
    stdout: bytes = b""
    stderr: bytes = b""


class CommandRunner(Protocol):
    def run(
        self,
        command: Sequence[str],
        *,
        cwd: Path,
        env: dict[str, str],
        timeout_seconds: float,
        output_limit_bytes: int,
    ) -> CommandResult: ...


class BoundedCommandRunner:
    """Run a command while bounding wall time and retained pipe output."""

    def run(
        self,
        command: Sequence[str],
        *,
        cwd: Path,
        env: dict[str, str],
        timeout_seconds: float,
        output_limit_bytes: int,
    ) -> CommandResult:
        if not command:
            return CommandResult("not_found", None)
        try:
            process = subprocess.Popen(
                list(command),
                cwd=cwd,
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                start_new_session=(os.name != "nt"),
            )
        except (FileNotFoundError, PermissionError, OSError):
            return CommandResult("not_found", None)

        output_limit_bytes = max(1, output_limit_bytes)
        stdout = bytearray()
        stderr = bytearray()
        lock = threading.Lock()
        over_limit = threading.Event()
        total_read = 0

        def drain(stream: Any, destination: bytearray) -> None:
            nonlocal total_read
            try:
                while True:
                    chunk = stream.read(64 * 1024)
                    if not chunk:
                        break
                    with lock:
                        remaining = max(0, output_limit_bytes - total_read)
                        if remaining:
                            destination.extend(chunk[:remaining])
                        total_read += len(chunk)
                        if total_read > output_limit_bytes:
                            over_limit.set()
            finally:
                stream.close()

        assert process.stdout is not None and process.stderr is not None
        threads = [
            threading.Thread(target=drain, args=(process.stdout, stdout), daemon=True),
            threading.Thread(target=drain, args=(process.stderr, stderr), daemon=True),
        ]
        for thread in threads:
            thread.start()

        deadline = time.monotonic() + timeout_seconds
        status: str | None = None
        while process.poll() is None:
            if over_limit.is_set():
                status = "output_limit"
                _terminate_process(process)
                break
            if time.monotonic() >= deadline:
                status = "timeout"
                _terminate_process(process)
                break
            time.sleep(0.01)

        try:
            returncode = process.wait(timeout=2.0)
        except subprocess.TimeoutExpired:
            _kill_process(process)
            returncode = process.wait()
        for thread in threads:
            thread.join(timeout=2.0)

        if status is None and over_limit.is_set():
            status = "output_limit"
        if status is None:
            status = "ok" if returncode == 0 else "nonzero"
        return CommandResult(status, returncode, bytes(stdout), bytes(stderr))


def _terminate_process(process: subprocess.Popen[bytes]) -> None:
    try:
        if os.name != "nt":
            os.killpg(process.pid, 15)
        else:
            process.terminate()
    except (ProcessLookupError, OSError):
        pass


def _kill_process(process: subprocess.Popen[bytes]) -> None:
    try:
        if os.name != "nt":
            os.killpg(process.pid, 9)
        else:
            process.kill()
    except (ProcessLookupError, OSError):
        pass


@dataclass(frozen=True)
class Backends:
    pillow: bool
    pymupdf: bool
    cairosvg: bool
    svg_command: tuple[str, ...] | None = None
    pdftoppm: bool = False
    pdfinfo: bool = False
    pdftotext: bool = False
    pdffonts: bool = False

    @classmethod
    def detect(cls, svg_command: Sequence[str] | None = None) -> "Backends":
        command = tuple(svg_command) if svg_command else None
        return cls(
            pillow=importlib.util.find_spec("PIL") is not None,
            pymupdf=importlib.util.find_spec("fitz") is not None,
            cairosvg=importlib.util.find_spec("cairosvg") is not None,
            svg_command=command,
            pdftoppm=executable_available(os.environ.get("PDFTOPPM", "pdftoppm")),
            pdfinfo=executable_available(os.environ.get("PDFINFO", "pdfinfo")),
            pdftotext=executable_available(os.environ.get("PDFTOTEXT", "pdftotext")),
            pdffonts=executable_available(os.environ.get("PDFFONTS", "pdffonts")),
        )

    def missing(self, *, require_pdffonts: bool = False) -> list[str]:
        missing = []
        if not self.pillow:
            missing.append("pillow")
        if not self.pymupdf and not (self.pdftoppm and self.pdfinfo):
            missing.append("pymupdf_or_poppler")
        if self.svg_command is None and not self.cairosvg:
            missing.append("cairosvg_or_svg_command")
        if self.svg_command and not executable_available(self.svg_command[0]):
            missing.append("svg_command")
        if not self.pdftotext:
            missing.append("pdftotext")
        if require_pdffonts and not self.pdffonts:
            missing.append("pdffonts")
        return missing


@dataclass(frozen=True)
class HarnessConfig:
    rxls_command: tuple[str, ...]
    libreoffice: str
    svg_rasterizer_command: tuple[str, ...] | None
    caps: Caps
    dpi: int
    locale: str
    dry_run: bool
    min_similarity_ppm: int | None
    fail_on_incomparable: bool
    require_font_pack: bool = False
    font_pack: "FontPack | None" = None
    oracle_profile: "OracleProfile | None" = None
    renderer_identity: dict[str, object] | None = None
    libreoffice_command: tuple[str, ...] | None = None
    pdffonts_identity: dict[str, object] | None = None
    print_mode: str = PRINT_MODE_SINGLE_PAGE
    format_filter: tuple[str, ...] = ()
    required_feature_filter: tuple[str, ...] = ()


@dataclass(frozen=True)
class FontPack:
    """Verified, path-private font-pack configuration and identity."""

    root: Path
    fonts_conf: Path
    font_paths: tuple[Path, ...]
    evidence: dict[str, object]
    pdf_identities: frozenset[str]


@dataclass(frozen=True)
class OracleProfile:
    """Fail-closed identity contract for one supported oracle environment."""

    name: str
    system: str
    machine: str
    locale: str
    timezone: str
    dpi: int
    pdf_filter: str
    profile_sha256: str
    font_pack_sha256: str
    libreoffice_version: str
    libreoffice_sha256: str
    python_version: str
    python_executable_sha256: str
    numpy_version: str
    pillow_version: str
    cairosvg_version: str
    pdfinfo_version: str
    pdfinfo_sha256: str
    pdftoppm_version: str
    pdftoppm_sha256: str
    pdftotext_version: str
    pdftotext_sha256: str
    pdffonts_version: str
    pdffonts_sha256: str
    source_evidence: dict[str, object]


@dataclass(frozen=True)
class BundlePage:
    index: int
    visibility: str
    svg_path: Path
    width_pixels: int
    height_pixels: int
    scene_sha256: str
    warnings: tuple[tuple[str, int, dict[str, int] | None], ...]


@dataclass(frozen=True)
class Bundle:
    pages: tuple[BundlePage, ...]
    renderer: dict[str, object]
    artifact_bytes: int


def executable_available(executable: str) -> bool:
    if not executable:
        return False
    expanded = os.path.expanduser(executable)
    if os.path.sep in expanded or (os.path.altsep and os.path.altsep in expanded):
        path = Path(expanded)
        return path.is_file() and os.access(path, os.X_OK)
    return shutil.which(expanded) is not None


def _sha256_file(path: Path, max_bytes: int | None = None) -> str:
    digest = hashlib.sha256()
    total = 0
    with path.open("rb") as source:
        while True:
            chunk = source.read(1024 * 1024)
            if not chunk:
                break
            total += len(chunk)
            if max_bytes is not None and total > max_bytes:
                raise HarnessError("input_limit")
            digest.update(chunk)
    return digest.hexdigest()


def renderer_binary_identity(
    command: Sequence[str],
    expected_sha256: str | None,
    *,
    required: bool,
) -> dict[str, object] | None:
    """Hash a direct rxls-render executable without retaining its host path."""
    if expected_sha256 is not None and not SHA256_RE.fullmatch(expected_sha256):
        raise HarnessError("renderer_binary_sha256")
    if not command:
        if required:
            raise HarnessError("renderer_binary_identity_required")
        return None
    executable_name = Path(command[0]).name
    if not executable_name.startswith("rxls-render"):
        if expected_sha256 is not None or required:
            raise HarnessError("renderer_direct_binary_required")
        return None
    if expected_sha256 is None and not required and not executable_available(command[0]):
        return None
    path = _resolved_executable(command[0], "renderer_binary")
    size = path.stat().st_size
    if size <= 0 or size > 512 * 1024 * 1024:
        raise HarnessError("renderer_binary_size")
    digest = _sha256_file(path, 512 * 1024 * 1024)
    if expected_sha256 is not None and digest != expected_sha256:
        raise HarnessError("renderer_binary_identity")
    return {"bytes": size, "sha256": digest}


def pdffonts_binary_identity(
    expected_sha256: str | None,
    *,
    required: bool,
) -> dict[str, object] | None:
    """Hash the active PDF font inspector and bind it to an expected lock."""
    if expected_sha256 is not None and not SHA256_RE.fullmatch(expected_sha256):
        raise HarnessError("pdffonts_binary_sha256")
    if expected_sha256 is None and not required:
        return None
    if expected_sha256 is None:
        raise HarnessError("pdffonts_binary_identity_required")
    path = _resolved_executable(
        os.environ.get("PDFFONTS", "pdffonts"), "pdffonts_binary"
    )
    digest = _sha256_file(path, 512 * 1024 * 1024)
    if digest != expected_sha256:
        raise HarnessError("pdffonts_binary_identity")
    return {"kind": "poppler", "pdffonts_sha256": digest}


def _canonical_json_bytes(value: object) -> bytes:
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode("utf-8")


def _safe_pack_member(root: Path, value: object) -> Path:
    if not isinstance(value, str) or not value or "\0" in value or "\\" in value:
        raise HarnessError("font_pack_path")
    pure = PurePosixPath(value)
    if pure.is_absolute() or ".." in pure.parts or value != pure.as_posix():
        raise HarnessError("font_pack_path")
    path = root.joinpath(*pure.parts)
    try:
        path.resolve(strict=False).relative_to(root)
    except ValueError as error:
        raise HarnessError("font_pack_path") from error
    if path.is_symlink() or not path.is_file():
        raise HarnessError("font_pack_file")
    return path


def _normalized_pdf_font_identity(value: object, code: str) -> str:
    """Normalize one trusted PDF/PostScript font identity for exact matching."""
    if (
        not isinstance(value, str)
        or not 1 <= len(value) <= 128
        or value != value.strip()
        or not value.isascii()
        or not value.isprintable()
    ):
        raise HarnessError(code)
    normalized = value.replace(" ", "").lower()
    if not PDF_FONT_NAME_RE.fullmatch(normalized):
        raise HarnessError(code)
    return normalized


def load_font_pack(manifest_path: Path) -> FontPack:
    """Verify a pinned local font pack without retaining its host path."""
    if manifest_path.is_symlink() or not manifest_path.is_file():
        raise HarnessError("font_pack_manifest")
    try:
        if manifest_path.stat().st_size > 4 * 1024 * 1024:
            raise HarnessError("font_pack_manifest_limit")
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise HarnessError("font_pack_manifest") from error
    if not isinstance(manifest, dict) or manifest.get("schema") != "rxls.render-font-pack.v1":
        raise HarnessError("font_pack_schema")
    root = manifest_path.parent.resolve()
    fonts = manifest.get("fonts")
    licenses = manifest.get("licenses")
    if (
        not isinstance(fonts, list)
        or not 1 <= len(fonts) <= 128
        or not isinstance(licenses, list)
        or not 1 <= len(licenses) <= 128
    ):
        raise HarnessError("font_pack_rows")
    expected_paths = {"manifest.json", "fonts.conf"}
    total_bytes = 0
    font_paths = []
    pdf_identities: set[str] = set()
    for row in fonts:
        if not isinstance(row, dict):
            raise HarnessError("font_pack_row")
        path = _safe_pack_member(root, row.get("output"))
        expected_paths.add(path.relative_to(root).as_posix())
        size = row.get("bytes")
        digest = row.get("sha256")
        if (
            not isinstance(size, int)
            or not 0 < size <= 32 * 1024 * 1024
            or not isinstance(digest, str)
            or not SHA256_RE.fullmatch(digest)
            or path.stat().st_size != size
            or _sha256_file(path, 32 * 1024 * 1024) != digest
            or not isinstance(row.get("family"), str)
            or row.get("style") not in {"normal", "italic"}
            or not isinstance(row.get("weight"), int)
        ):
            raise HarnessError("font_pack_font_identity")
        total_bytes += size
        font_paths.append(path)
        pdf_identities.add(
            _normalized_pdf_font_identity(
                row["family"], "font_pack_font_identity"
            )
        )
        pdf_identities.add(
            _normalized_pdf_font_identity(
                PurePosixPath(str(row["output"])).stem,
                "font_pack_font_identity",
            )
        )
    for row in licenses:
        if not isinstance(row, dict):
            raise HarnessError("font_pack_license")
        path = _safe_pack_member(root, row.get("output"))
        expected_paths.add(path.relative_to(root).as_posix())
        size = row.get("bytes")
        digest = row.get("sha256")
        if (
            not isinstance(size, int)
            or not 0 < size <= 1024 * 1024
            or not isinstance(digest, str)
            or not SHA256_RE.fullmatch(digest)
            or path.stat().st_size != size
            or _sha256_file(path, 1024 * 1024) != digest
        ):
            raise HarnessError("font_pack_license_identity")
        total_bytes += size
    fonts_conf = _safe_pack_member(root, "fonts.conf")
    configuration_sha = _sha256_file(fonts_conf, 1024 * 1024)
    total_bytes += fonts_conf.stat().st_size
    if configuration_sha != manifest.get("fonts_conf_sha256"):
        raise HarnessError("font_pack_config_identity")
    if total_bytes > 128 * 1024 * 1024 or manifest.get("total_bytes") != total_bytes:
        raise HarnessError("font_pack_total")
    actual_paths = set()
    for path in root.rglob("*"):
        if path.is_symlink():
            raise HarnessError("font_pack_symlink")
        if path.is_file():
            actual_paths.add(path.relative_to(root).as_posix())
    if actual_paths != expected_paths:
        raise HarnessError("font_pack_file_set")
    identity = {
        "fonts": fonts,
        "fonts_conf_sha256": configuration_sha,
        "licenses": licenses,
    }
    aliases = manifest.get("aliases")
    if aliases is not None:
        if not isinstance(aliases, list) or len(aliases) > 128:
            raise HarnessError("font_pack_aliases")
        available_families = {
            row["family"].strip().lower() for row in fonts if isinstance(row, dict)
        }
        normalized_aliases = []
        for alias in aliases:
            if not isinstance(alias, dict) or set(alias) != {"family", "substitute"}:
                raise HarnessError("font_pack_alias")
            family = alias.get("family")
            substitute = alias.get("substitute")
            if (
                not isinstance(family, str)
                or not 0 < len(family) <= 128
                or family != family.strip()
                or not family.isascii()
                or not family.isprintable()
                or not isinstance(substitute, str)
                or not 0 < len(substitute) <= 128
                or substitute != substitute.strip()
                or not substitute.isascii()
                or not substitute.isprintable()
                or substitute.lower() not in available_families
            ):
                raise HarnessError("font_pack_alias")
            normalized_aliases.append(family.lower())
        if normalized_aliases != sorted(set(normalized_aliases)):
            raise HarnessError("font_pack_alias_order")
        identity["aliases"] = aliases
    pack_sha = manifest.get("pack_sha256")
    if (
        not isinstance(pack_sha, str)
        or not SHA256_RE.fullmatch(pack_sha)
        or hashlib.sha256(_canonical_json_bytes(identity)).hexdigest() != pack_sha
    ):
        raise HarnessError("font_pack_identity")
    return FontPack(
        root=root,
        fonts_conf=fonts_conf,
        font_paths=tuple(font_paths),
        evidence={
            "alias_count": len(aliases or []),
            "font_count": len(fonts),
            "pdf_identity_count": len(pdf_identities),
            "pdf_identities_sha256": hashlib.sha256(
                (
                    ("\n".join(sorted(pdf_identities)) + "\n")
                    if pdf_identities
                    else ""
                ).encode("ascii")
            ).hexdigest(),
            "fonts_conf_sha256": configuration_sha,
            "license": manifest.get("license"),
            "pack_sha256": pack_sha,
        },
        pdf_identities=frozenset(pdf_identities),
    )


def _bounded_json_object(path: Path, *, schema: str) -> dict[str, object]:
    if path.is_symlink() or not path.is_file():
        raise HarnessError("oracle_lock_manifest")
    try:
        if path.stat().st_size > 1024 * 1024:
            raise HarnessError("oracle_lock_manifest_limit")
        document = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise HarnessError("oracle_lock_manifest") from error
    if not isinstance(document, dict) or document.get("schema") != schema:
        raise HarnessError("oracle_lock_schema")
    return document


def _exact_keys(value: object, expected: set[str], code: str) -> dict[str, object]:
    if not isinstance(value, dict) or set(value) != expected:
        raise HarnessError(code)
    return value


def _locked_text(value: object, code: str, *, max_length: int = 256) -> str:
    if (
        not isinstance(value, str)
        or not value
        or len(value) > max_length
        or any(character < " " or character == "\x7f" for character in value)
    ):
        raise HarnessError(code)
    return value


def _locked_sha256(value: object, code: str) -> str:
    if not isinstance(value, str) or not SHA256_RE.fullmatch(value):
        raise HarnessError(code)
    return value


def load_oracle_profile(lock_path: Path, requested_profile: str | None) -> OracleProfile:
    """Load one exact, path-neutral LibreOffice oracle identity profile."""
    document = _bounded_json_object(
        lock_path, schema="rxls.render-oracle-lock.v1"
    )
    if set(document) != {"default_profile", "profiles", "schema"}:
        raise HarnessError("oracle_lock_keys")
    selected = requested_profile or document.get("default_profile")
    selected = _locked_text(selected, "oracle_profile_name", max_length=96)
    rows = document.get("profiles")
    if not isinstance(rows, list) or not 1 <= len(rows) <= 32:
        raise HarnessError("oracle_profiles")
    matching = [row for row in rows if isinstance(row, dict) and row.get("name") == selected]
    if len(matching) != 1:
        raise HarnessError("oracle_profile_missing")
    row = _exact_keys(
        matching[0],
        {
            "configuration",
            "font_pack_sha256",
            "libreoffice",
            "name",
            "pdf_rasterizer",
            "platform",
            "python",
            "source",
            "svg_rasterizer",
        },
        "oracle_profile_keys",
    )
    target = _exact_keys(row["platform"], {"machine", "system"}, "oracle_platform")
    configuration = _exact_keys(
        row["configuration"],
        {"dpi", "locale", "pdf_filter", "profile_sha256", "timezone"},
        "oracle_configuration",
    )
    libreoffice = _exact_keys(
        row["libreoffice"], {"executable_sha256", "version"}, "oracle_libreoffice"
    )
    python = _exact_keys(
        row["python"],
        {
            "executable_sha256",
            "implementation",
            "numpy_version",
            "pillow_version",
            "version",
        },
        "oracle_python",
    )
    svg = _exact_keys(
        row["svg_rasterizer"],
        {"distribution", "kind", "version"},
        "oracle_svg_rasterizer",
    )
    pdf = _exact_keys(
        row["pdf_rasterizer"],
        {
            "kind",
            "pdffonts_sha256",
            "pdffonts_version",
            "pdfinfo_sha256",
            "pdfinfo_version",
            "pdftoppm_sha256",
            "pdftoppm_version",
            "pdftotext_sha256",
            "pdftotext_version",
        },
        "oracle_pdf_rasterizer",
    )
    source = _exact_keys(
        row["source"],
        {"artifact_bytes", "artifact_sha256", "artifact_url"},
        "oracle_source",
    )
    if python.get("implementation") != "cpython":
        raise HarnessError("oracle_python")
    if svg.get("kind") != "cairosvg" or svg.get("distribution") != "CairoSVG":
        raise HarnessError("oracle_svg_rasterizer")
    if pdf.get("kind") != "poppler":
        raise HarnessError("oracle_pdf_rasterizer")
    dpi = configuration.get("dpi")
    artifact_bytes = source.get("artifact_bytes")
    if not isinstance(dpi, int) or not 1 <= dpi <= 2400:
        raise HarnessError("oracle_configuration")
    if not isinstance(artifact_bytes, int) or not 0 < artifact_bytes <= 2 * 1024**3:
        raise HarnessError("oracle_source")
    artifact_url = _locked_text(source.get("artifact_url"), "oracle_source", max_length=2048)
    if not artifact_url.startswith("https://download.documentfoundation.org/"):
        raise HarnessError("oracle_source")
    source_evidence = {
        "artifact_bytes": artifact_bytes,
        "artifact_sha256": _locked_sha256(source.get("artifact_sha256"), "oracle_source"),
        "artifact_url": artifact_url,
    }
    return OracleProfile(
        name=selected,
        system=_locked_text(target.get("system"), "oracle_platform", max_length=32),
        machine=_locked_text(target.get("machine"), "oracle_platform", max_length=32),
        locale=_locked_text(configuration.get("locale"), "oracle_configuration", max_length=64),
        timezone=_locked_text(configuration.get("timezone"), "oracle_configuration", max_length=32),
        dpi=dpi,
        pdf_filter=_locked_text(configuration.get("pdf_filter"), "oracle_configuration", max_length=512),
        profile_sha256=_locked_sha256(
            configuration.get("profile_sha256"), "oracle_configuration"
        ),
        font_pack_sha256=_locked_sha256(row.get("font_pack_sha256"), "oracle_font_pack"),
        libreoffice_version=_locked_text(
            libreoffice.get("version"), "oracle_libreoffice", max_length=256
        ),
        libreoffice_sha256=_locked_sha256(
            libreoffice.get("executable_sha256"), "oracle_libreoffice"
        ),
        python_version=_locked_text(python.get("version"), "oracle_python", max_length=32),
        python_executable_sha256=_locked_sha256(
            python.get("executable_sha256"), "oracle_python"
        ),
        numpy_version=_locked_text(
            python.get("numpy_version"), "oracle_python", max_length=32
        ),
        pillow_version=_locked_text(
            python.get("pillow_version"), "oracle_python", max_length=32
        ),
        cairosvg_version=_locked_text(svg.get("version"), "oracle_svg_rasterizer", max_length=32),
        pdfinfo_version=_locked_text(pdf.get("pdfinfo_version"), "oracle_pdf_rasterizer"),
        pdfinfo_sha256=_locked_sha256(pdf.get("pdfinfo_sha256"), "oracle_pdf_rasterizer"),
        pdftoppm_version=_locked_text(pdf.get("pdftoppm_version"), "oracle_pdf_rasterizer"),
        pdftoppm_sha256=_locked_sha256(pdf.get("pdftoppm_sha256"), "oracle_pdf_rasterizer"),
        pdftotext_version=_locked_text(pdf.get("pdftotext_version"), "oracle_pdf_rasterizer"),
        pdftotext_sha256=_locked_sha256(pdf.get("pdftotext_sha256"), "oracle_pdf_rasterizer"),
        pdffonts_version=_locked_text(
            pdf.get("pdffonts_version"), "oracle_pdf_rasterizer"
        ),
        pdffonts_sha256=_locked_sha256(
            pdf.get("pdffonts_sha256"), "oracle_pdf_rasterizer"
        ),
        source_evidence=source_evidence,
    )


def _resolved_executable(value: str, code: str) -> Path:
    expanded = os.path.expanduser(value)
    if os.path.sep in expanded or (os.path.altsep and os.path.altsep in expanded):
        candidate = Path(expanded)
    else:
        located = shutil.which(expanded)
        if located is None:
            raise HarnessError(f"{code}_not_found")
        candidate = Path(located)
    try:
        resolved = candidate.resolve(strict=True)
    except OSError as error:
        raise HarnessError(f"{code}_not_found") from error
    if not resolved.is_file() or not os.access(resolved, os.X_OK):
        raise HarnessError(f"{code}_not_found")
    return resolved


def _probe_version(
    runner: CommandRunner,
    command: Sequence[str],
    *,
    code: str,
) -> str:
    result = runner.run(
        command,
        cwd=ROOT,
        env={**os.environ, "LANG": "C", "LC_ALL": "C", "TZ": "UTC"},
        timeout_seconds=10.0,
        output_limit_bytes=64 * 1024,
    )
    if result.status != "ok":
        raise HarnessError(f"{code}_version_probe")
    rendered = (result.stdout + result.stderr).decode("utf-8", "replace").strip()
    if not rendered or len(rendered) > 4096:
        raise HarnessError(f"{code}_version_probe")
    return rendered.splitlines()[0].strip()


def verify_oracle_profile(
    profile: OracleProfile,
    *,
    config: HarnessConfig,
    backends: Backends,
    runner: CommandRunner,
) -> dict[str, object]:
    """Verify every active comparison tool before rendering any workbook."""
    if config.print_mode != PRINT_MODE_SINGLE_PAGE:
        raise HarnessError("authored_print_requires_container_adapter")
    if config.libreoffice_command is not None:
        raise HarnessError("oracle_profile_direct_mode_required")
    if platform.system().lower() != profile.system or platform.machine().lower() != profile.machine:
        raise HarnessError("oracle_platform_mismatch")
    if (
        config.locale != profile.locale
        or profile.timezone != "UTC"
        or config.dpi != profile.dpi
        or profile.pdf_filter != PDF_FILTER
    ):
        raise HarnessError("oracle_configuration_mismatch")
    if config.font_pack is None:
        raise HarnessError("oracle_font_pack_required")
    if config.font_pack.evidence.get("pack_sha256") != profile.font_pack_sha256:
        raise HarnessError("oracle_font_pack_mismatch")
    if _sha256_file(ORACLE_PROFILE_PATH, 64 * 1024) != profile.profile_sha256:
        raise HarnessError("oracle_profile_identity")
    if config.svg_rasterizer_command is not None or not backends.cairosvg:
        raise HarnessError("oracle_svg_rasterizer_mismatch")
    if backends.pymupdf or not (
        backends.pdfinfo
        and backends.pdftoppm
        and backends.pdftotext
        and backends.pdffonts
    ):
        raise HarnessError("oracle_pdf_rasterizer_mismatch")

    libreoffice = _resolved_executable(config.libreoffice, "oracle_libreoffice")
    pdfinfo = _resolved_executable(os.environ.get("PDFINFO", "pdfinfo"), "oracle_pdfinfo")
    pdftoppm = _resolved_executable(
        os.environ.get("PDFTOPPM", "pdftoppm"), "oracle_pdftoppm"
    )
    pdftotext = _resolved_executable(
        os.environ.get("PDFTOTEXT", "pdftotext"), "oracle_pdftotext"
    )
    pdffonts = _resolved_executable(
        os.environ.get("PDFFONTS", "pdffonts"), "oracle_pdffonts"
    )
    python = _resolved_executable(sys.executable, "oracle_python")
    identities = (
        (libreoffice, profile.libreoffice_sha256, "oracle_libreoffice"),
        (pdfinfo, profile.pdfinfo_sha256, "oracle_pdfinfo"),
        (pdftoppm, profile.pdftoppm_sha256, "oracle_pdftoppm"),
        (pdftotext, profile.pdftotext_sha256, "oracle_pdftotext"),
        (pdffonts, profile.pdffonts_sha256, "oracle_pdffonts"),
        (python, profile.python_executable_sha256, "oracle_python"),
    )
    for path, expected, code in identities:
        if _sha256_file(path, 512 * 1024 * 1024) != expected:
            raise HarnessError(f"{code}_identity")
    if sys.implementation.name != "cpython" or platform.python_version() != profile.python_version:
        raise HarnessError("oracle_python_version")
    try:
        numpy = importlib.metadata.version("numpy")
        pillow = importlib.metadata.version("Pillow")
        cairosvg = importlib.metadata.version("CairoSVG")
    except importlib.metadata.PackageNotFoundError as error:
        raise HarnessError("oracle_python_library_missing") from error
    if (
        numpy != profile.numpy_version
        or pillow != profile.pillow_version
        or cairosvg != profile.cairosvg_version
    ):
        raise HarnessError("oracle_python_library_version")

    libreoffice_version = _probe_version(
        runner, [str(libreoffice), "--version"], code="oracle_libreoffice"
    )
    pdfinfo_version = _probe_version(
        runner, [str(pdfinfo), "-v"], code="oracle_pdfinfo"
    )
    pdftoppm_version = _probe_version(
        runner, [str(pdftoppm), "-v"], code="oracle_pdftoppm"
    )
    pdftotext_version = _probe_version(
        runner, [str(pdftotext), "-v"], code="oracle_pdftotext"
    )
    pdffonts_version = _probe_version(
        runner, [str(pdffonts), "-v"], code="oracle_pdffonts"
    )
    if libreoffice_version != profile.libreoffice_version:
        raise HarnessError("oracle_libreoffice_version")
    if pdfinfo_version != profile.pdfinfo_version:
        raise HarnessError("oracle_pdfinfo_version")
    if pdftoppm_version != profile.pdftoppm_version:
        raise HarnessError("oracle_pdftoppm_version")
    if pdftotext_version != profile.pdftotext_version:
        raise HarnessError("oracle_pdftotext_version")
    if pdffonts_version != profile.pdffonts_version:
        raise HarnessError("oracle_pdffonts_version")
    return {
        "profile": profile.name,
        "platform": {"machine": profile.machine, "system": profile.system},
        "configuration": {
            "dpi": profile.dpi,
            "locale": profile.locale,
            "pdf_filter_sha256": hashlib.sha256(profile.pdf_filter.encode()).hexdigest(),
            "profile_sha256": profile.profile_sha256,
            "timezone": profile.timezone,
        },
        "font_pack_sha256": profile.font_pack_sha256,
        "libreoffice": {
            "executable_sha256": profile.libreoffice_sha256,
            "version": libreoffice_version,
        },
        "python": {
            "cairosvg_version": cairosvg,
            "executable_sha256": profile.python_executable_sha256,
            "implementation": "cpython",
            "numpy_version": numpy,
            "pillow_version": pillow,
            "version": profile.python_version,
        },
        "pdf_rasterizer": {
            "kind": "poppler",
            "pdfinfo_sha256": profile.pdfinfo_sha256,
            "pdfinfo_version": pdfinfo_version,
            "pdftoppm_sha256": profile.pdftoppm_sha256,
            "pdftoppm_version": pdftoppm_version,
            "pdftotext_sha256": profile.pdftotext_sha256,
            "pdftotext_version": pdftotext_version,
            "pdffonts_sha256": profile.pdffonts_sha256,
            "pdffonts_version": pdffonts_version,
        },
        "source": profile.source_evidence,
        "svg_rasterizer": {"kind": "cairosvg", "version": cairosvg},
    }


def command_fact(result: CommandResult) -> dict[str, object]:
    """Retain stable execution facts without embedding path-bearing logs."""
    return {
        "status": result.status,
        "returncode": result.returncode,
        "stdout_nonempty": bool(result.stdout),
        "stderr_nonempty": bool(result.stderr),
    }


def normalize_evidence_label(raw: str, *, suffix: str = "") -> str:
    """Return a relative, control-free evidence label without host path data."""
    raw = raw.replace("\\", "/")
    path = PurePosixPath(raw)
    unsafe = (
        raw.startswith("/")
        or bool(re.match(r"^[A-Za-z]:/", raw))
        or ".." in path.parts
        or "\0" in raw
    )
    if unsafe:
        extension = suffix or PurePosixPath(raw).suffix
        return f"input-{hashlib.sha256(raw.encode('utf-8', 'replace')).hexdigest()[:16]}{extension}"
    parts = []
    for part in path.parts:
        if part in {"", "."}:
            continue
        cleaned = "".join(ch if ch >= " " and ch != "\x7f" else "_" for ch in part)
        cleaned = SAFE_LABEL_PART_RE.sub("_", cleaned).strip()
        parts.append(cleaned or "_")
    label = "/".join(parts) or "input"
    if len(label.encode("utf-8")) > 512:
        extension = suffix or PurePosixPath(label).suffix
        return f"input-{hashlib.sha256(label.encode()).hexdigest()[:16]}{extension}"
    return label


def normalized_command(tokens: Sequence[str]) -> list[str]:
    """Make command configuration useful without retaining absolute paths."""
    normalized = []
    for token in tokens:
        candidate = token.replace("\\", "/")
        if candidate.startswith("file://") or candidate.startswith("/"):
            normalized.append(f"<absolute>/{PurePosixPath(candidate).name}")
            continue
        if re.match(r"^[A-Za-z]:/", candidate):
            normalized.append(f"<absolute>/{PurePosixPath(candidate).name}")
            continue
        match = re.match(r"(?P<prefix>--[^=]+=)(?P<path>/.*)", candidate)
        if match:
            normalized.append(
                f"{match.group('prefix')}<absolute>/{PurePosixPath(match.group('path')).name}"
            )
            continue
        normalized.append(token)
    return normalized


def _safe_stat(path: Path) -> tuple[int | None, str | None]:
    try:
        if path.is_symlink():
            return None, "symlink_input"
        if not path.is_file():
            return None, "missing_input"
        return path.stat().st_size, None
    except OSError:
        return None, "unreadable_input"


def discover_corpus(
    root: Path,
    *,
    max_candidates: int,
    max_files: int,
) -> tuple[list[InputCase], dict[str, object]]:
    root = root.resolve()
    if not root.is_dir():
        raise HarnessError("corpus_not_directory")
    candidates: list[Path] = []
    seen = 0
    truncated = False
    for current, directories, filenames in os.walk(root, followlinks=False):
        directories[:] = sorted(
            [name for name in directories if not (Path(current) / name).is_symlink()],
            key=lambda value: (value.casefold(), value),
        )
        for filename in sorted(filenames, key=lambda value: (value.casefold(), value)):
            path = Path(current) / filename
            if path.suffix.lower() not in SUPPORTED_EXTENSIONS:
                continue
            seen += 1
            if seen > max_candidates:
                truncated = True
                break
            candidates.append(path)
        if truncated:
            break
    candidates.sort(
        key=lambda path: (
            path.relative_to(root).as_posix().casefold(),
            path.relative_to(root).as_posix(),
        )
    )
    selected = candidates[:max_files]
    truncated = truncated or len(candidates) > max_files
    cases = []
    for path in selected:
        label = normalize_evidence_label(path.relative_to(root).as_posix(), suffix=path.suffix)
        size, _ = _safe_stat(path)
        cases.append(InputCase(path, label, size))
    return cases, {
        "candidate_count": min(seen, max_candidates),
        "selected_count": len(cases),
        "truncated": truncated,
    }


def discover_manifest(
    manifest: Path,
    *,
    max_manifest_bytes: int,
    max_candidates: int,
    max_files: int,
) -> tuple[list[InputCase], dict[str, object]]:
    try:
        payload = manifest.read_bytes()
    except OSError as error:
        raise HarnessError("manifest_unreadable") from error
    if len(payload) > max_manifest_bytes:
        raise HarnessError("manifest_limit")
    try:
        document = json.loads(payload)
    except json.JSONDecodeError as error:
        raise HarnessError("manifest_invalid_json") from error
    rows = document.get("files") if isinstance(document, dict) else None
    if not isinstance(rows, list):
        raise HarnessError("manifest_files_missing")

    render_corpus = document.get("schema") == "rxls.render-corpus-manifest.v1"
    cases_with_order: list[tuple[str, int, int, InputCase]] = []
    candidates = 0
    truncated = False
    for offset, row in enumerate(rows):
        if not isinstance(row, dict):
            continue
        status = row.get("status")
        if render_corpus:
            if row.get("eligible") is not True or status not in {"ready", "duplicate"}:
                continue
            if "render_selected" in row and row.get("render_selected") is not True:
                continue
        elif status not in {None, "downloaded", "generated", "available"}:
            continue
        raw_path = row.get("local_path")
        if not render_corpus and not isinstance(raw_path, str):
            raw_path = row.get("path")
        label_value = row.get("source_path") if render_corpus else row.get("path")
        if not isinstance(raw_path, str) or not isinstance(label_value, str):
            continue
        path = Path(raw_path)
        if not path.is_absolute():
            manifest_root = manifest.parent.resolve()
            path = manifest.parent / path
            try:
                path = path.resolve(strict=False)
                path.relative_to(manifest_root)
            except ValueError as error:
                raise HarnessError("manifest_local_path_unsafe") from error
        elif render_corpus:
            raise HarnessError("render_manifest_absolute_local_path")
        if path.suffix.lower() not in SUPPORTED_EXTENSIONS:
            continue
        candidates += 1
        if candidates > max_candidates:
            truncated = True
            break
        label = normalize_evidence_label(label_value, suffix=path.suffix)
        source = row.get("source_id") if render_corpus else row.get("source")
        if isinstance(source, str) and source:
            label = f"{normalize_evidence_label(source)}/{label}"
        size, _ = _safe_stat(path)
        expected_sha256 = row.get("sha256")
        if not isinstance(expected_sha256, str) or not SHA256_RE.fullmatch(expected_sha256):
            expected_sha256 = None
        expected_bytes = row.get("bytes")
        if not render_corpus and not isinstance(expected_bytes, int):
            expected_bytes = row.get("byte_length")
        if not isinstance(expected_bytes, int) or expected_bytes < 0:
            expected_bytes = None
        rights_tier = row.get("rights_tier")
        if rights_tier not in {"S", "U", "Q"}:
            rights_tier = None
        raw_features = row.get("features")
        features: tuple[str, ...] = ()
        if (
            isinstance(raw_features, list)
            and all(isinstance(feature, str) and feature for feature in raw_features)
            and raw_features == sorted(set(raw_features))
            and len(raw_features) <= 256
        ):
            features = tuple(raw_features)
        status_priority = 0 if status != "duplicate" else 1
        cases_with_order.append(
            (
                label.casefold(),
                status_priority,
                offset,
                InputCase(
                    path,
                    label,
                    size,
                    expected_sha256,
                    expected_bytes,
                    rights_tier,
                    features,
                ),
            )
        )
    canonical_by_path: dict[str, tuple[str, int, int, InputCase]] = {}
    for item in cases_with_order:
        _, status_priority, offset, case = item
        key = os.path.normcase(os.path.abspath(case.path))
        previous = canonical_by_path.get(key)
        rank = (status_priority, case.label.casefold(), case.label, offset)
        if previous is None:
            canonical_by_path[key] = item
            continue
        previous_rank = (
            previous[1],
            previous[3].label.casefold(),
            previous[3].label,
            previous[2],
        )
        if rank < previous_rank:
            canonical_by_path[key] = item
    deduplicated = sorted(
        (item[3] for item in canonical_by_path.values()),
        key=lambda case: (case.label.casefold(), case.label),
    )
    cases = deduplicated[:max_files]
    truncated = truncated or len(deduplicated) > max_files
    return cases, {
        "candidate_count": min(candidates, max_candidates),
        "selected_count": len(cases),
        "truncated": truncated,
    }


def select_shard(
    cases: Sequence[InputCase],
    discovery: dict[str, object],
    *,
    shard_count: int,
    shard_index: int,
    max_files: int,
) -> tuple[list[InputCase], dict[str, object]]:
    """Select a deterministic content-identity shard before applying the cap."""
    selected = []
    for case in cases:
        identity = case.expected_sha256
        if identity is None:
            identity = hashlib.sha256(case.label.encode("utf-8")).hexdigest()
        if int(identity[:16], 16) % shard_count == shard_index:
            selected.append(case)
    capped = selected[:max_files]
    return capped, {
        **discovery,
        "pre_shard_selected_count": len(cases),
        "shard_candidate_count": len(selected),
        "selected_count": len(capped),
        "shard_count": shard_count,
        "shard_index": shard_index,
        "truncated": bool(discovery.get("truncated")) or len(selected) > max_files,
    }


def filter_cases(
    cases: Sequence[InputCase],
    discovery: dict[str, object],
    *,
    formats: Sequence[str],
    required_features: Sequence[str],
) -> tuple[list[InputCase], dict[str, object]]:
    """Apply an explicit manifest-backed lane filter before content sharding."""
    normalized_formats = tuple(sorted(set(formats)))
    normalized_features = tuple(sorted(set(required_features)))
    if any(value not in {suffix.lstrip(".") for suffix in SUPPORTED_EXTENSIONS} for value in normalized_formats):
        raise HarnessError("format_filter")
    if any(
        not re.fullmatch(r"[a-z][a-z0-9-]{0,63}", value)
        for value in normalized_features
    ):
        raise HarnessError("required_feature_filter")
    if not normalized_formats and not normalized_features:
        return list(cases), discovery
    selected = [
        case
        for case in cases
        if (
            not normalized_formats
            or case.path.suffix.lower().lstrip(".") in normalized_formats
        )
        and all(feature in case.features for feature in normalized_features)
    ]
    return selected, discovery


def _parse_svg_length(value: str, dpi: int) -> int:
    match = SVG_LENGTH_RE.fullmatch(value)
    if not match:
        raise HarnessError("svg_dimension_invalid")
    number = Fraction(match.group(1))
    unit = (match.group(2) or "px").lower()
    if number <= 0:
        raise HarnessError("svg_dimension_invalid")
    factors = {
        "px": Fraction(dpi, 96),
        "pt": Fraction(dpi, 72),
        "pc": Fraction(dpi, 6),
        "in": Fraction(dpi),
        "cm": Fraction(dpi * 50, 127),
        "mm": Fraction(dpi * 5, 127),
        "q": Fraction(dpi * 5, 508),
    }
    pixels = number * factors[unit]
    return (pixels.numerator + pixels.denominator - 1) // pixels.denominator


def inspect_svg(path: Path, *, dpi: int, max_svg_bytes: int) -> tuple[int, int]:
    try:
        payload = path.read_bytes()
    except OSError as error:
        raise HarnessError("svg_unreadable") from error
    if len(payload) > max_svg_bytes:
        raise HarnessError("svg_output_limit")
    upper = payload[:4096].upper()
    if b"<!DOCTYPE" in upper or b"<!ENTITY" in upper:
        raise HarnessError("svg_unsafe_markup")
    try:
        root = ET.fromstring(payload)
    except ET.ParseError as error:
        raise HarnessError("svg_invalid_xml") from error
    if root.tag.rsplit("}", 1)[-1] != "svg":
        raise HarnessError("svg_root_invalid")

    def reject_external_url(value: str) -> None:
        for match in SVG_URL_RE.finditer(value):
            target = match.group(2).strip()
            if target and not target.startswith(("#", "data:")):
                raise HarnessError("svg_external_reference")

    for element in root.iter():
        element_name = element.tag.rsplit("}", 1)[-1].lower()
        for name, value in element.attrib.items():
            local_name = name.rsplit("}", 1)[-1].lower()
            if local_name == "href" and value and not value.startswith(("#", "data:")):
                # An SVG anchor is inert navigation metadata during PNG
                # rasterization. Resource-bearing hrefs must remain local.
                if element_name != "a":
                    raise HarnessError("svg_external_reference")
                continue
            reject_external_url(value)
        if element.text:
            reject_external_url(element.text)

    width = root.attrib.get("width")
    height = root.attrib.get("height")
    if width is not None and height is not None:
        return _parse_svg_length(width, dpi), _parse_svg_length(height, dpi)
    view_box = root.attrib.get("viewBox")
    if not view_box:
        raise HarnessError("svg_dimensions_missing")
    pieces = view_box.replace(",", " ").split()
    if len(pieces) != 4:
        raise HarnessError("svg_viewbox_invalid")
    try:
        width_value = Fraction(pieces[2])
        height_value = Fraction(pieces[3])
    except (ValueError, ZeroDivisionError) as error:
        raise HarnessError("svg_viewbox_invalid") from error
    if width_value <= 0 or height_value <= 0:
        raise HarnessError("svg_viewbox_invalid")
    return (
        math.ceil(width_value * dpi / 96),
        math.ceil(height_value * dpi / 96),
    )


def _bounded_directory_files(root: Path) -> list[Path]:
    files = []
    for current, directories, filenames in os.walk(root, followlinks=False):
        for name in directories:
            if (Path(current) / name).is_symlink():
                raise HarnessError("artifact_symlink")
        for name in filenames:
            path = Path(current) / name
            if path.is_symlink():
                raise HarnessError("artifact_symlink")
            if not path.is_file():
                raise HarnessError("artifact_special_file")
            files.append(path)
            if len(files) > MAX_ARTIFACT_FILES:
                raise HarnessError("artifact_file_count_limit")
    return sorted(files, key=lambda path: path.relative_to(root).as_posix())


def _manifest_warning_evidence(
    warning_rows: object, *, code: str
) -> tuple[tuple[str, int, dict[str, int] | None], ...]:
    if not isinstance(warning_rows, list):
        raise HarnessError(code)
    warning_evidence = []
    warning_codes = []
    for warning in warning_rows:
        if not isinstance(warning, dict) or set(warning) not in (
            {"code", "occurrences"},
            {"code", "first_cell", "occurrences"},
        ):
            raise HarnessError(code)
        warning_code = warning.get("code")
        occurrences = warning.get("occurrences")
        if (
            not isinstance(warning_code, str)
            or not re.fullmatch(r"[a-z][a-z0-9_]{0,63}", warning_code)
            or not isinstance(occurrences, int)
            or not 1 <= occurrences <= 2**63 - 1
        ):
            raise HarnessError(code)
        first_cell = warning.get("first_cell")
        if first_cell is not None:
            if (
                not isinstance(first_cell, dict)
                or set(first_cell) != {"col", "row"}
                or not isinstance(first_cell.get("row"), int)
                or not isinstance(first_cell.get("col"), int)
                or not 0 <= first_cell["row"] <= 1_048_575
                or not 0 <= first_cell["col"] <= 16_383
            ):
                raise HarnessError(code)
            first_cell = {"row": first_cell["row"], "col": first_cell["col"]}
        warning_codes.append(warning_code)
        warning_evidence.append((warning_code, occurrences, first_cell))
    if len(warning_codes) != len(set(warning_codes)):
        raise HarnessError(code)
    return tuple(warning_evidence)


def _validate_bundle_artifact(
    artifact: object,
    *,
    expected_file: str,
    bundle_dir: Path,
    files: Sequence[Path],
    max_bytes: int,
    code: str,
) -> Path:
    if not isinstance(artifact, dict) or set(artifact) != {
        "bytes",
        "file",
        "sha256",
    }:
        raise HarnessError(code)
    if artifact.get("file") != expected_file:
        raise HarnessError(code)
    path = bundle_dir.joinpath(*PurePosixPath(expected_file).parts)
    if path not in files:
        raise HarnessError(code)
    size = path.stat().st_size
    if (
        artifact.get("bytes") != size
        or artifact.get("sha256") != _sha256_file(path, max_bytes)
    ):
        raise HarnessError(code)
    return path


def _validate_authored_print_report(report: dict[str, object], page_count: int) -> None:
    """Validate content-neutral authored pagination facts before comparison."""
    paper = report.get("paper")
    content = report.get("content_rect")
    pages = report.get("pages")
    if (
        not isinstance(paper, dict)
        or set(paper) != {"code", "height_raw", "width_raw"}
        or paper.get("code") != 1
        or paper.get("width_raw") != 835_584
        or paper.get("height_raw") != 1_081_344
        or not isinstance(content, dict)
        or set(content) != {"height_raw", "width_raw", "x_raw", "y_raw"}
        or content != {
            "x_raw": 49_152,
            "y_raw": 73_728,
            "width_raw": 737_280,
            "height_raw": 933_888,
        }
        or not isinstance(pages, list)
        or len(pages) != page_count
        or page_count != 4
    ):
        raise HarnessError("render_manifest_authored_paper")
    for key in ("x_raw", "y_raw", "width_raw", "height_raw"):
        if not isinstance(content.get(key), int) or content[key] <= 0:
            raise HarnessError("render_manifest_authored_margins")
    if (
        content["x_raw"] + content["width_raw"] >= paper["width_raw"]
        or content["y_raw"] + content["height_raw"] >= paper["height_raw"]
    ):
        raise HarnessError("render_manifest_authored_margins")
    if report.get("page_order") != "over_then_down":
        raise HarnessError("render_manifest_authored_page_order")
    scale = report.get("scale_permille")
    if not isinstance(scale, int) or isinstance(scale, bool) or not 100 <= scale <= 4_000:
        raise HarnessError("render_manifest_authored_scale")
    manual_rows = report.get("manual_row_breaks")
    manual_cols = report.get("manual_col_breaks")
    if (
        not isinstance(manual_rows, list)
        or manual_rows != [8]
        or not isinstance(manual_cols, list)
        or manual_cols != [3]
    ):
        raise HarnessError("render_manifest_authored_breaks")
    for output_index, page in enumerate(pages):
        if not isinstance(page, dict) or page.get("output_index") != output_index:
            raise HarnessError("render_manifest_authored_page_map")
        page_scale = page.get("scale_permille")
        if (
            page_scale != scale
            or page.get("displayed_page_number") != output_index + 1
            or page.get("area_index") != 0
            or page.get("horizontal_index") != output_index % 2
            or page.get("vertical_index") != output_index // 2
            or page.get("manual_col_break_before") is not (output_index % 2 == 1)
            or page.get("manual_row_break_before") is not (output_index >= 2)
        ):
            raise HarnessError("render_manifest_authored_page_map")
        if page.get("repeat_rows") != [0, 0] or page.get("repeat_cols") != [5, 5]:
            raise HarnessError("render_manifest_authored_titles")
        body = page.get("body_range")
        if (
            not isinstance(body, dict)
            or body.get("first_row") != (1 if output_index < 2 else 8)
            or body.get("last_row") != (7 if output_index < 2 else 17)
            or body.get("first_col") != (0 if output_index % 2 == 0 else 3)
            or body.get("last_col") not in (
                {2} if output_index % 2 == 0 else {4, 5}
            )
        ):
            raise HarnessError("render_manifest_authored_page_map")


def validate_bundle(
    bundle_dir: Path,
    *,
    input_sha256: str,
    input_bytes: int,
    caps: Caps,
    dpi: int,
    expected_font_pack_sha256: str | None = None,
    require_single_page_print: bool = False,
    print_mode: str | None = None,
) -> Bundle:
    if require_single_page_print:
        if print_mode not in {None, PRINT_MODE_SINGLE_PAGE}:
            raise HarnessError("render_manifest_print_mode")
        print_mode = PRINT_MODE_SINGLE_PAGE
    if print_mode is not None and print_mode not in PRINT_MODES:
        raise HarnessError("render_manifest_print_mode")
    files = _bounded_directory_files(bundle_dir)
    total_bytes = sum(path.stat().st_size for path in files)
    if total_bytes > caps.max_artifact_bytes:
        raise HarnessError("renderer_artifact_limit")
    manifest_path = bundle_dir / "render-manifest.json"
    if manifest_path not in files:
        raise HarnessError("render_manifest_missing")
    try:
        manifest_bytes = manifest_path.read_bytes()
        if len(manifest_bytes) > min(caps.max_svg_bytes, caps.max_artifact_bytes):
            raise HarnessError("render_manifest_limit")
        manifest = json.loads(manifest_bytes)
    except json.JSONDecodeError as error:
        raise HarnessError("render_manifest_invalid_json") from error
    if not isinstance(manifest, dict) or manifest.get("schema") != RENDER_MANIFEST_SCHEMA:
        raise HarnessError("render_manifest_schema")
    source = manifest.get("source")
    if not isinstance(source, dict):
        raise HarnessError("render_manifest_source")
    if source.get("sha256") != input_sha256 or source.get("bytes") != input_bytes:
        raise HarnessError("render_manifest_source_mismatch")
    renderer = manifest.get("renderer")
    if not isinstance(renderer, dict):
        raise HarnessError("render_manifest_renderer")
    safe_renderer = {
        "name": renderer.get("name"),
        "version": renderer.get("version"),
        "fixed_units_per_pixel": renderer.get("fixed_units_per_pixel"),
        "font_pack_sha256": renderer.get("font_pack_sha256"),
    }
    if safe_renderer["name"] != "rxls-render" or not isinstance(
        safe_renderer["version"], str
    ):
        raise HarnessError("render_manifest_renderer")
    if not re.fullmatch(r"[0-9A-Za-z.+-]{1,64}", safe_renderer["version"]):
        raise HarnessError("render_manifest_renderer")
    if safe_renderer["fixed_units_per_pixel"] != FIXED_UNITS_PER_PIXEL:
        raise HarnessError("render_manifest_renderer")
    font_pack_sha256 = safe_renderer["font_pack_sha256"]
    if font_pack_sha256 is not None and (
        not isinstance(font_pack_sha256, str)
        or not SHA256_RE.fullmatch(font_pack_sha256)
    ):
        raise HarnessError("render_manifest_font_pack")
    if font_pack_sha256 != expected_font_pack_sha256:
        raise HarnessError("render_manifest_font_pack_mismatch")

    rows = manifest.get("sheets")
    if not isinstance(rows, list):
        raise HarnessError("render_manifest_sheets")
    if len(rows) > caps.max_pages:
        raise HarnessError("renderer_page_limit")
    pages = []
    expected_files = {"render-manifest.json"}
    total_pixels = 0
    for index, row in enumerate(rows):
        if not isinstance(row, dict) or row.get("index") != index:
            raise HarnessError("render_manifest_sheet_order")
        sheet_name = row.get("name")
        if not isinstance(sheet_name, str):
            raise HarnessError("render_manifest_sheet_name")
        visibility = row.get("visibility")
        if visibility not in {"visible", "hidden", "very_hidden"}:
            raise HarnessError("render_manifest_visibility")
        filename = f"sheet-{index:04d}.svg"
        if row.get("file") != filename:
            raise HarnessError("render_manifest_filename")
        svg_path = bundle_dir / filename
        if svg_path not in files:
            raise HarnessError("render_svg_missing")
        expected_files.add(filename)
        svg = row.get("svg")
        if not isinstance(svg, dict):
            raise HarnessError("render_manifest_svg")
        size = svg_path.stat().st_size
        digest = _sha256_file(svg_path, caps.max_svg_bytes)
        if svg.get("bytes") != size or svg.get("sha256") != digest:
            raise HarnessError("render_manifest_svg_mismatch")
        scene = row.get("scene")
        if (
            not isinstance(scene, dict)
            or not isinstance(scene.get("sha256"), str)
            or not SHA256_RE.fullmatch(scene["sha256"])
        ):
            raise HarnessError("render_manifest_scene")
        width, height = inspect_svg(svg_path, dpi=dpi, max_svg_bytes=caps.max_svg_bytes)
        if print_mode is None:
            pixels = width * height
            if pixels > caps.max_page_pixels:
                raise HarnessError("renderer_page_pixel_limit")
            total_pixels += pixels
            if total_pixels > caps.max_total_pixels:
                raise HarnessError("renderer_total_pixel_limit")
        canvas = row.get("canvas")
        report = row.get("report")
        if not isinstance(canvas, dict) or not isinstance(report, dict):
            raise HarnessError("render_manifest_sheet_metadata")
        if not all(
            isinstance(canvas.get(key), int) and canvas[key] > 0
            for key in ("width_raw", "height_raw")
        ):
            raise HarnessError("render_manifest_canvas")
        report_schema = report.get("schema_version")
        if (
            report_schema not in {1, 2}
            or report.get("sheet_index") != index
            or report.get("sheet_name") != sheet_name
            or report.get("svg_bytes") != size
            or not isinstance(report.get("warnings"), list)
        ):
            raise HarnessError("render_manifest_report")
        if report_schema == 2:
            if report.get("font_pack_sha256") != font_pack_sha256:
                raise HarnessError("render_manifest_report_font_pack")
            font_faces = report.get("font_faces")
            if not isinstance(font_faces, list) or len(font_faces) > 128:
                raise HarnessError("render_manifest_report_font_faces")
            ordered_faces = []
            for face in font_faces:
                if not isinstance(face, dict) or set(face) != {
                    "source_pack_sha256",
                    "face_sha256",
                    "family",
                    "weight",
                    "italic",
                    "substituted",
                }:
                    raise HarnessError("render_manifest_report_font_face")
                if (
                    not isinstance(face["source_pack_sha256"], str)
                    or not SHA256_RE.fullmatch(face["source_pack_sha256"])
                    or not isinstance(face["face_sha256"], str)
                    or not SHA256_RE.fullmatch(face["face_sha256"])
                    or not isinstance(face["family"], str)
                    or not 0 < len(face["family"]) <= 512
                    or not isinstance(face["weight"], int)
                    or not 1 <= face["weight"] <= 1000
                    or not isinstance(face["italic"], bool)
                    or not isinstance(face["substituted"], bool)
                ):
                    raise HarnessError("render_manifest_report_font_face")
                ordered_faces.append(
                    (
                        face["source_pack_sha256"],
                        face["face_sha256"],
                        face["family"],
                        face["weight"],
                        face["italic"],
                    )
                )
            if ordered_faces != sorted(set(ordered_faces)):
                raise HarnessError("render_manifest_report_font_face_order")
            if font_pack_sha256 is None and font_faces:
                raise HarnessError("render_manifest_report_font_faces_without_pack")
        warning_evidence = _manifest_warning_evidence(
            report["warnings"], code="render_manifest_warning"
        )

        if print_mode is None:
            pages.append(
                BundlePage(
                    len(pages),
                    visibility,
                    svg_path,
                    width,
                    height,
                    scene["sha256"],
                    warning_evidence,
                )
            )
            continue

        print_bundle = row.get("print")
        expected_print_keys = {
            "page_count",
            "page_scenes",
            "pdf",
            "png_dpi",
            "png_pages",
            "report",
            "schema",
            "svg_pages",
        }
        if print_mode == PRINT_MODE_SINGLE_PAGE:
            expected_print_keys.add("layout_override")
        if not isinstance(print_bundle, dict) or set(print_bundle) != expected_print_keys:
            raise HarnessError("render_manifest_print")
        page_count = print_bundle.get("page_count")
        if (
            print_bundle.get("schema") != "rxls.render.print-bundle.v1"
            or not isinstance(page_count, int)
            or isinstance(page_count, bool)
            or not 1 <= page_count <= caps.max_pages
            or print_bundle.get("pdf") is not None
            or print_bundle.get("png_dpi") is not None
            or print_bundle.get("png_pages") != []
        ):
            raise HarnessError("render_manifest_print")
        if (
            print_mode == PRINT_MODE_SINGLE_PAGE
            and (
                print_bundle.get("layout_override") != "single_page_sheets"
                or page_count != 1
            )
        ):
            raise HarnessError("render_manifest_print")
        if len(pages) + page_count > caps.max_pages:
            raise HarnessError("renderer_page_limit")

        report_filename = f"sheet-{index:04d}-pages.json"
        print_report_path = _validate_bundle_artifact(
            print_bundle.get("report"),
            expected_file=report_filename,
            bundle_dir=bundle_dir,
            files=files,
            max_bytes=min(caps.max_svg_bytes, caps.max_artifact_bytes),
            code="render_manifest_print_report",
        )
        expected_files.add(report_filename)
        try:
            print_report = json.loads(print_report_path.read_bytes())
        except json.JSONDecodeError as error:
            raise HarnessError("render_manifest_print_report") from error
        print_schema_version = (
            print_report.get("schema_version")
            if isinstance(print_report, dict)
            else None
        )
        print_pages = print_report.get("pages") if isinstance(print_report, dict) else None
        expected_override = (
            "single_page_sheets" if print_mode == PRINT_MODE_SINGLE_PAGE else None
        )
        if (
            not isinstance(print_report, dict)
            or print_schema_version not in ({1, 2} if print_mode == PRINT_MODE_SINGLE_PAGE else {2})
            or print_report.get("sheet_index") != index
            or print_report.get("sheet_name") != sheet_name
            or print_report.get("layout_override") != expected_override
            or (print_mode == PRINT_MODE_AUTHORED and "layout_override" in print_report)
            or not isinstance(print_pages, list)
            or len(print_pages) != page_count
        ):
            raise HarnessError("render_manifest_print_report")
        if print_schema_version == 2:
            source_reports = print_report.get("source_reports")
            if (
                not isinstance(source_reports, list)
                or not source_reports
                or source_reports[0] != print_report.get("source_report")
            ):
                raise HarnessError("render_manifest_print_report")
        if print_mode == PRINT_MODE_AUTHORED:
            _validate_authored_print_report(print_report, page_count)
        print_warnings = _manifest_warning_evidence(
            print_report.get("warnings"), code="render_manifest_print_warning"
        )
        page_scenes = print_bundle.get("page_scenes")
        page_artifacts = print_bundle.get("svg_pages")
        if (
            not isinstance(page_scenes, list)
            or len(page_scenes) != page_count
            or not isinstance(page_artifacts, list)
            or len(page_artifacts) != page_count
        ):
            raise HarnessError("render_manifest_print_pages")
        for page_index in range(page_count):
            page_scene = page_scenes[page_index]
            if (
                not isinstance(page_scene, dict)
                or set(page_scene) != {"index", "sha256"}
                or page_scene.get("index") != page_index
                or not isinstance(page_scene.get("sha256"), str)
                or not SHA256_RE.fullmatch(page_scene["sha256"])
            ):
                raise HarnessError("render_manifest_print_scene")
            page_filename = f"sheet-{index:04d}-pages/page-{page_index + 1:04d}.svg"
            page_path = _validate_bundle_artifact(
                page_artifacts[page_index],
                expected_file=page_filename,
                bundle_dir=bundle_dir,
                files=files,
                max_bytes=caps.max_svg_bytes,
                code="render_manifest_print_svg",
            )
            expected_files.add(page_filename)
            page_width, page_height = inspect_svg(
                page_path, dpi=dpi, max_svg_bytes=caps.max_svg_bytes
            )
            pixels = page_width * page_height
            if pixels > caps.max_page_pixels:
                raise HarnessError("renderer_page_pixel_limit")
            total_pixels += pixels
            if total_pixels > caps.max_total_pixels:
                raise HarnessError("renderer_total_pixel_limit")
            pages.append(
                BundlePage(
                    len(pages),
                    visibility,
                    page_path,
                    page_width,
                    page_height,
                    page_scene["sha256"],
                    tuple((*warning_evidence, *print_warnings)),
                )
            )
    actual_relative = {path.relative_to(bundle_dir).as_posix() for path in files}
    if actual_relative != expected_files:
        raise HarnessError("renderer_unexpected_artifact")
    return Bundle(tuple(pages), safe_renderer, total_bytes)


def build_rxls_command(
    base: Sequence[str],
    input_path: Path,
    output_dir: Path,
    font_pack_manifest: Path | None = None,
    print_mode: str = PRINT_MODE_SINGLE_PAGE,
) -> list[str]:
    if print_mode not in PRINT_MODES:
        raise HarnessError("print_mode")
    command = [*base, "bundle", str(input_path)]
    if font_pack_manifest is not None:
        command.extend(("--font-pack-manifest", str(font_pack_manifest)))
    if print_mode == PRINT_MODE_SINGLE_PAGE:
        command.append("--single-page-sheets")
    else:
        command.extend(("--print-layout", "--print-backends", "svg"))
    command.extend(("--output-dir", str(output_dir)))
    return command


def build_libreoffice_command(
    executable: str,
    input_path: Path,
    output_dir: Path,
    profile_dir: Path,
    print_mode: str = PRINT_MODE_SINGLE_PAGE,
) -> list[str]:
    if print_mode not in PRINT_MODES:
        raise HarnessError("print_mode")
    return [
        executable,
        f"-env:UserInstallation={profile_dir.resolve().as_uri()}",
        "--headless",
        "--nologo",
        "--nodefault",
        "--nolockcheck",
        "--norestore",
        "--convert-to",
        PDF_FILTER if print_mode == PRINT_MODE_SINGLE_PAGE else AUTHORED_PDF_FILTER,
        "--outdir",
        str(output_dir),
        str(input_path),
    ]


def build_libreoffice_oracle_command(
    template: Sequence[str],
    input_path: Path,
    output_dir: Path,
    run_id: str,
    font_pack: "FontPack | None",
    print_mode: str = PRINT_MODE_SINGLE_PAGE,
) -> list[str]:
    """Expand a bounded external-oracle adapter command without a shell.

    This permits the parity harness to route LibreOffice export through the
    locked, offline container wrapper. The adapter owns the output directory
    and must leave exactly one PDF beneath it; the ordinary artifact validator
    still checks the complete directory afterward.
    """
    if not re.fullmatch(r"[a-z0-9](?:[a-z0-9-]{0,30}[a-z0-9])?", run_id):
        raise HarnessError("libreoffice_command_run_id")
    joined = "\0".join(template)
    for required in ("{input}", "{output_dir}", "{run_id}"):
        if required not in joined:
            raise HarnessError("libreoffice_command_placeholder")
    if font_pack is None:
        raise HarnessError("libreoffice_command_font_pack_required")
    if print_mode not in PRINT_MODES:
        raise HarnessError("print_mode")
    if "{font_pack}" not in joined:
        raise HarnessError("libreoffice_command_placeholder")
    allowed = {"{input}", "{output_dir}", "{run_id}", "{font_pack}"}
    for token in template:
        scrubbed = token
        for placeholder in allowed:
            scrubbed = scrubbed.replace(placeholder, "")
        if "{" in scrubbed or "}" in scrubbed:
            raise HarnessError("libreoffice_command_placeholder")
    replacements = {
        "{input}": str(input_path),
        "{output_dir}": str(output_dir),
        "{run_id}": run_id,
        "{font_pack}": str(font_pack.root),
    }
    command = [
        _replace_placeholders(token, replacements)
        for token in template
    ]
    command.extend(("--print-mode", print_mode))
    return command


def _replace_placeholders(token: str, replacements: dict[str, str]) -> str:
    for placeholder, value in replacements.items():
        token = token.replace(placeholder, value)
    return token


def _reject_pathful_adapter_evidence(value: object) -> None:
    if isinstance(value, dict):
        for item in value.values():
            _reject_pathful_adapter_evidence(item)
    elif isinstance(value, list):
        for item in value:
            _reject_pathful_adapter_evidence(item)
    elif isinstance(value, str):
        lowered = value.lower()
        if (
            value.startswith("/")
            or lowered.startswith("file://")
            or re.match(r"[A-Za-z]:[\\/]", value)
        ):
            raise HarnessError("libreoffice_adapter_host_path")


def validate_libreoffice_adapter_output(
    root: Path,
    *,
    input_sha256: str,
    input_bytes: int,
    extension: str,
    font_pack_sha256: str,
    print_mode: str = PRINT_MODE_SINGLE_PAGE,
) -> dict[str, object]:
    """Validate the complete path-neutral container adapter evidence contract."""
    files = _bounded_directory_files(root)
    relative = [path.relative_to(root).as_posix() for path in files]
    if relative != ["execution.json", "oracle-manifest.json", "oracle.pdf"]:
        raise HarnessError("libreoffice_adapter_file_set")
    manifest = _bounded_json_object(
        root / "oracle-manifest.json", schema=CONTAINER_OUTPUT_SCHEMA
    )
    execution = _bounded_json_object(
        root / "execution.json", schema=CONTAINER_EXECUTION_SCHEMA
    )
    _exact_keys(
        manifest,
        {
            "artifact",
            "export",
            "font_pack_sha256",
            "lock_sha256",
            "oracle",
            "schema",
            "source",
        },
        "libreoffice_adapter_manifest_keys",
    )
    _exact_keys(
        execution,
        {
            "artifacts",
            "font_pack_sha256",
            "image",
            "isolation",
            "limits",
            "lock_file_sha256",
            "runtime",
            "schema",
            "source",
        },
        "libreoffice_adapter_execution_keys",
    )
    expected_source = {
        "bytes": input_bytes,
        "path": f"source/input{extension}",
        "sha256": input_sha256,
    }
    if manifest.get("source") != expected_source or execution.get("source") != expected_source:
        raise HarnessError("libreoffice_adapter_source_identity")
    if (
        manifest.get("font_pack_sha256") != font_pack_sha256
        or execution.get("font_pack_sha256") != font_pack_sha256
    ):
        raise HarnessError("libreoffice_adapter_font_pack_identity")
    lock_sha256 = _locked_sha256(
        manifest.get("lock_sha256"), "libreoffice_adapter_lock_identity"
    )
    lock_file_sha256 = _locked_sha256(
        execution.get("lock_file_sha256"),
        "libreoffice_adapter_lock_file_identity",
    )
    oracle = manifest.get("oracle")
    if oracle != {
        "artifact_sha256": CONTAINER_LIBREOFFICE_ARTIFACT_SHA256,
        "name": "LibreOffice",
        "version": "26.2.3.2",
    }:
        raise HarnessError("libreoffice_adapter_oracle_identity")
    if print_mode not in PRINT_MODES:
        raise HarnessError("print_mode")
    if manifest.get("export") != {
        "filter": "calc_pdf_Export",
        "single_page_sheets": print_mode == PRINT_MODE_SINGLE_PAGE,
    }:
        raise HarnessError("libreoffice_adapter_export_contract")
    image = _exact_keys(
        execution.get("image"),
        {"architecture", "expected_id", "id", "identity_status", "lock_sha256"},
        "libreoffice_adapter_image_keys",
    )
    image_id = image.get("id")
    expected_image_id = image.get("expected_id")
    if not isinstance(image_id, str) or not re.fullmatch(
        r"sha256:[0-9a-f]{64}", image_id
    ):
        raise HarnessError("libreoffice_adapter_image_identity")
    if expected_image_id is not None and expected_image_id != image_id:
        raise HarnessError("libreoffice_adapter_image_identity")
    expected_status = "pinned_match" if expected_image_id is not None else "runtime_verified"
    if (
        image.get("architecture") != "linux/amd64"
        or image.get("lock_sha256") != lock_sha256
        or image.get("identity_status") != expected_status
    ):
        raise HarnessError("libreoffice_adapter_image_identity")
    if execution.get("runtime") not in {"docker", "podman"}:
        raise HarnessError("libreoffice_adapter_runtime")
    isolation = _exact_keys(
        execution.get("isolation"),
        {
            "capabilities",
            "corpus_mount",
            "evidence_mount",
            "external_links",
            "font_mount",
            "macro_execution",
            "network",
            "no_new_privileges",
            "root_filesystem",
            "source_mount",
            "unique_home_xdg_profile",
        },
        "libreoffice_adapter_isolation_keys",
    )
    if any(
        isolation.get(key) != value
        for key, value in {
            "capabilities": "none",
            "corpus_mount": "read_only",
            "evidence_mount": "size_capped_tmpfs",
            "external_links": "network_and_filesystem_isolated",
            "font_mount": "read_only",
            "macro_execution": "disabled",
            "network": "none",
            "no_new_privileges": True,
            "root_filesystem": "read_only",
            "source_mount": "read_only",
            "unique_home_xdg_profile": True,
        }.items()
    ):
        raise HarnessError("libreoffice_adapter_isolation")
    limits = _exact_keys(
        execution.get("limits"),
        {
            "cpus",
            "evidence_bytes",
            "memory_bytes",
            "nofile",
            "pids",
            "timeout_milliseconds",
        },
        "libreoffice_adapter_limits_keys",
    )
    if (
        not isinstance(limits.get("cpus"), str)
        or not re.fullmatch(r"(?:[0-9]|1[0-6])\.[0-9]{2}", limits["cpus"])
        or any(
            not isinstance(limits.get(key), int)
            or isinstance(limits.get(key), bool)
            or int(limits[key]) <= 0
            for key in (
                "evidence_bytes",
                "memory_bytes",
                "nofile",
                "pids",
                "timeout_milliseconds",
            )
        )
    ):
        raise HarnessError("libreoffice_adapter_limits")
    artifact = _exact_keys(
        manifest.get("artifact"),
        {"bytes", "path", "sha256"},
        "libreoffice_adapter_artifact",
    )
    pdf = root / "oracle.pdf"
    try:
        pdf_bytes = pdf.stat().st_size
        with pdf.open("rb") as source:
            if pdf_bytes < 5 or source.read(5) != b"%PDF-":
                raise HarnessError("libreoffice_adapter_pdf")
    except OSError as error:
        raise HarnessError("libreoffice_adapter_pdf") from error
    if (
        artifact.get("path") != "oracle/oracle.pdf"
        or artifact.get("bytes") != pdf_bytes
        or artifact.get("sha256") != _sha256_file(pdf, max(pdf_bytes, 1))
        or execution.get("artifacts")
        != {"manifest": "oracle/oracle-manifest.json", "pdf": artifact}
    ):
        raise HarnessError("libreoffice_adapter_artifact")
    _reject_pathful_adapter_evidence(manifest)
    _reject_pathful_adapter_evidence(execution)
    return {
        "font_pack_sha256": font_pack_sha256,
        "image": {
            "architecture": "linux/amd64",
            "expected_id": expected_image_id,
            "id": image_id,
            "identity_status": expected_status,
        },
        "lock_sha256": lock_sha256,
        "lock_file_sha256": lock_file_sha256,
        "oracle": oracle,
        "runtime": execution["runtime"],
        "schema": CONTAINER_EXECUTION_SCHEMA,
    }


def _container_identity_from_adapter(
    adapter: object,
    *,
    font_pack_sha256: str,
    pdffonts_identity: object,
) -> dict[str, object]:
    """Normalize one validated adapter row into a path/content-neutral identity."""
    row = _exact_keys(
        adapter,
        {
            "font_pack_sha256",
            "image",
            "lock_file_sha256",
            "lock_sha256",
            "oracle",
            "runtime",
            "schema",
        },
        "libreoffice_adapter_identity_keys",
    )
    if (
        row.get("schema") != CONTAINER_EXECUTION_SCHEMA
        or row.get("font_pack_sha256") != font_pack_sha256
    ):
        raise HarnessError("libreoffice_adapter_aggregate_identity")
    image = _exact_keys(
        row.get("image"),
        {"architecture", "expected_id", "id", "identity_status"},
        "libreoffice_adapter_aggregate_image",
    )
    image_id = image.get("id")
    expected_image_id = image.get("expected_id")
    if (
        image.get("architecture") != "linux/amd64"
        or not isinstance(image_id, str)
        or re.fullmatch(r"sha256:[0-9a-f]{64}", image_id) is None
        or (
            expected_image_id is not None
            and (
                expected_image_id != image_id
                or image.get("identity_status") != "pinned_match"
            )
        )
        or (
            expected_image_id is None
            and image.get("identity_status") != "runtime_verified"
        )
    ):
        raise HarnessError("libreoffice_adapter_aggregate_image")
    oracle = _exact_keys(
        row.get("oracle"),
        {"artifact_sha256", "name", "version"},
        "libreoffice_adapter_aggregate_oracle",
    )
    if (
        oracle.get("artifact_sha256") != CONTAINER_LIBREOFFICE_ARTIFACT_SHA256
        or oracle.get("name") != "LibreOffice"
        or oracle.get("version") != "26.2.3.2"
    ):
        raise HarnessError("libreoffice_adapter_aggregate_oracle")
    inspector = _exact_keys(
        pdffonts_identity,
        {"kind", "pdffonts_sha256"},
        "pdffonts_binary_identity_required",
    )
    if inspector.get("kind") != "poppler":
        raise HarnessError("pdffonts_binary_identity")
    pdffonts_sha256 = _locked_sha256(
        inspector.get("pdffonts_sha256"), "pdffonts_binary_identity"
    )
    runtime = row.get("runtime")
    if runtime not in {"docker", "podman"}:
        raise HarnessError("libreoffice_adapter_aggregate_runtime")
    return {
        "build_contract_sha256": _locked_sha256(
            row.get("lock_sha256"), "libreoffice_adapter_aggregate_lock"
        ),
        "font_pack_sha256": font_pack_sha256,
        "image": {
            "architecture": "linux/amd64",
            "config_digest": image_id,
            "expected_config_digest": expected_image_id,
            "identity_status": image["identity_status"],
        },
        "libreoffice": dict(oracle),
        "lock_file_sha256": _locked_sha256(
            row.get("lock_file_sha256"),
            "libreoffice_adapter_aggregate_lock_file",
        ),
        "pdf_font_inspector": {
            "kind": "poppler",
            "pdffonts_sha256": pdffonts_sha256,
        },
        "runtime": runtime,
        "schema": CONTAINER_IDENTITY_SCHEMA,
    }


def aggregate_container_oracle_identity(
    results: Sequence[dict[str, object]],
    *,
    config: HarnessConfig,
) -> dict[str, object] | None:
    """Require every comparable adapter result to share one exact oracle identity."""
    if config.libreoffice_command is None:
        return None
    if config.font_pack is None:
        raise HarnessError("libreoffice_adapter_font_pack_identity")
    font_pack_sha256 = _locked_sha256(
        config.font_pack.evidence.get("pack_sha256"),
        "libreoffice_adapter_font_pack_identity",
    )
    normalized: list[dict[str, object]] = []
    for result in results:
        adapter = result.get("oracle_adapter")
        comparable = result.get("status") in {"compared", "different"}
        if comparable and adapter is None:
            raise HarnessError("libreoffice_adapter_identity_missing")
        if adapter is not None:
            normalized.append(
                _container_identity_from_adapter(
                    adapter,
                    font_pack_sha256=font_pack_sha256,
                    pdffonts_identity=config.pdffonts_identity,
                )
            )
    if not normalized:
        if config.dry_run:
            return None
        raise HarnessError("libreoffice_adapter_identity_missing")
    first = normalized[0]
    expected = _canonical_json_bytes(first)
    if any(_canonical_json_bytes(row) != expected for row in normalized[1:]):
        raise HarnessError("libreoffice_adapter_identity_mixed")
    return first


def seed_libreoffice_profile(profile_dir: Path) -> str:
    """Seed one clean profile with the tracked active-content/recalc policy."""
    try:
        payload = ORACLE_PROFILE_PATH.read_bytes()
    except OSError as error:
        raise HarnessError("oracle_profile_unreadable") from error
    if not payload or len(payload) > 64 * 1024:
        raise HarnessError("oracle_profile_invalid")
    digest = hashlib.sha256(payload).hexdigest()
    user = profile_dir / "user"
    if profile_dir.is_symlink() or user.is_symlink():
        raise HarnessError("oracle_profile_unsafe")
    user.mkdir(parents=True, exist_ok=True)
    target = user / "registrymodifications.xcu"
    if target.exists() or target.is_symlink():
        raise HarnessError("oracle_profile_unsafe")
    target.write_bytes(payload)
    return digest


def _substitute_command(
    template: Sequence[str],
    *,
    input_path: Path,
    output_path: Path,
    width: int,
    height: int,
    dpi: int,
) -> list[str]:
    replacements = {
        "{input}": str(input_path),
        "{output}": str(output_path),
        "{width}": str(width),
        "{height}": str(height),
        "{dpi}": str(dpi),
    }
    rendered = []
    for token in template:
        for needle, value in replacements.items():
            token = token.replace(needle, value)
        rendered.append(token)
    return rendered


CAIROSVG_WORKER = r"""
import sys
import cairosvg
cairosvg.svg2png(
    url=sys.argv[1],
    write_to=sys.argv[2],
    output_width=int(sys.argv[3]),
    output_height=int(sys.argv[4]),
)
"""


PYMUPDF_WORKER = r"""
import json
from decimal import Decimal, ROUND_CEILING
from pathlib import Path
import sys
import fitz

source = sys.argv[1]
target = Path(sys.argv[2])
dpi = int(sys.argv[3])
max_pages = int(sys.argv[4])
max_page_pixels = int(sys.argv[5])
max_total_pixels = int(sys.argv[6])
max_output_bytes = int(sys.argv[7])

def fail(code, exit_code):
    print(json.dumps({"code": code}, sort_keys=True, separators=(",", ":")))
    raise SystemExit(exit_code)

document = fitz.open(source)
if len(document) > max_pages:
    fail("libreoffice_page_limit", 20)
target.mkdir(parents=True, exist_ok=True)
pages = []
total_pixels = 0
total_bytes = 0
for index, page in enumerate(document):
    width_value = Decimal(str(page.rect.width)) * dpi / 72
    height_value = Decimal(str(page.rect.height)) * dpi / 72
    width = int(width_value.to_integral_value(rounding=ROUND_CEILING))
    height = int(height_value.to_integral_value(rounding=ROUND_CEILING))
    if width <= 0 or height <= 0 or width * height > max_page_pixels:
        fail("libreoffice_page_pixel_limit", 21)
    total_pixels += width * height
    if total_pixels > max_total_pixels:
        fail("libreoffice_total_pixel_limit", 22)
    pixmap = page.get_pixmap(dpi=dpi, alpha=False)
    if pixmap.width * pixmap.height > max_page_pixels:
        fail("libreoffice_page_pixel_limit", 21)
    path = target / f"page-{index:04d}.png"
    pixmap.save(path)
    total_bytes += path.stat().st_size
    if total_bytes > max_output_bytes:
        fail("libreoffice_raster_output_limit", 23)
    pages.append({"file": path.name, "width": pixmap.width, "height": pixmap.height})
print(json.dumps({"pages": pages}, sort_keys=True, separators=(",", ":")))
"""


def _command_failure(prefix: str, result: CommandResult) -> str | None:
    if result.status == "ok":
        return None
    suffix = {
        "not_found": "not_found",
        "timeout": "timeout",
        "output_limit": "command_output_limit",
        "nonzero": "failed",
    }.get(result.status, "failed")
    return f"{prefix}_{suffix}"


def _run_svg_rasterizer(
    page: BundlePage,
    output: Path,
    *,
    config: HarnessConfig,
    backends: Backends,
    runner: CommandRunner,
    cwd: Path,
    env: dict[str, str],
) -> CommandResult:
    if backends.svg_command is not None:
        command = _substitute_command(
            backends.svg_command,
            input_path=page.svg_path,
            output_path=output,
            width=page.width_pixels,
            height=page.height_pixels,
            dpi=config.dpi,
        )
    else:
        command = [
            sys.executable,
            "-c",
            CAIROSVG_WORKER,
            str(page.svg_path),
            str(output),
            str(page.width_pixels),
            str(page.height_pixels),
        ]
    return runner.run(
        command,
        cwd=cwd,
        env=env,
        timeout_seconds=config.caps.timeout_seconds,
        output_limit_bytes=config.caps.max_command_output_bytes,
    )


def _run_pdf_rasterizer(
    pdf: Path,
    output_dir: Path,
    *,
    config: HarnessConfig,
    backends: Backends,
    runner: CommandRunner,
    cwd: Path,
    env: dict[str, str],
) -> tuple[CommandResult, dict[str, object] | None]:
    if not backends.pymupdf and backends.pdftoppm and backends.pdfinfo:
        return _run_poppler_pdf_rasterizer(
            pdf,
            output_dir,
            config=config,
            runner=runner,
            cwd=cwd,
            env=env,
        )
    command = [
        sys.executable,
        "-c",
        PYMUPDF_WORKER,
        str(pdf),
        str(output_dir),
        str(config.dpi),
        str(config.caps.max_pages),
        str(config.caps.max_page_pixels),
        str(config.caps.max_total_pixels),
        str(config.caps.max_artifact_bytes),
    ]
    result = runner.run(
        command,
        cwd=cwd,
        env=env,
        timeout_seconds=config.caps.timeout_seconds,
        output_limit_bytes=config.caps.max_command_output_bytes,
    )
    if result.status != "ok":
        return result, None
    try:
        document = json.loads(result.stdout)
    except json.JSONDecodeError:
        return CommandResult("nonzero", 24, result.stdout, result.stderr), None
    if not isinstance(document, dict) or not isinstance(document.get("pages"), list):
        return CommandResult("nonzero", 24, result.stdout, result.stderr), None
    return result, document


PDFINFO_PAGES_RE = re.compile(r"^Pages:\s+(\d+)\s*$", re.MULTILINE)
PDFINFO_SIZE_RE = re.compile(
    r"^Page(?:\s+\d+)?\s+size:\s+"
    r"([0-9]+(?:\.[0-9]+)?)\s+x\s+([0-9]+(?:\.[0-9]+)?)\s+pts\s*$",
    re.MULTILINE,
)


def parse_pdfinfo(
    text: str, *, require_all_sizes: bool
) -> tuple[int, list[tuple[Fraction, Fraction]]]:
    pages_match = PDFINFO_PAGES_RE.search(text)
    if pages_match is None:
        raise HarnessError("pdfinfo_pages_missing")
    pages = int(pages_match.group(1))
    sizes = [
        (Fraction(width), Fraction(height))
        for width, height in PDFINFO_SIZE_RE.findall(text)
    ]
    if pages <= 0:
        raise HarnessError("pdfinfo_pages_invalid")
    if require_all_sizes and len(sizes) != pages:
        raise HarnessError("pdfinfo_page_sizes_missing")
    return pages, sizes


def normalize_semantic_tokens(
    text: str,
    *,
    max_codepoints: int,
    max_tokens: int,
) -> tuple[str, ...]:
    """Normalize layout-only text differences without retaining cell content."""
    if len(text) > max_codepoints * 4:
        raise HarnessError("semantic_raw_codepoint_limit")
    normalized = unicodedata.normalize("NFC", text)
    normalized = "".join(
        character
        for character in normalized
        if character not in SEMANTIC_IGNORED_CODEPOINTS
    )
    tokens = tuple(normalized.split())
    if len(tokens) > max_tokens:
        raise HarnessError("semantic_token_limit")
    if sum(len(token) for token in tokens) > max_codepoints:
        raise HarnessError("semantic_codepoint_limit")
    return tokens


def _validated_svg_visible_label(
    element: ET.Element,
    *,
    max_raw_codepoints: int,
) -> tuple[str, str]:
    """Return bounded full/visible labels after validating their provenance."""
    aria_label = element.attrib.get("aria-label")
    if aria_label is None:
        raise HarnessError("semantic_svg_label_missing")
    visible_label = element.attrib.get("data-rxls-visible-label")
    if visible_label is None:
        raise HarnessError("semantic_svg_visible_label_missing")
    if len(visible_label) > max_raw_codepoints:
        raise HarnessError("semantic_svg_visible_label_unbounded")
    if any(
        unicodedata.category(character) in INVALID_VISIBLE_LABEL_CATEGORIES
        for character in visible_label
    ):
        raise HarnessError("semantic_svg_visible_label_control")
    if len(visible_label) > len(aria_label):
        raise HarnessError("semantic_svg_visible_label_length")

    # Rust emits the derivative by selecting Unicode scalar values from the
    # source label, with whitespace permitted between disjoint visible
    # clusters.  Validate that exact relationship without ASCII assumptions.
    source = iter(aria_label)
    for character in visible_label:
        if character.isspace():
            continue
        if not any(candidate == character for candidate in source):
            raise HarnessError("semantic_svg_visible_label_injection")
    return aria_label, visible_label


def _has_normalized_semantic_text(text: str) -> bool:
    """Match the normalization predicate used before transform validation."""
    normalized = unicodedata.normalize("NFC", text)
    normalized = "".join(
        character
        for character in normalized
        if character not in SEMANTIC_IGNORED_CODEPOINTS
    )
    return bool(normalized.split())


def _xml_local_name(tag: object) -> str:
    return tag.rsplit("}", 1)[-1] if isinstance(tag, str) else ""


def _parse_finite_number(value: object, code: str) -> float:
    if not isinstance(value, str) or not value or len(value) > 128:
        raise HarnessError(code)
    try:
        number = float(value)
    except ValueError as error:
        raise HarnessError(code) from error
    if not math.isfinite(number) or abs(number) > 1_000_000_000:
        raise HarnessError(code)
    return number


def _tokenize_svg_path(value: object, *, max_tokens: int) -> tuple[str, ...]:
    if not isinstance(value, str) or not value:
        raise HarnessError("semantic_svg_path_missing")
    tokens: list[str] = []
    previous_end = 0
    previous_token: str | None = None
    for match in SVG_PATH_TOKEN_RE.finditer(value):
        gap = value[previous_end : match.start()]
        if any(character not in " \t\r\n," for character in gap):
            raise HarnessError("semantic_svg_path_syntax")
        if gap.count(",") > 1:
            raise HarnessError("semantic_svg_path_syntax")
        token = match.group(0)
        if "," in gap and (
            previous_token is None
            or previous_token.isalpha()
            or token.isalpha()
        ):
            raise HarnessError("semantic_svg_path_syntax")
        tokens.append(token)
        if len(tokens) > max_tokens:
            raise HarnessError("semantic_svg_path_token_limit")
        previous_token = token
        previous_end = match.end()
    tail = value[previous_end:]
    if any(character not in " \t\r\n" for character in tail):
        raise HarnessError("semantic_svg_path_syntax")
    if not tokens:
        raise HarnessError("semantic_svg_path_syntax")
    return tuple(tokens)


def _quadratic_coordinate(p0: float, p1: float, p2: float, t: float) -> float:
    inverse = 1.0 - t
    return inverse * inverse * p0 + 2.0 * inverse * t * p1 + t * t * p2


def _cubic_coordinate(
    p0: float, p1: float, p2: float, p3: float, t: float
) -> float:
    inverse = 1.0 - t
    return (
        inverse * inverse * inverse * p0
        + 3.0 * inverse * inverse * t * p1
        + 3.0 * inverse * t * t * p2
        + t * t * t * p3
    )


def _quadratic_extrema(p0: float, p1: float, p2: float) -> tuple[float, ...]:
    denominator = p0 - 2.0 * p1 + p2
    if denominator == 0.0:
        return ()
    t = (p0 - p1) / denominator
    return (t,) if 0.0 < t < 1.0 else ()


def _cubic_extrema(
    p0: float, p1: float, p2: float, p3: float
) -> tuple[float, ...]:
    a = -p0 + 3.0 * p1 - 3.0 * p2 + p3
    b = 2.0 * (p0 - 2.0 * p1 + p2)
    c = p1 - p0
    scale = max(1.0, abs(a), abs(b), abs(c))
    epsilon = 1e-14 * scale
    if abs(a) <= epsilon:
        if abs(b) <= epsilon:
            return ()
        t = -c / b
        return (t,) if 0.0 < t < 1.0 else ()
    discriminant = b * b - 4.0 * a * c
    if discriminant < -epsilon:
        return ()
    root = math.sqrt(max(0.0, discriminant))
    values = []
    for t in ((-b - root) / (2.0 * a), (-b + root) / (2.0 * a)):
        if 0.0 < t < 1.0 and not any(math.isclose(t, item) for item in values):
            values.append(t)
    return tuple(values)


def _svg_path_bounds(
    value: object, *, max_tokens: int = MAX_SVG_PATH_TOKENS
) -> tuple[tuple[float, float, float, float] | None, int]:
    """Return exact Bézier bounds for the bounded SVG path subset we emit."""
    tokens = _tokenize_svg_path(value, max_tokens=max_tokens)
    index = 0
    command: str | None = None
    current = (0.0, 0.0)
    subpath = (0.0, 0.0)
    previous_kind = ""
    previous_cubic_control: tuple[float, float] | None = None
    previous_quadratic_control: tuple[float, float] | None = None
    bounds: list[float] | None = None

    def number() -> float:
        nonlocal index
        if index >= len(tokens) or tokens[index].isalpha():
            raise HarnessError("semantic_svg_path_arity")
        result = _parse_finite_number(tokens[index], "semantic_svg_path_number")
        index += 1
        return result

    def point(relative: bool) -> tuple[float, float]:
        x = number()
        y = number()
        if relative:
            return current[0] + x, current[1] + y
        return x, y

    def include(points: Sequence[tuple[float, float]]) -> None:
        nonlocal bounds
        for x, y in points:
            if not math.isfinite(x) or not math.isfinite(y):
                raise HarnessError("semantic_svg_path_number")
            if bounds is None:
                bounds = [x, y, x, y]
            else:
                bounds[0] = min(bounds[0], x)
                bounds[1] = min(bounds[1], y)
                bounds[2] = max(bounds[2], x)
                bounds[3] = max(bounds[3], y)

    def line_to(target: tuple[float, float]) -> None:
        nonlocal current
        include((current, target))
        current = target

    def quadratic_to(
        control: tuple[float, float], target: tuple[float, float]
    ) -> None:
        nonlocal current
        start = current
        points = [start, target]
        extrema = set(_quadratic_extrema(start[0], control[0], target[0]))
        extrema.update(_quadratic_extrema(start[1], control[1], target[1]))
        points.extend(
            (
                _quadratic_coordinate(start[0], control[0], target[0], t),
                _quadratic_coordinate(start[1], control[1], target[1], t),
            )
            for t in extrema
        )
        include(points)
        current = target

    def cubic_to(
        control1: tuple[float, float],
        control2: tuple[float, float],
        target: tuple[float, float],
    ) -> None:
        nonlocal current
        start = current
        points = [start, target]
        extrema = set(
            _cubic_extrema(start[0], control1[0], control2[0], target[0])
        )
        extrema.update(
            _cubic_extrema(start[1], control1[1], control2[1], target[1])
        )
        points.extend(
            (
                _cubic_coordinate(
                    start[0], control1[0], control2[0], target[0], t
                ),
                _cubic_coordinate(
                    start[1], control1[1], control2[1], target[1], t
                ),
            )
            for t in extrema
        )
        include(points)
        current = target

    while index < len(tokens):
        if tokens[index].isalpha():
            command = tokens[index]
            index += 1
            if command not in SVG_PATH_COMMANDS:
                raise HarnessError("semantic_svg_path_command")
        elif command is None or command in "Zz":
            raise HarnessError("semantic_svg_path_syntax")
        assert command is not None
        kind = command.upper()
        relative = command.islower()
        if kind == "Z":
            line_to(subpath)
            previous_kind = kind
            previous_cubic_control = None
            previous_quadratic_control = None
            command = None
            continue

        groups = 0
        while index < len(tokens) and not tokens[index].isalpha():
            groups += 1
            if kind == "M":
                target = point(relative)
                if groups == 1:
                    current = target
                    subpath = target
                else:
                    line_to(target)
            elif kind == "L":
                line_to(point(relative))
            elif kind == "H":
                x = number()
                line_to((current[0] + x if relative else x, current[1]))
            elif kind == "V":
                y = number()
                line_to((current[0], current[1] + y if relative else y))
            elif kind == "C":
                control1 = point(relative)
                control2 = point(relative)
                target = point(relative)
                cubic_to(control1, control2, target)
                previous_cubic_control = control2
            elif kind == "S":
                control1 = (
                    (
                        2.0 * current[0] - previous_cubic_control[0],
                        2.0 * current[1] - previous_cubic_control[1],
                    )
                    if previous_kind in {"C", "S"}
                    and previous_cubic_control is not None
                    else current
                )
                control2 = point(relative)
                target = point(relative)
                cubic_to(control1, control2, target)
                previous_cubic_control = control2
            elif kind == "Q":
                control = point(relative)
                target = point(relative)
                quadratic_to(control, target)
                previous_quadratic_control = control
            elif kind == "T":
                control = (
                    (
                        2.0 * current[0] - previous_quadratic_control[0],
                        2.0 * current[1] - previous_quadratic_control[1],
                    )
                    if previous_kind in {"Q", "T"}
                    and previous_quadratic_control is not None
                    else current
                )
                target = point(relative)
                quadratic_to(control, target)
                previous_quadratic_control = control
            else:  # pragma: no cover - guarded by SVG_PATH_COMMANDS
                raise HarnessError("semantic_svg_path_command")
            if kind not in {"C", "S"}:
                previous_cubic_control = None
            if kind not in {"Q", "T"}:
                previous_quadratic_control = None
            previous_kind = "L" if kind == "M" and groups > 1 else kind
        if groups == 0:
            raise HarnessError("semantic_svg_path_arity")
        if kind == "M":
            command = "l" if relative else "L"
    return (tuple(bounds) if bounds is not None else None), len(tokens)


def _svg_length_points(value: object) -> float:
    if not isinstance(value, str):
        raise HarnessError("semantic_svg_dimensions")
    match = SVG_LENGTH_RE.fullmatch(value)
    if match is None:
        raise HarnessError("semantic_svg_dimensions")
    magnitude = _parse_finite_number(match.group(1), "semantic_svg_dimensions")
    if magnitude <= 0:
        raise HarnessError("semantic_svg_dimensions")
    unit = (match.group(2) or "px").lower()
    factors = {
        "px": 72.0 / 96.0,
        "pt": 1.0,
        "pc": 12.0,
        "in": 72.0,
        "cm": 72.0 / 2.54,
        "mm": 72.0 / 25.4,
        "q": 72.0 / 101.6,
    }
    return magnitude * factors[unit]


def _svg_rect(element: ET.Element) -> tuple[float, float, float, float]:
    allowed = {"x", "y", "width", "height"}
    if any(_xml_local_name(name) not in allowed for name in element.attrib):
        raise HarnessError("semantic_svg_clip")
    x = _parse_finite_number(element.attrib.get("x", "0"), "semantic_svg_clip")
    y = _parse_finite_number(element.attrib.get("y", "0"), "semantic_svg_clip")
    width = _parse_finite_number(element.attrib.get("width"), "semantic_svg_clip")
    height = _parse_finite_number(element.attrib.get("height"), "semantic_svg_clip")
    if width <= 0 or height <= 0:
        raise HarnessError("semantic_svg_clip")
    return x, y, x + width, y + height


def extract_svg_semantic_evidence(
    path: Path,
    *,
    max_svg_bytes: int,
    max_codepoints: int,
    max_tokens: int,
    max_path_tokens: int = MAX_SVG_PATH_TOKENS,
) -> SvgSemanticEvidence:
    """Extract transient labels and clipped glyph-outline boxes in PDF points."""
    try:
        payload = path.read_bytes()
    except OSError as error:
        raise HarnessError("semantic_svg_unreadable") from error
    if len(payload) > max_svg_bytes:
        raise HarnessError("svg_output_limit")
    upper = payload.upper()
    if b"<!DOCTYPE" in upper or b"<!ENTITY" in upper:
        raise HarnessError("semantic_svg_unsafe_markup")
    try:
        text = payload.decode("utf-8", "strict")
        if "\x00" in text:
            raise HarnessError("semantic_svg_invalid_utf8")
        root = ET.fromstring(text)
    except UnicodeDecodeError as error:
        raise HarnessError("semantic_svg_invalid_utf8") from error
    except ET.ParseError as error:
        raise HarnessError("semantic_svg_invalid_xml") from error
    if _xml_local_name(root.tag) != "svg":
        raise HarnessError("semantic_svg_root")

    view_box = root.attrib.get("viewBox")
    if not isinstance(view_box, str):
        raise HarnessError("semantic_svg_viewbox")
    pieces = view_box.replace(",", " ").split()
    if len(pieces) != 4:
        raise HarnessError("semantic_svg_viewbox")
    minimum_x, minimum_y, view_width, view_height = (
        _parse_finite_number(piece, "semantic_svg_viewbox") for piece in pieces
    )
    if view_width <= 0 or view_height <= 0:
        raise HarnessError("semantic_svg_viewbox")
    page_width_points = _svg_length_points(root.attrib.get("width"))
    page_height_points = _svg_length_points(root.attrib.get("height"))
    scale_x = page_width_points / view_width
    scale_y = page_height_points / view_height
    if not math.isclose(scale_x, scale_y, rel_tol=1e-9, abs_tol=1e-9):
        raise HarnessError("semantic_svg_aspect_ratio")

    parents = {
        child: parent for parent in root.iter() for child in list(parent)
    }

    def has_transform(element: ET.Element) -> bool:
        return any(
            _xml_local_name(name) == "transform" for name in element.attrib
        )

    clips: dict[str, tuple[float, float, float, float]] = {}
    for element in root.iter():
        if _xml_local_name(element.tag) != "clipPath":
            continue
        identifier = element.attrib.get("id")
        units = element.attrib.get("clipPathUnits", "userSpaceOnUse")
        if (
            not isinstance(identifier, str)
            or not identifier
            or identifier in clips
            or units != "userSpaceOnUse"
            or any(
                _xml_local_name(name) not in {"id", "clipPathUnits"}
                for name in element.attrib
            )
            or has_transform(element)
        ):
            raise HarnessError("semantic_svg_clip")
        children = list(element)
        if len(children) != 1 or _xml_local_name(children[0].tag) != "rect":
            raise HarnessError("semantic_svg_clip")
        clips[identifier] = _svg_rect(children[0])

    all_tokens: list[str] = []
    boxes: list[SemanticTextBox] = []
    unbounded = 0
    raw_codepoints = 0
    visible_raw_codepoints = 0
    semantic_codepoints_used = 0
    path_tokens_used = 0
    raw_codepoint_limit = max_codepoints * 4
    for element in root.iter():
        if element.attrib.get("role") != "text":
            continue
        aria_label, visible_label = _validated_svg_visible_label(
            element,
            max_raw_codepoints=raw_codepoint_limit,
        )
        visible_raw_codepoints += len(visible_label)
        if visible_raw_codepoints > raw_codepoint_limit:
            raise HarnessError("semantic_svg_visible_label_unbounded")
        raw_codepoints += len(aria_label)
        if raw_codepoints > raw_codepoint_limit:
            raise HarnessError("semantic_raw_codepoint_limit")
        label_tokens = normalize_semantic_tokens(
            visible_label,
            max_codepoints=max_codepoints - semantic_codepoints_used,
            max_tokens=max_tokens - len(all_tokens),
        )
        all_tokens.extend(label_tokens)
        semantic_codepoints_used += sum(len(token) for token in label_tokens)
        if label_tokens or _has_normalized_semantic_text(aria_label):
            ancestor: ET.Element | None = element
            while ancestor is not None:
                if has_transform(ancestor):
                    raise HarnessError("semantic_svg_text_transform")
                ancestor = parents.get(ancestor)
            if any(has_transform(child) for child in element.iter()):
                raise HarnessError("semantic_svg_text_transform")
        if not label_tokens:
            continue
        clip_value = element.attrib.get("clip-path")
        clip: tuple[float, float, float, float] | None = None
        if clip_value is not None:
            match = SVG_CLIP_REFERENCE_RE.fullmatch(clip_value)
            if match is None or match.group(2) not in clips:
                raise HarnessError("semantic_svg_clip_reference")
            clip = clips[match.group(2)]

        box: list[float] | None = None
        path_count = 0
        for child in element.iter():
            if child is element or _xml_local_name(child.tag) != "path":
                continue
            path_count += 1
            remaining = max_path_tokens - path_tokens_used
            if remaining <= 0:
                raise HarnessError("semantic_svg_path_token_limit")
            child_bounds, consumed = _svg_path_bounds(
                child.attrib.get("d"), max_tokens=remaining
            )
            path_tokens_used += consumed
            if child_bounds is None:
                continue
            if box is None:
                box = list(child_bounds)
            else:
                box[0] = min(box[0], child_bounds[0])
                box[1] = min(box[1], child_bounds[1])
                box[2] = max(box[2], child_bounds[2])
                box[3] = max(box[3], child_bounds[3])
        if path_count == 0 or box is None:
            unbounded += 1
            continue
        if clip is not None:
            box = [
                max(box[0], clip[0]),
                max(box[1], clip[1]),
                min(box[2], clip[2]),
                min(box[3], clip[3]),
            ]
        if box[2] <= box[0] or box[3] <= box[1]:
            unbounded += 1
            continue
        points = (
            (box[0] - minimum_x) * scale_x,
            (box[1] - minimum_y) * scale_y,
            (box[2] - minimum_x) * scale_x,
            (box[3] - minimum_y) * scale_y,
        )
        if not all(math.isfinite(value) for value in points):
            raise HarnessError("semantic_svg_box")
        boxes.append(SemanticTextBox(label_tokens, points))
    return SvgSemanticEvidence(
        tuple(all_tokens), tuple(boxes), unbounded, path_tokens_used
    )


def extract_svg_semantic_tokens(
    path: Path,
    *,
    max_svg_bytes: int,
    max_codepoints: int,
    max_tokens: int,
) -> tuple[str, ...]:
    """Extract ordered visible labels from path-outlined renderer text."""
    try:
        payload = path.read_bytes()
    except OSError as error:
        raise HarnessError("semantic_svg_unreadable") from error
    if len(payload) > max_svg_bytes:
        raise HarnessError("svg_output_limit")
    upper = payload.upper()
    if b"<!DOCTYPE" in upper or b"<!ENTITY" in upper:
        raise HarnessError("semantic_svg_unsafe_markup")
    try:
        text = payload.decode("utf-8", "strict")
        if "\x00" in text:
            raise HarnessError("semantic_svg_invalid_utf8")
        root = ET.fromstring(text)
    except UnicodeDecodeError as error:
        raise HarnessError("semantic_svg_invalid_utf8") from error
    except ET.ParseError as error:
        raise HarnessError("semantic_svg_invalid_xml") from error
    all_tokens: list[str] = []
    raw_codepoints = 0
    visible_raw_codepoints = 0
    semantic_codepoints_used = 0
    raw_codepoint_limit = max_codepoints * 4
    for element in root.iter():
        if element.attrib.get("role") != "text":
            continue
        aria_label, visible_label = _validated_svg_visible_label(
            element,
            max_raw_codepoints=raw_codepoint_limit,
        )
        visible_raw_codepoints += len(visible_label)
        if visible_raw_codepoints > raw_codepoint_limit:
            raise HarnessError("semantic_svg_visible_label_unbounded")
        raw_codepoints += len(aria_label)
        if raw_codepoints > raw_codepoint_limit:
            raise HarnessError("semantic_raw_codepoint_limit")
        label_tokens = normalize_semantic_tokens(
            visible_label,
            max_codepoints=max_codepoints - semantic_codepoints_used,
            max_tokens=max_tokens - len(all_tokens),
        )
        all_tokens.extend(label_tokens)
        semantic_codepoints_used += sum(len(token) for token in label_tokens)
    return tuple(all_tokens)


def parse_pdffonts_output(
    payload: bytes,
    *,
    max_bytes: int,
    max_fonts: int = 4096,
) -> tuple[PdfFontRecord, ...]:
    """Parse the fixed pinned-Poppler table without retaining raw font names."""
    if (
        not payload
        or len(payload) > max_bytes
        or max_fonts < 0
        or b"\x00" in payload
        or b"\r" in payload
        or not payload.endswith(b"\n")
    ):
        raise HarnessError("pdffonts_output_contract")
    try:
        text = payload.decode("ascii", "strict")
    except UnicodeDecodeError as error:
        raise HarnessError("pdffonts_output_contract") from error
    lines = text.splitlines()
    if (
        len(lines) < 2
        or lines[0] != PDFFONTS_HEADER
        or lines[1] != PDFFONTS_SEPARATOR
        or len(lines) - 2 > max_fonts
    ):
        raise HarnessError("pdffonts_output_contract")

    records: list[PdfFontRecord] = []
    objects: set[tuple[int, int]] = set()
    for line in lines[2:]:
        if (
            len(line) != len(PDFFONTS_HEADER)
            or line[36] != " "
            or line[54] != " "
            or line[71] != " "
            or line[75] != " "
            or line[79] != " "
            or line[83] != " "
        ):
            raise HarnessError("pdffonts_row_contract")
        raw_name = line[0:36].rstrip(" ")
        font_type = line[37:54].rstrip(" ")
        encoding = line[55:71].rstrip(" ")
        embedded = line[72:75]
        subset = line[76:79]
        unicode_map = line[80:83]
        object_match = re.fullmatch(r" *([1-9][0-9]*) +([0-9]+)", line[84:93])
        if (
            not raw_name
            or raw_name != line[0 : len(raw_name)]
            or not PDF_FONT_NAME_RE.fullmatch(raw_name)
            or font_type not in PDF_FONT_TYPES
            or not PDF_FONT_ENCODING_RE.fullmatch(encoding)
            or embedded not in {"yes", "no "}
            or subset not in {"yes", "no "}
            or unicode_map not in {"yes", "no "}
            or object_match is None
        ):
            raise HarnessError("pdffonts_row_contract")
        object_number = int(object_match.group(1))
        generation = int(object_match.group(2))
        object_identity = (object_number, generation)
        if (
            object_number > 2**31 - 1
            or generation > 65535
            or object_identity in objects
        ):
            raise HarnessError("pdffonts_object_contract")
        objects.add(object_identity)

        prefix = PDF_SUBSET_PREFIX_RE.match(raw_name)
        has_subset_prefix = prefix is not None
        base_name = raw_name[prefix.end() :] if prefix is not None else raw_name
        if (
            "+" in base_name
            or (subset == "yes") != has_subset_prefix
            or not PDF_FONT_NAME_RE.fullmatch(base_name)
        ):
            raise HarnessError("pdffonts_subset_contract")
        records.append(
            PdfFontRecord(
                _normalized_pdf_font_identity(base_name, "pdffonts_font_name"),
                embedded == "yes",
                subset == "yes",
                unicode_map == "yes",
            )
        )
    return tuple(records)


def attest_pdf_fonts(
    records: Sequence[PdfFontRecord], font_pack: FontPack
) -> dict[str, object]:
    """Return path/content-neutral evidence for one font-pack attestation."""
    embedded = sum(record.embedded for record in records)
    subset = sum(record.subset for record in records)
    unicode_maps = sum(record.unicode_map for record in records)
    if embedded != len(records):
        raise HarnessError("libreoffice_font_not_embedded")
    if subset != len(records):
        raise HarnessError("libreoffice_font_not_subset")
    if unicode_maps != len(records):
        raise HarnessError("libreoffice_font_unicode_map_missing")
    identities = sorted(record.normalized_identity for record in records)
    identity_payload = (("\n".join(identities) + "\n") if identities else "").encode(
        "ascii"
    )
    matched = sum(
        record.normalized_identity in font_pack.pdf_identities for record in records
    )
    return {
        "embedded_font_objects": embedded,
        "font_objects": len(records),
        "matched_font_objects": matched,
        "normalized_identities_sha256": hashlib.sha256(identity_payload).hexdigest(),
        "subset_font_objects": subset,
        "unicode_font_objects": unicode_maps,
        "unique_font_identities": len(set(identities)),
    }


def parse_pdftotext_pages(
    payload: bytes,
    *,
    expected_pages: int,
    max_codepoints: int,
    max_tokens: int,
) -> tuple[tuple[str, ...], ...]:
    """Parse one bounded ``pdftotext`` stream into normalized page tokens."""
    try:
        text = payload.decode("utf-8", "strict")
    except UnicodeDecodeError as error:
        raise HarnessError("semantic_text_invalid_utf8") from error
    if "\x00" in text:
        raise HarnessError("semantic_text_nul")
    pages = text.split("\f")
    while len(pages) > expected_pages and pages[-1].strip() == "":
        pages.pop()
    if len(pages) != expected_pages:
        raise HarnessError("semantic_text_page_count")
    normalized_pages = []
    codepoints_used = 0
    tokens_used = 0
    for page in pages:
        tokens = normalize_semantic_tokens(
            page,
            max_codepoints=max_codepoints - codepoints_used,
            max_tokens=max_tokens - tokens_used,
        )
        codepoints_used += sum(len(token) for token in tokens)
        tokens_used += len(tokens)
        normalized_pages.append(tokens)
    return tuple(normalized_pages)


def parse_pdftotext_bbox_pages(
    payload: bytes,
    *,
    expected_pages: int,
    max_bytes: int,
    max_codepoints: int,
    max_tokens: int,
) -> tuple[PdfTextPage, ...]:
    """Parse bounded Poppler ``-bbox-layout`` XHTML without retaining text."""
    if not payload or len(payload) > max_bytes:
        raise HarnessError("semantic_bbox_output_limit")
    upper = payload.upper()
    if b"<!ENTITY" in upper:
        raise HarnessError("semantic_bbox_unsafe_markup")
    if b"<!DOCTYPE" in upper and (
        payload.count(PDFTOTEXT_XHTML_DOCTYPE) != 1
        or upper.count(b"<!DOCTYPE") != 1
    ):
        raise HarnessError("semantic_bbox_unsafe_markup")
    try:
        text_payload = payload.decode("utf-8", "strict")
        if "\x00" in text_payload:
            raise HarnessError("semantic_bbox_invalid_utf8")
        root = ET.fromstring(text_payload)
    except UnicodeDecodeError as error:
        raise HarnessError("semantic_bbox_invalid_utf8") from error
    except ET.ParseError as error:
        raise HarnessError("semantic_bbox_invalid_xml") from error
    page_elements = [
        element for element in root.iter() if _xml_local_name(element.tag) == "page"
    ]
    if len(page_elements) != expected_pages:
        raise HarnessError("semantic_bbox_page_count")

    pages: list[PdfTextPage] = []
    codepoints_used = 0
    tokens_used = 0
    raw_codepoints = 0
    for page_element in page_elements:
        width = _parse_finite_number(
            page_element.attrib.get("width"), "semantic_bbox_page_geometry"
        )
        height = _parse_finite_number(
            page_element.attrib.get("height"), "semantic_bbox_page_geometry"
        )
        if width <= 0 or height <= 0:
            raise HarnessError("semantic_bbox_page_geometry")
        if width > 1_000_000 or height > 1_000_000:
            raise HarnessError("semantic_bbox_page_geometry")
        words: list[SemanticTextBox] = []
        for element in page_element.iter():
            if _xml_local_name(element.tag) != "word":
                continue
            if list(element):
                raise HarnessError("semantic_bbox_word_markup")
            text = "".join(element.itertext())
            raw_codepoints += len(text)
            if raw_codepoints > max_codepoints * 4:
                raise HarnessError("semantic_raw_codepoint_limit")
            tokens = normalize_semantic_tokens(
                text,
                max_codepoints=max_codepoints - codepoints_used,
                max_tokens=max_tokens - tokens_used,
            )
            codepoints_used += sum(len(token) for token in tokens)
            tokens_used += len(tokens)
            if not tokens:
                continue
            x_min = _parse_finite_number(
                element.attrib.get("xMin"), "semantic_bbox_word_geometry"
            )
            y_min = _parse_finite_number(
                element.attrib.get("yMin"), "semantic_bbox_word_geometry"
            )
            x_max = _parse_finite_number(
                element.attrib.get("xMax"), "semantic_bbox_word_geometry"
            )
            y_max = _parse_finite_number(
                element.attrib.get("yMax"), "semantic_bbox_word_geometry"
            )
            epsilon = BBOX_COORDINATE_EPSILON_POINTS
            if (
                x_max <= x_min
                or y_max <= y_min
                or x_min < -epsilon
                or y_min < -epsilon
                or x_max > width + epsilon
                or y_max > height + epsilon
            ):
                raise HarnessError("semantic_bbox_word_geometry")
            words.append(
                SemanticTextBox(
                    tokens,
                    (
                        min(width, max(0.0, x_min)),
                        min(height, max(0.0, y_min)),
                        min(width, max(0.0, x_max)),
                        min(height, max(0.0, y_max)),
                    ),
                )
            )
        pages.append(PdfTextPage(width, height, tuple(words)))
    return tuple(pages)


def _histogram_nearest_rank(
    histogram: Counter[int], numerator: int, denominator: int
) -> int | None:
    total = sum(histogram.values())
    if total == 0:
        return None
    rank = max(1, (total * numerator + denominator - 1) // denominator)
    seen = 0
    for value, count in sorted(histogram.items()):
        seen += count
        if seen >= rank:
            return value
    raise HarnessError("text_box_histogram")


def _text_box_numeric_evidence(
    candidates: int,
    matched: int,
    ambiguous: int,
    unmatched: int,
    histogram: Counter[int],
) -> dict[str, object]:
    if (
        min(candidates, matched, ambiguous, unmatched) < 0
        or candidates != matched + ambiguous + unmatched
        or sum(histogram.values()) != matched
        or any(error < 0 or count <= 0 for error, count in histogram.items())
    ):
        raise HarnessError("text_box_histogram")
    return {
        "text_box_candidate_items": candidates,
        "text_box_matched_items": matched,
        "text_box_ambiguous_items": ambiguous,
        "text_box_unmatched_items": unmatched,
        "text_box_match_coverage_ppm": _ratio_ppm(
            matched, candidates, empty=1_000_000
        ),
        "text_box_error_histogram_millipoints": [
            {"error_millipoints": error, "count": count}
            for error, count in sorted(histogram.items())
        ],
        "text_box_median_error_millipoints": _histogram_nearest_rank(
            histogram, 1, 2
        ),
        "text_box_p95_error_millipoints": _histogram_nearest_rank(
            histogram, 95, 100
        ),
    }


def text_box_metrics(
    rxls: SvgSemanticEvidence,
    libreoffice: PdfTextPage,
    *,
    max_match_work: int = MAX_TEXT_BOX_MATCH_WORK,
) -> dict[str, object]:
    """Match exact labels to unique nearest Poppler boxes and discard content."""
    lo_tokens = [word.tokens for word in libreoffice.words]
    lo_boxes = [word.bbox_points for word in libreoffice.words]
    starts_by_token: dict[str, list[int]] = {}
    for index, tokens in enumerate(lo_tokens):
        starts_by_token.setdefault(tokens[0], []).append(index)

    used = [False] * len(lo_tokens)
    matched = 0
    ambiguous = 0
    unmatched = rxls.unbounded_items
    histogram: Counter[int] = Counter()
    work = 0
    for box in rxls.boxes:
        candidates: list[
            tuple[float, int, int, tuple[float, float, float, float]]
        ] = []
        for start in starts_by_token.get(box.tokens[0], ()):
            if used[start]:
                continue
            accumulated: list[str] = []
            end = start
            while end < len(lo_tokens) and len(accumulated) < len(box.tokens):
                work += 1
                if work > max_match_work:
                    raise HarnessError("text_box_match_work_limit")
                if used[end]:
                    break
                accumulated.extend(lo_tokens[end])
                end += 1
            if tuple(accumulated) != box.tokens:
                continue
            candidate_box = (
                min(value[0] for value in lo_boxes[start:end]),
                min(value[1] for value in lo_boxes[start:end]),
                max(value[2] for value in lo_boxes[start:end]),
                max(value[3] for value in lo_boxes[start:end]),
            )
            rxls_center = (
                (box.bbox_points[0] + box.bbox_points[2]) / 2.0,
                (box.bbox_points[1] + box.bbox_points[3]) / 2.0,
            )
            lo_center = (
                (candidate_box[0] + candidate_box[2]) / 2.0,
                (candidate_box[1] + candidate_box[3]) / 2.0,
            )
            distance = (rxls_center[0] - lo_center[0]) ** 2 + (
                rxls_center[1] - lo_center[1]
            ) ** 2
            candidates.append((distance, start, end, candidate_box))
        if not candidates:
            unmatched += 1
            continue
        candidates.sort(key=lambda item: (item[0], item[1], item[2]))
        if len(candidates) > 1 and math.isclose(
            candidates[0][0],
            candidates[1][0],
            rel_tol=1e-12,
            abs_tol=1e-9,
        ):
            ambiguous += 1
            continue
        _, start, end, candidate_box = candidates[0]
        used[start:end] = [True] * (end - start)
        error_points = max(
            abs(left - right)
            for left, right in zip(box.bbox_points, candidate_box)
        )
        error_millipoints = int(math.floor(error_points * 1000.0 + 0.5))
        histogram[error_millipoints] += 1
        matched += 1
    return _text_box_numeric_evidence(
        len(rxls.boxes) + rxls.unbounded_items,
        matched,
        ambiguous,
        unmatched,
        histogram,
    )


def _semantic_ratio_evidence(
    prefix: str,
    rxls_items: int,
    libreoffice_items: int,
    matched_items: int,
) -> dict[str, int]:
    both_empty = rxls_items == 0 and libreoffice_items == 0
    precision = _ratio_ppm(
        matched_items,
        rxls_items,
        empty=1_000_000 if both_empty else 0,
    )
    recall = _ratio_ppm(
        matched_items,
        libreoffice_items,
        empty=1_000_000 if both_empty else 0,
    )
    f1 = _ratio_ppm(
        2 * matched_items,
        rxls_items + libreoffice_items,
        empty=1_000_000 if both_empty else 0,
    )
    return {
        f"semantic_{prefix}_rxls_items": rxls_items,
        f"semantic_{prefix}_libreoffice_items": libreoffice_items,
        f"semantic_{prefix}_matched_items": matched_items,
        f"semantic_{prefix}_precision_ppm": precision,
        f"semantic_{prefix}_recall_ppm": recall,
        f"semantic_{prefix}_f1_ppm": f1,
    }


def semantic_text_metrics(
    rxls_tokens: Sequence[str], libreoffice_tokens: Sequence[str]
) -> dict[str, int]:
    """Return privacy-preserving exact-token, codepoint, and order evidence."""
    rxls_token_counter = Counter(rxls_tokens)
    libreoffice_token_counter = Counter(libreoffice_tokens)
    matched_tokens = sum((rxls_token_counter & libreoffice_token_counter).values())

    rxls_codepoints = Counter("".join(rxls_tokens))
    libreoffice_codepoints = Counter("".join(libreoffice_tokens))
    matched_codepoints = sum((rxls_codepoints & libreoffice_codepoints).values())

    rxls_bigrams = Counter(zip(rxls_tokens, rxls_tokens[1:]))
    libreoffice_bigrams = Counter(
        zip(libreoffice_tokens, libreoffice_tokens[1:])
    )
    matched_bigrams = sum((rxls_bigrams & libreoffice_bigrams).values())

    one_sided_empty = bool(rxls_tokens) != bool(libreoffice_tokens)
    evidence = {
        "semantic_exact": int(tuple(rxls_tokens) == tuple(libreoffice_tokens)),
        "semantic_comparable": int(not one_sided_empty),
        "semantic_one_sided_empty": int(one_sided_empty),
    }
    evidence.update(
        _semantic_ratio_evidence(
            "token", len(rxls_tokens), len(libreoffice_tokens), matched_tokens
        )
    )
    evidence.update(
        _semantic_ratio_evidence(
            "codepoint",
            sum(rxls_codepoints.values()),
            sum(libreoffice_codepoints.values()),
            matched_codepoints,
        )
    )
    evidence.update(
        _semantic_ratio_evidence(
            "bigram",
            sum(rxls_bigrams.values()),
            sum(libreoffice_bigrams.values()),
            matched_bigrams,
        )
    )
    return evidence


def _run_pdf_font_inspector(
    pdf: Path,
    *,
    config: HarnessConfig,
    runner: CommandRunner,
    cwd: Path,
    env: dict[str, str],
) -> CommandResult:
    pdffonts = os.environ.get("PDFFONTS", "pdffonts")
    inspection_env = dict(env)
    inspection_env.update({"LANG": "C", "LC_ALL": "C"})
    return runner.run(
        [pdffonts, str(pdf)],
        cwd=cwd,
        env=inspection_env,
        timeout_seconds=config.caps.timeout_seconds,
        output_limit_bytes=config.caps.max_command_output_bytes,
    )


def _run_pdf_text_extractor(
    pdf: Path,
    pages: int,
    *,
    config: HarnessConfig,
    runner: CommandRunner,
    cwd: Path,
    env: dict[str, str],
) -> CommandResult:
    pdftotext = os.environ.get("PDFTOTEXT", "pdftotext")
    text_env = dict(env)
    text_env.update({"LANG": "C", "LC_ALL": "C"})
    return runner.run(
        [
            pdftotext,
            "-f",
            "1",
            "-l",
            str(pages),
            "-layout",
            "-enc",
            "UTF-8",
            str(pdf),
            "-",
        ],
        cwd=cwd,
        env=text_env,
        timeout_seconds=config.caps.timeout_seconds,
        output_limit_bytes=config.caps.max_command_output_bytes,
    )


def _run_pdf_bbox_extractor(
    pdf: Path,
    pages: int,
    output: Path,
    *,
    config: HarnessConfig,
    runner: CommandRunner,
    cwd: Path,
    env: dict[str, str],
) -> CommandResult:
    pdftotext = os.environ.get("PDFTOTEXT", "pdftotext")
    text_env = dict(env)
    text_env.update({"LANG": "C", "LC_ALL": "C"})
    try:
        output.unlink(missing_ok=True)
    except OSError:
        return CommandResult("nonzero", 25)
    return runner.run(
        [
            pdftotext,
            "-f",
            "1",
            "-l",
            str(pages),
            "-bbox-layout",
            "-enc",
            "UTF-8",
            str(pdf),
            str(output),
        ],
        cwd=cwd,
        env=text_env,
        timeout_seconds=config.caps.timeout_seconds,
        output_limit_bytes=config.caps.max_command_output_bytes,
    )


def _poppler_failure(code: int, classification: str) -> tuple[CommandResult, None]:
    payload = json.dumps({"code": classification}, sort_keys=True).encode()
    return CommandResult("nonzero", code, payload, b""), None


def _run_poppler_pdf_rasterizer(
    pdf: Path,
    output_dir: Path,
    *,
    config: HarnessConfig,
    runner: CommandRunner,
    cwd: Path,
    env: dict[str, str],
) -> tuple[CommandResult, dict[str, object] | None]:
    pdfinfo = os.environ.get("PDFINFO", "pdfinfo")
    pdftoppm = os.environ.get("PDFTOPPM", "pdftoppm")
    poppler_env = dict(env)
    poppler_env.update({"LANG": "C", "LC_ALL": "C"})
    first = runner.run(
        [pdfinfo, "-box", str(pdf)],
        cwd=cwd,
        env=poppler_env,
        timeout_seconds=config.caps.timeout_seconds,
        output_limit_bytes=config.caps.max_command_output_bytes,
    )
    if first.status != "ok":
        return first, None
    try:
        pages, _ = parse_pdfinfo(first.stdout.decode("utf-8", "replace"), require_all_sizes=False)
    except HarnessError:
        return _poppler_failure(24, "pdfinfo_invalid")
    if pages > config.caps.max_pages:
        return _poppler_failure(20, "libreoffice_page_limit")

    details = runner.run(
        [pdfinfo, "-box", "-f", "1", "-l", str(pages), str(pdf)],
        cwd=cwd,
        env=poppler_env,
        timeout_seconds=config.caps.timeout_seconds,
        output_limit_bytes=config.caps.max_command_output_bytes,
    )
    if details.status != "ok":
        return details, None
    try:
        detailed_pages, sizes = parse_pdfinfo(
            details.stdout.decode("utf-8", "replace"), require_all_sizes=True
        )
    except HarnessError:
        return _poppler_failure(24, "pdfinfo_invalid")
    if detailed_pages != pages:
        return _poppler_failure(24, "pdfinfo_inconsistent")
    total_pixels = 0
    for width_points, height_points in sizes:
        width = math.ceil(width_points * config.dpi / 72)
        height = math.ceil(height_points * config.dpi / 72)
        pixels = width * height
        if width <= 0 or height <= 0 or pixels > config.caps.max_page_pixels:
            return _poppler_failure(21, "libreoffice_page_pixel_limit")
        total_pixels += pixels
        if total_pixels > config.caps.max_total_pixels:
            return _poppler_failure(22, "libreoffice_total_pixel_limit")

    prefix = output_dir / "page"
    result = runner.run(
        [pdftoppm, "-png", "-r", str(config.dpi), str(pdf), str(prefix)],
        cwd=cwd,
        env=poppler_env,
        timeout_seconds=config.caps.timeout_seconds,
        output_limit_bytes=config.caps.max_command_output_bytes,
    )
    if result.status != "ok":
        return result, None
    generated = []
    for path in output_dir.glob("page-*.png"):
        match = re.fullmatch(r"page-(\d+)\.png", path.name)
        if match:
            generated.append((int(match.group(1)), path))
    generated.sort()
    if [number for number, _ in generated] != list(range(1, pages + 1)):
        return _poppler_failure(24, "pdftoppm_page_sequence")
    total_output = 0
    rows = []
    for offset, (_, source) in enumerate(generated):
        target = output_dir / f"page-{offset:04d}.png"
        source.replace(target)
        total_output += target.stat().st_size
        if total_output > config.caps.max_artifact_bytes:
            return _poppler_failure(23, "libreoffice_raster_output_limit")
        rows.append({"file": target.name})
    payload = json.dumps({"pages": rows}, sort_keys=True, separators=(",", ":")).encode()
    return CommandResult("ok", 0, payload, result.stderr), {"pages": rows}


def integer_image_metrics(left_rgb: bytes, right_rgb: bytes) -> dict[str, int]:
    """Compute normalized visual metrics with integer arithmetic only."""
    if len(left_rgb) != len(right_rgb) or len(left_rgb) % 3:
        raise HarnessError("metric_buffer_mismatch")
    pixels = len(left_rgb) // 3
    if pixels == 0:
        raise HarnessError("metric_empty_image")
    changed_pixels = 0
    absolute_error_sum = 0
    squared_error_sum = 0
    max_channel_delta = 0
    for offset in range(0, len(left_rgb), 3):
        changed = False
        for channel in range(3):
            delta = abs(left_rgb[offset + channel] - right_rgb[offset + channel])
            if delta:
                changed = True
            absolute_error_sum += delta
            squared_error_sum += delta * delta
            max_channel_delta = max(max_channel_delta, delta)
        if changed:
            changed_pixels += 1
    channel_denominator = pixels * 3 * 255
    squared_denominator = pixels * 3 * 255 * 255
    mean_absolute_error_ppm = (
        absolute_error_sum * 1_000_000 + channel_denominator // 2
    ) // channel_denominator
    mismatch_ppm = (changed_pixels * 1_000_000 + pixels // 2) // pixels
    root_mean_square_error_ppm = math.isqrt(
        (squared_error_sum * 1_000_000_000_000) // squared_denominator
    )
    return {
        "pixels": pixels,
        "changed_pixels": changed_pixels,
        "mismatch_ppm": mismatch_ppm,
        "absolute_error_sum": absolute_error_sum,
        "squared_error_sum": squared_error_sum,
        "max_channel_delta": max_channel_delta,
        "mean_absolute_error_ppm": mean_absolute_error_ppm,
        "root_mean_square_error_ppm": root_mean_square_error_ppm,
        "similarity_ppm": max(0, 1_000_000 - mean_absolute_error_ppm),
    }


def _ratio_ppm(numerator: int, denominator: int, *, empty: int = 0) -> int:
    if denominator == 0:
        return empty
    return (numerator * 1_000_000 + denominator // 2) // denominator


def _round_signed(numerator: int, denominator: int) -> int:
    if denominator <= 0:
        return 0
    if numerator < 0:
        return -((-numerator + denominator // 2) // denominator)
    return (numerator + denominator // 2) // denominator


def _empty_bbox() -> dict[str, int]:
    return {"present": 0, "left": 0, "top": 0, "right": 0, "bottom": 0}


def _mask_stats(mask: bytes | bytearray, width: int, height: int) -> dict[str, object]:
    if width <= 0 or height <= 0 or len(mask) != width * height:
        raise HarnessError("metric_buffer_mismatch")
    count = 0
    x_sum = 0
    y_sum = 0
    left = width
    top = height
    right = -1
    bottom = -1
    for y in range(height):
        row = y * width
        for x in range(width):
            if not mask[row + x]:
                continue
            count += 1
            x_sum += x
            y_sum += y
            left = min(left, x)
            top = min(top, y)
            right = max(right, x)
            bottom = max(bottom, y)
    bbox = (
        {"present": 1, "left": left, "top": top, "right": right, "bottom": bottom}
        if count
        else _empty_bbox()
    )
    return {"count": count, "x_sum": x_sum, "y_sum": y_sum, "bbox": bbox}


def _dilate_mask_1px(mask: bytes | bytearray, width: int, height: int) -> bytearray:
    """Dilate a binary mask by a one-pixel Chebyshev neighbourhood."""
    horizontal = bytearray(width * height)
    for y in range(height):
        row = y * width
        for x in range(width):
            offset = row + x
            horizontal[offset] = int(
                bool(mask[offset])
                or (x > 0 and bool(mask[offset - 1]))
                or (x + 1 < width and bool(mask[offset + 1]))
            )
    expanded = bytearray(width * height)
    for y in range(height):
        row = y * width
        above = row - width
        below = row + width
        for x in range(width):
            offset = row + x
            expanded[offset] = int(
                bool(horizontal[offset])
                or (y > 0 and bool(horizontal[above + x]))
                or (y + 1 < height and bool(horizontal[below + x]))
            )
    return expanded


def _prf_evidence(
    prefix: str,
    rxls_pixels: int,
    libreoffice_pixels: int,
    rxls_matched: int,
    libreoffice_matched: int,
) -> dict[str, int]:
    both_empty = rxls_pixels == 0 and libreoffice_pixels == 0
    precision = _ratio_ppm(
        rxls_matched,
        rxls_pixels,
        empty=1_000_000 if both_empty else 0,
    )
    recall = _ratio_ppm(
        libreoffice_matched,
        libreoffice_pixels,
        empty=1_000_000 if both_empty else 0,
    )
    f1_denominator = (
        rxls_matched * libreoffice_pixels
        + libreoffice_matched * rxls_pixels
    )
    if both_empty:
        f1 = 1_000_000
    elif f1_denominator == 0:
        f1 = 0
    else:
        f1 = _ratio_ppm(
            2 * rxls_matched * libreoffice_matched,
            f1_denominator,
        )
    return {
        f"{prefix}_rxls_pixels": rxls_pixels,
        f"{prefix}_libreoffice_pixels": libreoffice_pixels,
        f"{prefix}_rxls_matched_1px": rxls_matched,
        f"{prefix}_libreoffice_matched_1px": libreoffice_matched,
        f"{prefix}_precision_ppm": precision,
        f"{prefix}_recall_ppm": recall,
        f"{prefix}_f1_ppm": f1,
    }


def _geometry_evidence(
    prefix: str,
    rxls: dict[str, object],
    libreoffice: dict[str, object],
) -> dict[str, object]:
    rxls_count = int(rxls["count"])
    libreoffice_count = int(libreoffice["count"])
    rxls_x_sum = int(rxls["x_sum"])
    rxls_y_sum = int(rxls["y_sum"])
    libreoffice_x_sum = int(libreoffice["x_sum"])
    libreoffice_y_sum = int(libreoffice["y_sum"])
    rxls_bbox = dict(rxls["bbox"])
    libreoffice_bbox = dict(libreoffice["bbox"])
    both_present = rxls_count > 0 and libreoffice_count > 0
    both_empty = rxls_count == 0 and libreoffice_count == 0
    comparable = int(both_present or both_empty)

    rxls_centroid_x = _round_signed(rxls_x_sum * 1000, rxls_count)
    rxls_centroid_y = _round_signed(rxls_y_sum * 1000, rxls_count)
    libreoffice_centroid_x = _round_signed(
        libreoffice_x_sum * 1000, libreoffice_count
    )
    libreoffice_centroid_y = _round_signed(
        libreoffice_y_sum * 1000, libreoffice_count
    )
    if both_present:
        denominator = rxls_count * libreoffice_count
        centroid_delta_x = _round_signed(
            (rxls_x_sum * libreoffice_count - libreoffice_x_sum * rxls_count)
            * 1000,
            denominator,
        )
        centroid_delta_y = _round_signed(
            (rxls_y_sum * libreoffice_count - libreoffice_y_sum * rxls_count)
            * 1000,
            denominator,
        )
        bbox_delta = {
            key: int(rxls_bbox[key]) - int(libreoffice_bbox[key])
            for key in ("left", "top", "right", "bottom")
        }
        bbox_max_delta = max(abs(value) for value in bbox_delta.values())
    else:
        centroid_delta_x = 0
        centroid_delta_y = 0
        bbox_delta = {"left": 0, "top": 0, "right": 0, "bottom": 0}
        bbox_max_delta = 0
    return {
        f"{prefix}_rxls_x_sum": rxls_x_sum,
        f"{prefix}_rxls_y_sum": rxls_y_sum,
        f"{prefix}_libreoffice_x_sum": libreoffice_x_sum,
        f"{prefix}_libreoffice_y_sum": libreoffice_y_sum,
        f"{prefix}_rxls_bbox": rxls_bbox,
        f"{prefix}_libreoffice_bbox": libreoffice_bbox,
        f"{prefix}_alignment_comparable": comparable,
        f"{prefix}_bbox_delta_pixels": bbox_delta,
        f"{prefix}_bbox_alignment_max_delta_pixels": bbox_max_delta,
        f"{prefix}_rxls_centroid_x_millipixels": rxls_centroid_x,
        f"{prefix}_rxls_centroid_y_millipixels": rxls_centroid_y,
        f"{prefix}_libreoffice_centroid_x_millipixels": libreoffice_centroid_x,
        f"{prefix}_libreoffice_centroid_y_millipixels": libreoffice_centroid_y,
        f"{prefix}_centroid_delta_x_millipixels": centroid_delta_x,
        f"{prefix}_centroid_delta_y_millipixels": centroid_delta_y,
        f"{prefix}_centroid_distance_millipixels": math.isqrt(
            centroid_delta_x * centroid_delta_x
            + centroid_delta_y * centroid_delta_y
        ),
    }


def _mask_evidence(
    prefix: str,
    rxls_mask: bytes | bytearray,
    libreoffice_mask: bytes | bytearray,
    width: int,
    height: int,
    *,
    geometry: bool,
) -> dict[str, object]:
    rxls = _mask_stats(rxls_mask, width, height)
    libreoffice = _mask_stats(libreoffice_mask, width, height)
    rxls_matched = sum(
        int(bool(value) and bool(candidate))
        for value, candidate in zip(
            rxls_mask,
            _dilate_mask_1px(libreoffice_mask, width, height),
        )
    )
    libreoffice_matched = sum(
        int(bool(value) and bool(candidate))
        for value, candidate in zip(
            libreoffice_mask,
            _dilate_mask_1px(rxls_mask, width, height),
        )
    )
    evidence: dict[str, object] = _prf_evidence(
        prefix,
        int(rxls["count"]),
        int(libreoffice["count"]),
        rxls_matched,
        libreoffice_matched,
    )
    if geometry:
        evidence.update(_geometry_evidence(prefix, rxls, libreoffice))
    return evidence


def _luma_and_foreground(rgb: bytes) -> tuple[bytearray, bytearray]:
    luma = bytearray(len(rgb) // 3)
    foreground = bytearray(len(rgb) // 3)
    for pixel, offset in enumerate(range(0, len(rgb), 3)):
        red = rgb[offset]
        green = rgb[offset + 1]
        blue = rgb[offset + 2]
        luma[pixel] = (77 * red + 150 * green + 29 * blue + 128) >> 8
        foreground[pixel] = int(
            red < FOREGROUND_CHANNEL_THRESHOLD
            or green < FOREGROUND_CHANNEL_THRESHOLD
            or blue < FOREGROUND_CHANNEL_THRESHOLD
        )
    return luma, foreground


def _edge_mask(luma: bytes | bytearray, width: int, height: int) -> bytearray:
    mask = bytearray(width * height)
    for y in range(height):
        row = y * width
        for x in range(width):
            offset = row + x
            value = luma[offset]
            mask[offset] = int(
                (x > 0 and abs(value - luma[offset - 1]) >= EDGE_LUMA_DELTA)
                or (
                    x + 1 < width
                    and abs(value - luma[offset + 1]) >= EDGE_LUMA_DELTA
                )
                or (y > 0 and abs(value - luma[offset - width]) >= EDGE_LUMA_DELTA)
                or (
                    y + 1 < height
                    and abs(value - luma[offset + width]) >= EDGE_LUMA_DELTA
                )
            )
    return mask


def _text_ink_mask(luma: bytes | bytearray, width: int, height: int) -> bytearray:
    """Return a conservative dark/local-contrast mask; this is not OCR."""
    mask = bytearray(width * height)
    for y in range(height):
        row = y * width
        for x in range(width):
            offset = row + x
            value = luma[offset]
            if value > TEXT_INK_MAX_LUMA:
                continue
            mask[offset] = int(
                (x > 0 and luma[offset - 1] - value >= EDGE_LUMA_DELTA)
                or (
                    x + 1 < width
                    and luma[offset + 1] - value >= EDGE_LUMA_DELTA
                )
                or (y > 0 and luma[offset - width] - value >= EDGE_LUMA_DELTA)
                or (
                    y + 1 < height
                    and luma[offset + width] - value >= EDGE_LUMA_DELTA
                )
            )
    return mask


def _box_blur_luma_3px(
    luma: bytes | bytearray, width: int, height: int
) -> bytearray:
    """Apply a clipped-edge separable three-pixel blur with integer rounding."""
    horizontal = bytearray(width * height)
    for y in range(height):
        row = y * width
        window = luma[row]
        if width > 1:
            window += luma[row + 1]
        for x in range(width):
            count = 1 + int(x > 0) + int(x + 1 < width)
            horizontal[row + x] = (window + count // 2) // count
            if x > 0:
                window -= luma[row + x - 1]
            if x + 2 < width:
                window += luma[row + x + 2]

    blurred = bytearray(width * height)
    for x in range(width):
        window = horizontal[x]
        if height > 1:
            window += horizontal[width + x]
        for y in range(height):
            count = 1 + int(y > 0) + int(y + 1 < height)
            blurred[y * width + x] = (window + count // 2) // count
            if y > 0:
                window -= horizontal[(y - 1) * width + x]
            if y + 2 < height:
                window += horizontal[(y + 2) * width + x]
    return blurred


def _matched_foreground_color_error(
    rxls_rgb: bytes,
    libreoffice_rgb: bytes,
    rxls_mask: bytes | bytearray,
    libreoffice_mask: bytes | bytearray,
    width: int,
    height: int,
) -> tuple[int, int]:
    samples = 0
    absolute_error = 0
    for y in range(height):
        row = y * width
        for x in range(width):
            pixel = row + x
            if not rxls_mask[pixel]:
                continue
            rxls_offset = pixel * 3
            best: int | None = None
            for candidate_y in range(max(0, y - 1), min(height, y + 2)):
                candidate_row = candidate_y * width
                for candidate_x in range(max(0, x - 1), min(width, x + 2)):
                    candidate = candidate_row + candidate_x
                    if not libreoffice_mask[candidate]:
                        continue
                    candidate_offset = candidate * 3
                    error = (
                        abs(
                            rxls_rgb[rxls_offset]
                            - libreoffice_rgb[candidate_offset]
                        )
                        + abs(
                            rxls_rgb[rxls_offset + 1]
                            - libreoffice_rgb[candidate_offset + 1]
                        )
                        + abs(
                            rxls_rgb[rxls_offset + 2]
                            - libreoffice_rgb[candidate_offset + 2]
                        )
                    )
                    if best is None or error < best:
                        best = error
            if best is not None:
                samples += 1
                absolute_error += best
    return samples, absolute_error


def _visual_image_metrics_python(
    rxls_rgb: bytes,
    libreoffice_rgb: bytes,
    width: int,
    height: int,
    *,
    max_metric_work_units: int | None = None,
) -> dict[str, object]:
    """Compute deterministic bounded evidence from two equally sized RGB buffers."""
    pixels = width * height
    if width <= 0 or height <= 0 or len(rxls_rgb) != pixels * 3:
        raise HarnessError("metric_buffer_mismatch")
    if len(libreoffice_rgb) != len(rxls_rgb):
        raise HarnessError("metric_buffer_mismatch")
    work_units = pixels * METRIC_WORK_UNITS_PER_PIXEL
    if max_metric_work_units is not None and work_units > max_metric_work_units:
        raise HarnessError("metric_work_limit")

    evidence: dict[str, object] = integer_image_metrics(rxls_rgb, libreoffice_rgb)
    rxls_luma, rxls_foreground = _luma_and_foreground(rxls_rgb)
    libreoffice_luma, libreoffice_foreground = _luma_and_foreground(
        libreoffice_rgb
    )
    evidence.update(
        _mask_evidence(
            "foreground",
            rxls_foreground,
            libreoffice_foreground,
            width,
            height,
            geometry=True,
        )
    )

    color_samples, color_absolute = _matched_foreground_color_error(
        rxls_rgb,
        libreoffice_rgb,
        rxls_foreground,
        libreoffice_foreground,
        width,
        height,
    )
    color_denominator = color_samples * 3 * 255
    color_mae = _ratio_ppm(color_absolute, color_denominator, empty=0)
    evidence.update(
        {
            "foreground_matched_color_samples": color_samples,
            "foreground_matched_color_absolute_error_sum": color_absolute,
            "foreground_matched_color_mean_absolute_error_ppm": color_mae,
            "foreground_matched_color_similarity_ppm": max(
                0, 1_000_000 - color_mae
            ),
        }
    )

    rxls_edge = _edge_mask(rxls_luma, width, height)
    libreoffice_edge = _edge_mask(libreoffice_luma, width, height)
    evidence.update(
        _mask_evidence(
            "edge",
            rxls_edge,
            libreoffice_edge,
            width,
            height,
            geometry=False,
        )
    )

    rxls_text_ink = _text_ink_mask(rxls_luma, width, height)
    libreoffice_text_ink = _text_ink_mask(libreoffice_luma, width, height)
    evidence.update(
        _mask_evidence(
            "text_ink",
            rxls_text_ink,
            libreoffice_text_ink,
            width,
            height,
            geometry=True,
        )
    )

    rxls_blurred = _box_blur_luma_3px(rxls_luma, width, height)
    libreoffice_blurred = _box_blur_luma_3px(libreoffice_luma, width, height)
    blurred_absolute = sum(
        abs(rxls_value - libreoffice_value)
        for rxls_value, libreoffice_value in zip(rxls_blurred, libreoffice_blurred)
    )
    blurred_mae = _ratio_ppm(blurred_absolute, pixels * 255)
    evidence.update(
        {
            "blurred_luma_absolute_error_sum": blurred_absolute,
            "blurred_luma_mean_absolute_error_ppm": blurred_mae,
            "blurred_luma_similarity_ppm": max(0, 1_000_000 - blurred_mae),
            "metric_work_units": work_units,
        }
    )
    return evidence


def _metric_numpy() -> Any | None:
    """Return NumPy when available; the locked oracle requires an exact version."""
    try:
        import numpy
    except ImportError:
        return None
    return numpy


def _metric_implementation_evidence() -> dict[str, str]:
    if _metric_numpy() is None:
        return {"kind": "python_integer_reference_v1"}
    return {
        "kind": "numpy_integer_exact_v1",
        "version": importlib.metadata.version("numpy"),
    }


def _numpy_mask_stats(mask: Any, width: int, height: int, np: Any) -> dict[str, object]:
    if width <= 0 or height <= 0 or mask.shape != (height, width):
        raise HarnessError("metric_buffer_mismatch")
    y, x = np.nonzero(mask)
    count = int(x.size)
    if count:
        bbox = {
            "present": 1,
            "left": int(x.min()),
            "top": int(y.min()),
            "right": int(x.max()),
            "bottom": int(y.max()),
        }
        x_sum = int(x.sum(dtype=np.uint64))
        y_sum = int(y.sum(dtype=np.uint64))
    else:
        bbox = _empty_bbox()
        x_sum = 0
        y_sum = 0
    return {"count": count, "x_sum": x_sum, "y_sum": y_sum, "bbox": bbox}


def _numpy_dilate_mask_1px(mask: Any, np: Any) -> Any:
    expanded = mask.copy()
    expanded[:, 1:] |= mask[:, :-1]
    expanded[:, :-1] |= mask[:, 1:]
    expanded[1:, :] |= mask[:-1, :]
    expanded[:-1, :] |= mask[1:, :]
    expanded[1:, 1:] |= mask[:-1, :-1]
    expanded[1:, :-1] |= mask[:-1, 1:]
    expanded[:-1, 1:] |= mask[1:, :-1]
    expanded[:-1, :-1] |= mask[1:, 1:]
    return expanded


def _numpy_mask_evidence(
    prefix: str,
    rxls_mask: Any,
    libreoffice_mask: Any,
    width: int,
    height: int,
    *,
    geometry: bool,
    np: Any,
) -> dict[str, object]:
    rxls = _numpy_mask_stats(rxls_mask, width, height, np)
    libreoffice = _numpy_mask_stats(libreoffice_mask, width, height, np)
    rxls_matched = int(
        np.count_nonzero(rxls_mask & _numpy_dilate_mask_1px(libreoffice_mask, np))
    )
    libreoffice_matched = int(
        np.count_nonzero(libreoffice_mask & _numpy_dilate_mask_1px(rxls_mask, np))
    )
    evidence: dict[str, object] = _prf_evidence(
        prefix,
        int(rxls["count"]),
        int(libreoffice["count"]),
        rxls_matched,
        libreoffice_matched,
    )
    if geometry:
        evidence.update(_geometry_evidence(prefix, rxls, libreoffice))
    return evidence


def _numpy_luma_and_foreground(rgb: Any, np: Any) -> tuple[Any, Any]:
    # The weighted sum tops out at 65,408, so uint16 is exact and halves the
    # peak allocation compared with a uint32 staging image.
    channels = rgb.astype(np.uint16)
    luma = (
        77 * channels[:, :, 0]
        + 150 * channels[:, :, 1]
        + 29 * channels[:, :, 2]
        + 128
    ) >> 8
    foreground = np.any(rgb < FOREGROUND_CHANNEL_THRESHOLD, axis=2)
    return luma.astype(np.uint8), foreground


def _numpy_edge_mask(luma: Any, np: Any) -> Any:
    values = luma.astype(np.int16)
    mask = np.zeros(luma.shape, dtype=np.bool_)
    horizontal = np.abs(values[:, 1:] - values[:, :-1]) >= EDGE_LUMA_DELTA
    vertical = np.abs(values[1:, :] - values[:-1, :]) >= EDGE_LUMA_DELTA
    mask[:, 1:] |= horizontal
    mask[:, :-1] |= horizontal
    mask[1:, :] |= vertical
    mask[:-1, :] |= vertical
    return mask


def _numpy_text_ink_mask(luma: Any, np: Any) -> Any:
    values = luma.astype(np.int16)
    mask = np.zeros(luma.shape, dtype=np.bool_)
    mask[:, :-1] |= values[:, 1:] - values[:, :-1] >= EDGE_LUMA_DELTA
    mask[:, 1:] |= values[:, :-1] - values[:, 1:] >= EDGE_LUMA_DELTA
    mask[:-1, :] |= values[1:, :] - values[:-1, :] >= EDGE_LUMA_DELTA
    mask[1:, :] |= values[:-1, :] - values[1:, :] >= EDGE_LUMA_DELTA
    mask &= luma <= TEXT_INK_MAX_LUMA
    return mask


def _numpy_box_blur_luma_3px(luma: Any, np: Any) -> Any:
    values = luma.astype(np.uint16)
    horizontal_sum = values.copy()
    horizontal_sum[:, 1:] += values[:, :-1]
    horizontal_sum[:, :-1] += values[:, 1:]
    horizontal_count = np.full(values.shape[1], 3, dtype=np.uint16)
    horizontal_count[0] -= 1
    horizontal_count[-1] -= 1
    horizontal = (
        horizontal_sum + horizontal_count[np.newaxis, :] // 2
    ) // horizontal_count[np.newaxis, :]

    vertical_sum = horizontal.copy()
    vertical_sum[1:, :] += horizontal[:-1, :]
    vertical_sum[:-1, :] += horizontal[1:, :]
    vertical_count = np.full(values.shape[0], 3, dtype=np.uint16)
    vertical_count[0] -= 1
    vertical_count[-1] -= 1
    return (
        (vertical_sum + vertical_count[:, np.newaxis] // 2)
        // vertical_count[:, np.newaxis]
    ).astype(np.uint8)


def _numpy_matched_foreground_color_error(
    rxls_rgb: Any,
    libreoffice_rgb: Any,
    rxls_mask: Any,
    libreoffice_mask: Any,
    np: Any,
) -> tuple[int, int]:
    height, width = rxls_mask.shape
    sentinel = 1024
    best = np.full((height, width), sentinel, dtype=np.uint16)
    for delta_y in (-1, 0, 1):
        if delta_y < 0:
            source_y = slice(1, height)
            candidate_y = slice(0, height - 1)
        elif delta_y > 0:
            source_y = slice(0, height - 1)
            candidate_y = slice(1, height)
        else:
            source_y = slice(0, height)
            candidate_y = slice(0, height)
        for delta_x in (-1, 0, 1):
            if delta_x < 0:
                source_x = slice(1, width)
                candidate_x = slice(0, width - 1)
            elif delta_x > 0:
                source_x = slice(0, width - 1)
                candidate_x = slice(1, width)
            else:
                source_x = slice(0, width)
                candidate_x = slice(0, width)
            candidate_mask = libreoffice_mask[candidate_y, candidate_x]
            error = np.abs(
                np.subtract(
                    rxls_rgb[source_y, source_x, :],
                    libreoffice_rgb[candidate_y, candidate_x, :],
                    dtype=np.int16,
                )
            ).sum(axis=2, dtype=np.uint16)
            target = best[source_y, source_x]
            np.minimum(target, np.where(candidate_mask, error, sentinel), out=target)
    matched = rxls_mask & (best != sentinel)
    return int(np.count_nonzero(matched)), int(best[matched].sum(dtype=np.uint64))


def _visual_image_metrics_numpy(
    rxls_rgb: bytes,
    libreoffice_rgb: bytes,
    width: int,
    height: int,
    *,
    max_metric_work_units: int | None = None,
) -> dict[str, object]:
    """Compute the reference metric exactly with bounded vectorized integer operations."""
    np = _metric_numpy()
    if np is None:
        raise HarnessError("numpy_missing")
    pixels = width * height
    if width <= 0 or height <= 0 or len(rxls_rgb) != pixels * 3:
        raise HarnessError("metric_buffer_mismatch")
    if len(libreoffice_rgb) != len(rxls_rgb):
        raise HarnessError("metric_buffer_mismatch")
    work_units = pixels * METRIC_WORK_UNITS_PER_PIXEL
    if max_metric_work_units is not None and work_units > max_metric_work_units:
        raise HarnessError("metric_work_limit")

    rxls = np.frombuffer(rxls_rgb, dtype=np.uint8).reshape(height, width, 3)
    libreoffice = np.frombuffer(libreoffice_rgb, dtype=np.uint8).reshape(
        height, width, 3
    )
    delta = np.abs(np.subtract(rxls, libreoffice, dtype=np.int16))
    changed_pixels = int(np.count_nonzero(np.any(delta, axis=2)))
    absolute_error_sum = int(delta.sum(dtype=np.uint64))
    squared_error_sum = int(
        np.square(delta.astype(np.uint32)).sum(dtype=np.uint64)
    )
    max_channel_delta = int(delta.max())
    channel_denominator = pixels * 3 * 255
    squared_denominator = pixels * 3 * 255 * 255
    mean_absolute_error_ppm = (
        absolute_error_sum * 1_000_000 + channel_denominator // 2
    ) // channel_denominator
    evidence: dict[str, object] = {
        "pixels": pixels,
        "changed_pixels": changed_pixels,
        "mismatch_ppm": (changed_pixels * 1_000_000 + pixels // 2) // pixels,
        "absolute_error_sum": absolute_error_sum,
        "squared_error_sum": squared_error_sum,
        "max_channel_delta": max_channel_delta,
        "mean_absolute_error_ppm": mean_absolute_error_ppm,
        "root_mean_square_error_ppm": math.isqrt(
            (squared_error_sum * 1_000_000_000_000) // squared_denominator
        ),
        "similarity_ppm": max(0, 1_000_000 - mean_absolute_error_ppm),
    }
    del delta

    rxls_luma, rxls_foreground = _numpy_luma_and_foreground(rxls, np)
    libreoffice_luma, libreoffice_foreground = _numpy_luma_and_foreground(
        libreoffice, np
    )
    evidence.update(
        _numpy_mask_evidence(
            "foreground",
            rxls_foreground,
            libreoffice_foreground,
            width,
            height,
            geometry=True,
            np=np,
        )
    )
    color_samples, color_absolute = _numpy_matched_foreground_color_error(
        rxls,
        libreoffice,
        rxls_foreground,
        libreoffice_foreground,
        np,
    )
    color_mae = _ratio_ppm(color_absolute, color_samples * 3 * 255, empty=0)
    evidence.update(
        {
            "foreground_matched_color_samples": color_samples,
            "foreground_matched_color_absolute_error_sum": color_absolute,
            "foreground_matched_color_mean_absolute_error_ppm": color_mae,
            "foreground_matched_color_similarity_ppm": max(
                0, 1_000_000 - color_mae
            ),
        }
    )
    del rxls_foreground, libreoffice_foreground

    rxls_edge = _numpy_edge_mask(rxls_luma, np)
    libreoffice_edge = _numpy_edge_mask(libreoffice_luma, np)
    evidence.update(
        _numpy_mask_evidence(
            "edge",
            rxls_edge,
            libreoffice_edge,
            width,
            height,
            geometry=False,
            np=np,
        )
    )
    del rxls_edge, libreoffice_edge
    rxls_text_ink = _numpy_text_ink_mask(rxls_luma, np)
    libreoffice_text_ink = _numpy_text_ink_mask(libreoffice_luma, np)
    evidence.update(
        _numpy_mask_evidence(
            "text_ink",
            rxls_text_ink,
            libreoffice_text_ink,
            width,
            height,
            geometry=True,
            np=np,
        )
    )
    del rxls_text_ink, libreoffice_text_ink

    rxls_blurred = _numpy_box_blur_luma_3px(rxls_luma, np)
    libreoffice_blurred = _numpy_box_blur_luma_3px(libreoffice_luma, np)
    blurred_absolute = int(
        np.abs(
            rxls_blurred.astype(np.int16) - libreoffice_blurred.astype(np.int16)
        ).sum(dtype=np.uint64)
    )
    blurred_mae = _ratio_ppm(blurred_absolute, pixels * 255)
    evidence.update(
        {
            "blurred_luma_absolute_error_sum": blurred_absolute,
            "blurred_luma_mean_absolute_error_ppm": blurred_mae,
            "blurred_luma_similarity_ppm": max(0, 1_000_000 - blurred_mae),
            "metric_work_units": work_units,
        }
    )
    return evidence


def visual_image_metrics(
    rxls_rgb: bytes,
    libreoffice_rgb: bytes,
    width: int,
    height: int,
    *,
    max_metric_work_units: int | None = None,
) -> dict[str, object]:
    """Compute deterministic metrics with a vectorized exact-equivalence fast path."""
    if _metric_numpy() is not None:
        return _visual_image_metrics_numpy(
            rxls_rgb,
            libreoffice_rgb,
            width,
            height,
            max_metric_work_units=max_metric_work_units,
        )
    return _visual_image_metrics_python(
        rxls_rgb,
        libreoffice_rgb,
        width,
        height,
        max_metric_work_units=max_metric_work_units,
    )


def _flatten_to_white(image: Any, Image: Any) -> Any:
    rgba = image.convert("RGBA")
    background = Image.new("RGBA", rgba.size, (255, 255, 255, 255))
    return Image.alpha_composite(background, rgba).convert("RGB")


def compare_pngs(
    left_path: Path,
    right_path: Path,
    *,
    max_page_pixels: int,
    max_metric_work_units: int | None = None,
) -> dict[str, object]:
    try:
        from PIL import Image
    except ImportError as error:
        raise HarnessError("pillow_missing") from error
    try:
        with Image.open(left_path) as source:
            left_size = source.size
            if source.n_frames != 1:
                raise HarnessError("raster_multiframe")
            if left_size[0] * left_size[1] > max_page_pixels:
                raise HarnessError("raster_page_pixel_limit")
            left = _flatten_to_white(source, Image)
        with Image.open(right_path) as source:
            right_size = source.size
            if source.n_frames != 1:
                raise HarnessError("raster_multiframe")
            if right_size[0] * right_size[1] > max_page_pixels:
                raise HarnessError("raster_page_pixel_limit")
            right = _flatten_to_white(source, Image)
    except HarnessError:
        raise
    except Exception as error:
        raise HarnessError("raster_invalid_png") from error

    width = max(left_size[0], right_size[0])
    height = max(left_size[1], right_size[1])
    if width * height > max_page_pixels:
        raise HarnessError("comparison_page_pixel_limit")
    if left.size != (width, height):
        canvas = Image.new("RGB", (width, height), (255, 255, 255))
        canvas.paste(left, (0, 0))
        left = canvas
    if right.size != (width, height):
        canvas = Image.new("RGB", (width, height), (255, 255, 255))
        canvas.paste(right, (0, 0))
        right = canvas
    metrics = visual_image_metrics(
        left.tobytes(),
        right.tobytes(),
        width,
        height,
        max_metric_work_units=max_metric_work_units,
    )
    return {
        "rxls_size": {"width": left_size[0], "height": left_size[1]},
        "libreoffice_size": {"width": right_size[0], "height": right_size[1]},
        "canvas_size": {"width": width, "height": height},
        **metrics,
    }


def _aggregate_mask_metrics(
    prefix: str,
    pages: Sequence[dict[str, object]],
    y_offsets: Sequence[int],
    *,
    geometry: bool,
) -> dict[str, object]:
    rxls_pixels = sum(int(page.get(f"{prefix}_rxls_pixels", 0)) for page in pages)
    libreoffice_pixels = sum(
        int(page.get(f"{prefix}_libreoffice_pixels", 0)) for page in pages
    )
    rxls_matched = sum(
        int(page.get(f"{prefix}_rxls_matched_1px", 0)) for page in pages
    )
    libreoffice_matched = sum(
        int(page.get(f"{prefix}_libreoffice_matched_1px", 0)) for page in pages
    )
    evidence: dict[str, object] = _prf_evidence(
        prefix,
        rxls_pixels,
        libreoffice_pixels,
        rxls_matched,
        libreoffice_matched,
    )
    if not geometry:
        return evidence

    def aggregate_side(side: str) -> dict[str, object]:
        count = 0
        x_sum = 0
        y_sum = 0
        bbox = _empty_bbox()
        for page, y_offset in zip(pages, y_offsets):
            page_count = int(page.get(f"{prefix}_{side}_pixels", 0))
            count += page_count
            x_sum += int(page.get(f"{prefix}_{side}_x_sum", 0))
            y_sum += int(page.get(f"{prefix}_{side}_y_sum", 0)) + (
                y_offset * page_count
            )
            page_bbox = page.get(f"{prefix}_{side}_bbox")
            if not isinstance(page_bbox, dict) or page_bbox.get("present") != 1:
                continue
            candidate = {
                "present": 1,
                "left": int(page_bbox["left"]),
                "top": int(page_bbox["top"]) + y_offset,
                "right": int(page_bbox["right"]),
                "bottom": int(page_bbox["bottom"]) + y_offset,
            }
            if bbox["present"] == 0:
                bbox = candidate
            else:
                bbox["left"] = min(bbox["left"], candidate["left"])
                bbox["top"] = min(bbox["top"], candidate["top"])
                bbox["right"] = max(bbox["right"], candidate["right"])
                bbox["bottom"] = max(bbox["bottom"], candidate["bottom"])
        return {"count": count, "x_sum": x_sum, "y_sum": y_sum, "bbox": bbox}

    evidence.update(
        _geometry_evidence(
            prefix,
            aggregate_side("rxls"),
            aggregate_side("libreoffice"),
        )
    )
    return evidence


def aggregate_page_metrics(
    pages: Sequence[dict[str, object]],
) -> dict[str, object]:
    pixels = sum(int(page["pixels"]) for page in pages)
    changed = sum(int(page["changed_pixels"]) for page in pages)
    absolute = sum(int(page["absolute_error_sum"]) for page in pages)
    squared = sum(int(page["squared_error_sum"]) for page in pages)
    max_delta = max((int(page["max_channel_delta"]) for page in pages), default=0)
    if pixels == 0:
        raise HarnessError("metric_empty_image")
    channel_denominator = pixels * 3 * 255
    squared_denominator = pixels * 3 * 255 * 255
    mae = (absolute * 1_000_000 + channel_denominator // 2) // channel_denominator
    mismatch = (changed * 1_000_000 + pixels // 2) // pixels
    rmse = math.isqrt((squared * 1_000_000_000_000) // squared_denominator)
    y_offsets = []
    stacked_height = 0
    stacked_width = 0
    page_dimension_mismatches = 0
    max_page_width_delta = 0
    max_page_height_delta = 0
    for page in pages:
        y_offsets.append(stacked_height)
        canvas = page.get("canvas_size")
        if isinstance(canvas, dict):
            width = int(canvas.get("width", 0))
            height = int(canvas.get("height", 0))
            stacked_width = max(stacked_width, width)
            stacked_height += height
        rxls_size = page.get("rxls_size")
        libreoffice_size = page.get("libreoffice_size")
        if isinstance(rxls_size, dict) and isinstance(libreoffice_size, dict):
            width_delta = abs(
                int(rxls_size.get("width", 0))
                - int(libreoffice_size.get("width", 0))
            )
            height_delta = abs(
                int(rxls_size.get("height", 0))
                - int(libreoffice_size.get("height", 0))
            )
            max_page_width_delta = max(max_page_width_delta, width_delta)
            max_page_height_delta = max(max_page_height_delta, height_delta)
            page_dimension_mismatches += int(width_delta != 0 or height_delta != 0)

    evidence: dict[str, object] = {
        "pages": len(pages),
        "pixels": pixels,
        "changed_pixels": changed,
        "mismatch_ppm": mismatch,
        "absolute_error_sum": absolute,
        "squared_error_sum": squared,
        "max_channel_delta": max_delta,
        "mean_absolute_error_ppm": mae,
        "root_mean_square_error_ppm": rmse,
        "similarity_ppm": max(0, 1_000_000 - mae),
        "exact_pages": sum(int(page["changed_pixels"]) == 0 for page in pages),
        "metric_work_units": sum(
            int(page.get("metric_work_units", 0)) for page in pages
        ),
        "page_dimension_mismatches": page_dimension_mismatches,
        "max_page_width_delta_pixels": max_page_width_delta,
        "max_page_height_delta_pixels": max_page_height_delta,
        "stacked_canvas_size": {
            "width": stacked_width,
            "height": stacked_height,
        },
    }
    for prefix, geometry in (
        ("foreground", True),
        ("edge", False),
        ("text_ink", True),
    ):
        evidence.update(
            _aggregate_mask_metrics(
                prefix,
                pages,
                y_offsets,
                geometry=geometry,
            )
        )

    color_samples = sum(
        int(page.get("foreground_matched_color_samples", 0)) for page in pages
    )
    color_absolute = sum(
        int(page.get("foreground_matched_color_absolute_error_sum", 0))
        for page in pages
    )
    color_mae = _ratio_ppm(color_absolute, color_samples * 3 * 255, empty=0)
    blurred_absolute = sum(
        int(page.get("blurred_luma_absolute_error_sum", 0)) for page in pages
    )
    blurred_mae = _ratio_ppm(blurred_absolute, pixels * 255)
    evidence.update(
        {
            "foreground_matched_color_samples": color_samples,
            "foreground_matched_color_absolute_error_sum": color_absolute,
            "foreground_matched_color_mean_absolute_error_ppm": color_mae,
            "foreground_matched_color_similarity_ppm": max(
                0, 1_000_000 - color_mae
            ),
            "blurred_luma_absolute_error_sum": blurred_absolute,
            "blurred_luma_mean_absolute_error_ppm": blurred_mae,
            "blurred_luma_similarity_ppm": max(0, 1_000_000 - blurred_mae),
        }
    )
    semantic_presence = ["semantic_exact" in page for page in pages]
    if any(semantic_presence) and not all(semantic_presence):
        raise HarnessError("semantic_metrics_incomplete")
    if all(semantic_presence):
        for prefix in ("token", "codepoint", "bigram"):
            rxls_items = sum(
                int(page[f"semantic_{prefix}_rxls_items"]) for page in pages
            )
            libreoffice_items = sum(
                int(page[f"semantic_{prefix}_libreoffice_items"])
                for page in pages
            )
            matched_items = sum(
                int(page[f"semantic_{prefix}_matched_items"])
                for page in pages
            )
            evidence.update(
                _semantic_ratio_evidence(
                    prefix, rxls_items, libreoffice_items, matched_items
                )
            )
        semantic_exact_pages = sum(int(page["semantic_exact"]) for page in pages)
        semantic_comparable_pages = sum(
            int(page["semantic_comparable"]) for page in pages
        )
        evidence.update(
            {
                "semantic_exact": int(semantic_exact_pages == len(pages)),
                "semantic_exact_pages": semantic_exact_pages,
                "semantic_page_mismatches": len(pages) - semantic_exact_pages,
                "semantic_comparable": int(
                    semantic_comparable_pages == len(pages)
                ),
                "semantic_comparable_pages": semantic_comparable_pages,
                "semantic_one_sided_empty_pages": sum(
                    int(page["semantic_one_sided_empty"]) for page in pages
                ),
            }
        )
    text_box_presence = ["text_box_candidate_items" in page for page in pages]
    if any(text_box_presence) and not all(text_box_presence):
        raise HarnessError("text_box_metrics_incomplete")
    if all(text_box_presence):
        candidates = 0
        matched = 0
        ambiguous = 0
        unmatched = 0
        histogram: Counter[int] = Counter()
        for page in pages:
            values = []
            for key in (
                "text_box_candidate_items",
                "text_box_matched_items",
                "text_box_ambiguous_items",
                "text_box_unmatched_items",
            ):
                value = page.get(key)
                if isinstance(value, bool) or not isinstance(value, int) or value < 0:
                    raise HarnessError("text_box_metrics")
                values.append(value)
            page_candidates, page_matched, page_ambiguous, page_unmatched = values
            rows = page.get("text_box_error_histogram_millipoints")
            if not isinstance(rows, list):
                raise HarnessError("text_box_histogram")
            page_histogram: Counter[int] = Counter()
            previous = -1
            for row in rows:
                if not isinstance(row, dict) or set(row) != {
                    "error_millipoints",
                    "count",
                }:
                    raise HarnessError("text_box_histogram")
                error = row["error_millipoints"]
                count = row["count"]
                if (
                    isinstance(error, bool)
                    or not isinstance(error, int)
                    or error <= previous
                    or isinstance(count, bool)
                    or not isinstance(count, int)
                    or count <= 0
                ):
                    raise HarnessError("text_box_histogram")
                previous = error
                page_histogram[error] = count
            if (
                page_candidates
                != page_matched + page_ambiguous + page_unmatched
                or sum(page_histogram.values()) != page_matched
            ):
                raise HarnessError("text_box_metrics")
            candidates += page_candidates
            matched += page_matched
            ambiguous += page_ambiguous
            unmatched += page_unmatched
            histogram.update(page_histogram)
        evidence.update(
            _text_box_numeric_evidence(
                candidates, matched, ambiguous, unmatched, histogram
            )
        )
    return evidence


def _base_result(case: InputCase, size: int | None) -> dict[str, object]:
    result: dict[str, object] = {
        "path": case.label,
        "bytes": size,
        "format": case.path.suffix.lower().lstrip("."),
    }
    if case.rights_tier is not None:
        result["rights_tier"] = case.rights_tier
    if case.features:
        result["features"] = list(case.features)
    return result


def _bounded_ooxml_members(path: Path) -> tuple[bytes, bytes]:
    """Read only the authored-print OOXML parts under strict archive bounds."""
    try:
        with zipfile.ZipFile(path) as archive:
            infos = archive.infolist()
            names = [info.filename for info in infos]
            if len(infos) > 4096 or len(names) != len(set(names)):
                raise HarnessError("authored_print_archive")
            selected = []
            for name in ("xl/workbook.xml", "xl/worksheets/sheet1.xml"):
                try:
                    info = archive.getinfo(name)
                except KeyError as error:
                    raise HarnessError("authored_print_part_missing") from error
                if (
                    info.flag_bits & 0x1
                    or info.file_size <= 0
                    or info.file_size > 4 * 1024 * 1024
                    or info.compress_size > 4 * 1024 * 1024
                ):
                    raise HarnessError("authored_print_part_limit")
                payload = archive.read(info)
                if len(payload) != info.file_size:
                    raise HarnessError("authored_print_part_truncated")
                upper = payload[:4096].upper()
                if b"<!DOCTYPE" in upper or b"<!ENTITY" in upper:
                    raise HarnessError("authored_print_part_unsafe")
                selected.append(payload)
    except (OSError, zipfile.BadZipFile, RuntimeError) as error:
        raise HarnessError("authored_print_archive") from error
    return selected[0], selected[1]


def _xml_local(element: ET.Element) -> str:
    return element.tag.rsplit("}", 1)[-1]


def attest_authored_print_source(case: InputCase) -> dict[str, object]:
    """Fail closed on the source metadata exercised by the authored-print lane."""
    if case.path.suffix.lower() not in {".xlsx", ".xlsm"}:
        raise HarnessError("authored_print_ooxml_required")
    if "print-settings" not in case.features:
        raise HarnessError("authored_print_feature_required")
    workbook_payload, sheet_payload = _bounded_ooxml_members(case.path)
    try:
        workbook = ET.fromstring(workbook_payload)
        sheet = ET.fromstring(sheet_payload)
    except ET.ParseError as error:
        raise HarnessError("authored_print_xml") from error

    def one(root: ET.Element, name: str) -> ET.Element:
        rows = [element for element in root.iter() if _xml_local(element) == name]
        if len(rows) != 1:
            raise HarnessError(f"authored_print_{name}")
        return rows[0]

    margins = one(sheet, "pageMargins")
    margin_names = ("left", "right", "top", "bottom", "header", "footer")
    if set(margins.attrib) != set(margin_names):
        raise HarnessError("authored_print_margins")
    margin_values = []
    for name in margin_names:
        try:
            value = float(margins.attrib[name])
        except (KeyError, ValueError) as error:
            raise HarnessError("authored_print_margins") from error
        if not math.isfinite(value) or not 0 <= value <= 10:
            raise HarnessError("authored_print_margins")
        margin_values.append(value)
    if margin_values != [0.5, 0.5, 0.75, 0.75, 0.2, 0.25]:
        raise HarnessError("authored_print_margins")

    setup = one(sheet, "pageSetup")
    try:
        paper_code = int(setup.attrib["paperSize"])
    except (KeyError, ValueError) as error:
        raise HarnessError("authored_print_paper") from error
    orientation = setup.attrib.get("orientation", "portrait")
    if paper_code != 1 or orientation != "portrait":
        raise HarnessError("authored_print_paper")
    has_scale = "scale" in setup.attrib
    has_fit = "fitToWidth" in setup.attrib or "fitToHeight" in setup.attrib
    fit_to_page = any(
        _xml_local(element) == "pageSetUpPr"
        and element.attrib.get("fitToPage") in {"1", "true"}
        for element in sheet.iter()
    )
    scale_contract = has_scale and not has_fit and not fit_to_page
    fit_contract = not has_scale and has_fit and fit_to_page
    if not (scale_contract or fit_contract):
        raise HarnessError("authored_print_scale_fit")
    if has_scale:
        try:
            scale = int(setup.attrib["scale"])
        except ValueError as error:
            raise HarnessError("authored_print_scale_fit") from error
        if not 10 <= scale <= 400:
            raise HarnessError("authored_print_scale_fit")
        if setup.attrib != {
            "orientation": "portrait",
            "paperSize": "1",
            "scale": "85",
            "pageOrder": "overThenDown",
        }:
            raise HarnessError("authored_print_scale_fit")
        scale_mode = "scale"
    else:
        try:
            fit_width = int(setup.attrib["fitToWidth"])
            fit_height = int(setup.attrib["fitToHeight"])
        except (KeyError, ValueError) as error:
            raise HarnessError("authored_print_scale_fit") from error
        if not 1 <= fit_width <= 100 or not 1 <= fit_height <= 100:
            raise HarnessError("authored_print_scale_fit")
        if setup.attrib != {
            "orientation": "portrait",
            "paperSize": "1",
            "fitToWidth": "2",
            "fitToHeight": "2",
            "pageOrder": "overThenDown",
        }:
            raise HarnessError("authored_print_scale_fit")
        scale_mode = "fit"

    break_counts = {}
    for kind in ("rowBreaks", "colBreaks"):
        parent = one(sheet, kind)
        breaks = [
            child
            for child in parent
            if _xml_local(child) == "brk"
            and child.attrib.get("man") in {"1", "true"}
        ]
        expected_break = (
            {"id": "8", "min": "0", "max": "16383", "man": "1"}
            if kind == "rowBreaks"
            else {"id": "3", "min": "0", "max": "1048575", "man": "1"}
        )
        if (
            parent.attrib != {"count": "1", "manualBreakCount": "1"}
            or len(breaks) != 1
            or breaks[0].attrib != expected_break
        ):
            raise HarnessError("authored_print_manual_breaks")
        break_counts[kind] = len(breaks)

    header_footer = one(sheet, "headerFooter")
    text_by_kind = {
        _xml_local(child): "".join(child.itertext()) for child in header_footer
    }
    header = text_by_kind.get("oddHeader", "")
    footer = text_by_kind.get("oddFooter", "")
    if (
        not header
        or not footer
        or "&P" not in header
        or "&N" not in header
        or "&P" not in footer
    ):
        raise HarnessError("authored_print_header_footer")

    defined_names = [
        (element.attrib.get("name"), "".join(element.itertext()))
        for element in workbook.iter()
        if _xml_local(element) == "definedName"
    ]
    if len(defined_names) != 2 or len({name for name, _ in defined_names}) != 2:
        raise HarnessError("authored_print_defined_names")
    names = dict(defined_names)
    print_area = names.get("_xlnm.Print_Area", "")
    print_titles = names.get("_xlnm.Print_Titles", "")
    if print_area != "Render!$A$1:$F$18":
        raise HarnessError("authored_print_area")
    if print_titles != "Render!$1:$1,Render!$F:$F":
        raise HarnessError("authored_print_titles")

    return {
        "expected_page_height_pixels": 1056,
        "expected_page_width_pixels": 816,
        "header_footer": True,
        "manual_col_breaks": break_counts["colBreaks"],
        "manual_row_breaks": break_counts["rowBreaks"],
        "margins": True,
        "paper_code": paper_code,
        "print_area": True,
        "repeated_cols": True,
        "repeated_rows": True,
        "scale_mode": scale_mode,
    }


def _classified(
    base: dict[str, object],
    status: str,
    classification: str,
    **extra: object,
) -> dict[str, object]:
    return {**base, "status": status, "classification": classification, **extra}


def _job_environment(
    root: Path, locale: str, font_pack: FontPack | None = None
) -> dict[str, str]:
    env = os.environ.copy()
    original_home = Path(env.get("HOME", str(Path.home())))
    home = root / "home"
    temporary = root / "tmp"
    home.mkdir(parents=True, exist_ok=True)
    temporary.mkdir(parents=True, exist_ok=True)
    env.update(
        {
            "HOME": str(home),
            "TMPDIR": str(temporary),
            "XDG_CACHE_HOME": str(root / "xdg-cache"),
            "XDG_CONFIG_HOME": str(root / "xdg-config"),
            "LANG": locale,
            "LC_ALL": locale,
            "SAL_DISABLE_OPENCL": "1",
            "SC_FORCE_CALCULATION": "core",
            "TZ": "UTC",
        }
    )
    if sys.platform.startswith("linux"):
        env["SAL_USE_VCLPLUGIN"] = "svp"
    else:
        # The generic X11 plugin is not available in headless macOS builds.
        env.pop("SAL_USE_VCLPLUGIN", None)
    # The default renderer command is Cargo-based.  Preserve explicitly pinned
    # local toolchain/cache roots while still isolating general HOME state and
    # LibreOffice's user profile.
    env.setdefault("CARGO_HOME", str(original_home / ".cargo"))
    env.setdefault("RUSTUP_HOME", str(original_home / ".rustup"))
    if font_pack is not None:
        env["FONTCONFIG_FILE"] = str(font_pack.fonts_conf)
        env["FONTCONFIG_PATH"] = str(font_pack.root)
        if sys.platform == "darwin":
            user_fonts = home / "Library" / "Fonts"
            user_fonts.mkdir(parents=True, exist_ok=True)
            for source in font_pack.font_paths:
                target = user_fonts / source.name
                try:
                    os.link(source, target)
                except OSError:
                    try:
                        target.symlink_to(source)
                    except OSError as error:
                        raise HarnessError("font_pack_activation") from error
    return env


def evaluate_case(
    case: InputCase,
    *,
    index: int,
    work_root: Path,
    config: HarnessConfig,
    backends: Backends,
    runner: CommandRunner,
) -> dict[str, object]:
    size, stat_error = _safe_stat(case.path)
    base = _base_result(case, size)
    if stat_error:
        return _classified(base, "skipped", stat_error)
    assert size is not None
    if size > config.caps.max_input_bytes:
        return _classified(base, "skipped", "input_limit")
    if case.expected_bytes is not None and size != case.expected_bytes:
        return _classified(base, "error", "manifest_size_mismatch")
    try:
        input_sha256 = _sha256_file(case.path, config.caps.max_input_bytes)
    except (OSError, HarnessError):
        return _classified(base, "skipped", "unreadable_input")
    base["sha256"] = input_sha256
    if case.expected_sha256 is not None and input_sha256 != case.expected_sha256:
        return _classified(base, "error", "manifest_sha256_mismatch")
    if config.print_mode == PRINT_MODE_AUTHORED:
        try:
            base["authored_print"] = attest_authored_print_source(case)
        except HarnessError as error:
            return _classified(base, "error", str(error))

    if config.dry_run:
        planned_rxls = [
            *normalized_command(config.rxls_command),
            "bundle",
            "<input>",
        ]
        if config.font_pack is not None:
            planned_rxls.extend(("--font-pack-manifest", "<font-pack-manifest>"))
        if config.print_mode == PRINT_MODE_SINGLE_PAGE:
            planned_rxls.append("--single-page-sheets")
        else:
            planned_rxls.extend(("--print-layout", "--print-backends", "svg"))
        planned_rxls.extend(("--output-dir", "<output-dir>"))
        if config.libreoffice_command is not None:
            planned_lo = normalized_command(
                build_libreoffice_oracle_command(
                    config.libreoffice_command,
                    Path("<input>"),
                    Path("<output-dir>"),
                    "case-0000-dry-run",
                    config.font_pack,
                    config.print_mode,
                )
            )
        else:
            planned_lo = [
                Path(config.libreoffice).name or config.libreoffice,
                "-env:UserInstallation=<profile-uri>",
                "--headless",
                "--nologo",
                "--nodefault",
                "--nolockcheck",
                "--norestore",
                "--convert-to",
                (
                    PDF_FILTER
                    if config.print_mode == PRINT_MODE_SINGLE_PAGE
                    else AUTHORED_PDF_FILTER
                ),
                "--outdir",
                "<output-dir>",
                "<input>",
            ]
        return _classified(
            base,
            "dry_run",
            "preflight_only",
            planned_commands={"rxls": planned_rxls, "libreoffice": planned_lo},
        )

    job = work_root / f"case-{index:04d}-{input_sha256[:12]}"
    bundle_dir = job / "rxls"
    libreoffice_dir = job / "libreoffice"
    profile_dir = job / "lo-profile"
    rxls_raster_dir = job / "rxls-raster"
    lo_raster_dir = job / "lo-raster"
    for directory in (bundle_dir, libreoffice_dir, profile_dir, rxls_raster_dir, lo_raster_dir):
        directory.mkdir(parents=True, exist_ok=True)
    if config.libreoffice_command is None:
        seed_libreoffice_profile(profile_dir)
    env = _job_environment(job, config.locale, config.font_pack)

    rxls_result = runner.run(
        build_rxls_command(
            config.rxls_command,
            case.path,
            bundle_dir,
            (
                config.font_pack.root / "manifest.json"
                if config.font_pack is not None
                else None
            ),
            config.print_mode,
        ),
        cwd=ROOT,
        env=env,
        timeout_seconds=config.caps.timeout_seconds,
        output_limit_bytes=config.caps.max_command_output_bytes,
    )
    rxls_fact = command_fact(rxls_result)
    rxls_failure = _command_failure("renderer", rxls_result)
    if rxls_failure:
        return _classified(base, "error", rxls_failure, commands={"rxls": rxls_fact})
    try:
        bundle = validate_bundle(
            bundle_dir,
            input_sha256=input_sha256,
            input_bytes=size,
            caps=config.caps,
            dpi=config.dpi,
            expected_font_pack_sha256=(
                config.font_pack.evidence["pack_sha256"]
                if config.font_pack is not None
                else None
            ),
            print_mode=config.print_mode,
        )
    except HarnessError as error:
        return _classified(
            base,
            "error",
            str(error),
            commands={"rxls": rxls_fact},
        )
    scene_evidence = [
        {
            "sheet_index": page.index,
            "sha256": page.scene_sha256,
            "warnings": [
                {
                    "code": code,
                    "occurrences": occurrences,
                    **({"first_cell": first_cell} if first_cell is not None else {}),
                }
                for code, occurrences, first_cell in page.warnings
            ],
        }
        for page in bundle.pages
    ]

    oracle_run_id = f"case-{index:04d}-{input_sha256[:12]}"
    lo_command = (
        build_libreoffice_oracle_command(
            config.libreoffice_command,
            case.path,
            libreoffice_dir,
            oracle_run_id,
            config.font_pack,
            config.print_mode,
        )
        if config.libreoffice_command is not None
        else build_libreoffice_command(
            config.libreoffice,
            case.path,
            libreoffice_dir,
            profile_dir,
            config.print_mode,
        )
    )
    lo_result = runner.run(
        lo_command,
        cwd=job,
        env=env,
        timeout_seconds=config.caps.timeout_seconds,
        output_limit_bytes=config.caps.max_command_output_bytes,
    )
    lo_fact = command_fact(lo_result)
    commands = {"rxls": rxls_fact, "libreoffice": lo_fact}
    if lo_result.status == "nonzero" and lo_result.returncode == 1:
        return _classified(
            base,
            "skipped",
            "libreoffice_oracle_rejected",
            renderer=bundle.renderer,
            scenes=scene_evidence,
            commands=commands,
        )
    lo_failure = _command_failure("libreoffice", lo_result)
    if lo_failure:
        return _classified(base, "error", lo_failure, commands=commands)
    oracle_adapter_evidence: dict[str, object] | None = None
    if config.libreoffice_command is not None:
        try:
            oracle_adapter_evidence = validate_libreoffice_adapter_output(
                libreoffice_dir,
                input_sha256=input_sha256,
                input_bytes=size,
                extension=case.path.suffix.lower(),
                font_pack_sha256=str(config.font_pack.evidence["pack_sha256"]),
                print_mode=config.print_mode,
            )
        except HarnessError as error:
            return _classified(
                base,
                "error",
                str(error),
                commands=commands,
            )
    try:
        pdfs = [
            path
            for path in _bounded_directory_files(libreoffice_dir)
            if path.suffix.lower() == ".pdf"
        ]
    except HarnessError as error:
        return _classified(base, "error", str(error), commands=commands)
    if len(pdfs) != 1:
        return _classified(base, "error", "libreoffice_pdf_count", commands=commands)
    pdf = pdfs[0]
    try:
        pdf_bytes = pdf.stat().st_size
        if bundle.artifact_bytes + pdf_bytes > config.caps.max_artifact_bytes:
            raise HarnessError("artifact_output_limit")
        with pdf.open("rb") as source:
            if pdf_bytes < 5 or source.read(5) != b"%PDF-":
                raise HarnessError("libreoffice_pdf_invalid")
    except OSError:
        return _classified(base, "error", "libreoffice_pdf_unreadable", commands=commands)
    except HarnessError as error:
        return _classified(base, "error", str(error), commands=commands)

    font_attestation: dict[str, object] | None = None
    if config.require_font_pack:
        if config.font_pack is None:
            return _classified(base, "error", "font_pack_required", commands=commands)
        font_result = _run_pdf_font_inspector(
            pdf,
            config=config,
            runner=runner,
            cwd=job,
            env=env,
        )
        font_fact = command_fact(font_result)
        commands["pdffonts"] = font_fact
        font_failure = _command_failure("libreoffice_font_inspector", font_result)
        if font_failure:
            return _classified(base, "error", font_failure, commands=commands)
        if font_result.stderr:
            return _classified(
                base,
                "error",
                "libreoffice_font_inspector_diagnostic",
                commands=commands,
            )
        try:
            font_records = parse_pdffonts_output(
                font_result.stdout,
                max_bytes=config.caps.max_command_output_bytes,
            )
            font_attestation = attest_pdf_fonts(font_records, config.font_pack)
        except HarnessError as error:
            return _classified(base, "error", str(error), commands=commands)
        if font_attestation["matched_font_objects"] != font_attestation["font_objects"]:
            return _classified(
                base,
                "different",
                "libreoffice_font_pack_mismatch",
                renderer=bundle.renderer,
                scenes=scene_evidence,
                commands=commands,
                font_attestation=font_attestation,
                **(
                    {"oracle_adapter": oracle_adapter_evidence}
                    if oracle_adapter_evidence is not None
                    else {}
                ),
            )

    missing = backends.missing(require_pdffonts=config.require_font_pack)
    if missing:
        return _classified(
            base,
            "skipped",
            "visual_dependencies_missing",
            missing_dependencies=missing,
            **(
                {"oracle_adapter": oracle_adapter_evidence}
                if oracle_adapter_evidence is not None
                else {}
            ),
            renderer=bundle.renderer,
            scenes=scene_evidence,
            commands=commands,
        )

    # Calc's SinglePageSheets export includes hidden worksheets, whereas its
    # authored print export emits visible sheets only. Preserve source/page
    # order while making that policy explicit and fail-closed in configuration.
    comparison_pages = [
        page
        for page in bundle.pages
        if config.print_mode == PRINT_MODE_SINGLE_PAGE or page.visibility == "visible"
    ]
    if not comparison_pages:
        return _classified(base, "error", "authored_print_no_visible_pages")
    scene_evidence = [
        {
            "sheet_index": output_index,
            "sha256": page.scene_sha256,
            "warnings": [
                {
                    "code": code,
                    "occurrences": occurrences,
                    **({"first_cell": first_cell} if first_cell is not None else {}),
                }
                for code, occurrences, first_cell in page.warnings
            ],
        }
        for output_index, page in enumerate(comparison_pages)
    ]
    rxls_pngs = []
    raster_facts = []
    for output_index, page in enumerate(comparison_pages):
        output = rxls_raster_dir / f"page-{output_index:04d}.png"
        result = _run_svg_rasterizer(
            page,
            output,
            config=config,
            backends=backends,
            runner=runner,
            cwd=job,
            env=env,
        )
        raster_facts.append(command_fact(result))
        failure = _command_failure("svg_rasterizer", result)
        if failure:
            return _classified(
                base,
                "error",
                failure,
                renderer=bundle.renderer,
                commands=commands,
                raster_commands=raster_facts,
            )
        if not output.is_file():
            return _classified(base, "error", "svg_raster_missing", commands=commands)
        rxls_pngs.append(output)

    pdf_result, pdf_manifest = _run_pdf_rasterizer(
        pdf,
        lo_raster_dir,
        config=config,
        backends=backends,
        runner=runner,
        cwd=job,
        env=env,
    )
    raster_facts.append(command_fact(pdf_result))
    if pdf_result.status != "ok":
        code_by_return = {
            20: "libreoffice_page_limit",
            21: "libreoffice_page_pixel_limit",
            22: "libreoffice_total_pixel_limit",
            23: "libreoffice_raster_output_limit",
        }
        classification = code_by_return.get(
            pdf_result.returncode,
            _command_failure("pdf_rasterizer", pdf_result) or "pdf_rasterizer_failed",
        )
        return _classified(
            base,
            "error",
            classification,
            renderer=bundle.renderer,
            commands=commands,
            raster_commands=raster_facts,
        )
    assert pdf_manifest is not None
    lo_rows = pdf_manifest["pages"]
    if len(lo_rows) != len(comparison_pages):
        return _classified(
            base,
            "error",
            "page_count_mismatch",
            rxls_pages=len(comparison_pages),
            libreoffice_pages=len(lo_rows),
            renderer=bundle.renderer,
            scenes=scene_evidence,
            commands=commands,
            raster_commands=raster_facts,
        )

    text_result = _run_pdf_text_extractor(
        pdf,
        len(comparison_pages),
        config=config,
        runner=runner,
        cwd=job,
        env=env,
    )
    text_fact = command_fact(text_result)
    text_failure = _command_failure("semantic_text", text_result)
    if text_failure:
        return _classified(
            base,
            "error",
            text_failure,
            renderer=bundle.renderer,
            scenes=scene_evidence,
            commands=commands,
            raster_commands=raster_facts,
            semantic_command=text_fact,
        )
    bbox_output = job / "pdftotext-bbox.xhtml"
    bbox_result = _run_pdf_bbox_extractor(
        pdf,
        len(comparison_pages),
        bbox_output,
        config=config,
        runner=runner,
        cwd=job,
        env=env,
    )
    bbox_fact = command_fact(bbox_result)
    bbox_failure = _command_failure("semantic_bbox", bbox_result)
    if bbox_failure:
        return _classified(
            base,
            "error",
            bbox_failure,
            renderer=bundle.renderer,
            scenes=scene_evidence,
            commands=commands,
            raster_commands=raster_facts,
            semantic_command=text_fact,
            text_box_command=bbox_fact,
        )
    try:
        if bbox_output.is_symlink() or not bbox_output.is_file():
            raise HarnessError("semantic_bbox_output_missing")
        bbox_bytes = bbox_output.stat().st_size
        if bbox_bytes <= 0 or bbox_bytes > config.caps.max_svg_bytes:
            raise HarnessError("semantic_bbox_output_limit")
        bbox_payload = bbox_output.read_bytes()
        libreoffice_semantic_pages = parse_pdftotext_pages(
            text_result.stdout,
            expected_pages=len(comparison_pages),
            max_codepoints=config.caps.max_semantic_codepoints,
            max_tokens=config.caps.max_semantic_tokens,
        )
        libreoffice_text_box_pages = parse_pdftotext_bbox_pages(
            bbox_payload,
            expected_pages=len(comparison_pages),
            max_bytes=config.caps.max_svg_bytes,
            max_codepoints=config.caps.max_semantic_codepoints,
            max_tokens=config.caps.max_semantic_tokens,
        )
        rxls_semantic_pages: list[SvgSemanticEvidence] = []
        rxls_codepoints_used = 0
        rxls_tokens_used = 0
        rxls_path_tokens_used = 0
        for page in comparison_pages:
            semantic_evidence = extract_svg_semantic_evidence(
                page.svg_path,
                max_svg_bytes=config.caps.max_svg_bytes,
                max_codepoints=(
                    config.caps.max_semantic_codepoints - rxls_codepoints_used
                ),
                max_tokens=config.caps.max_semantic_tokens - rxls_tokens_used,
                max_path_tokens=MAX_SVG_PATH_TOKENS - rxls_path_tokens_used,
            )
            rxls_codepoints_used += sum(
                len(token) for token in semantic_evidence.tokens
            )
            rxls_tokens_used += len(semantic_evidence.tokens)
            rxls_path_tokens_used += semantic_evidence.path_tokens
            rxls_semantic_pages.append(semantic_evidence)
    except (OSError, HarnessError) as error:
        classification = (
            str(error) if isinstance(error, HarnessError) else "semantic_bbox_unreadable"
        )
        return _classified(
            base,
            "error",
            classification,
            renderer=bundle.renderer,
            scenes=scene_evidence,
            commands=commands,
            raster_commands=raster_facts,
            semantic_command=text_fact,
            text_box_command=bbox_fact,
        )

    lo_pngs = []
    for offset, row in enumerate(lo_rows):
        expected = f"page-{offset:04d}.png"
        if not isinstance(row, dict) or row.get("file") != expected:
            return _classified(base, "error", "pdf_raster_manifest_invalid", commands=commands)
        path = lo_raster_dir / expected
        if not path.is_file() or path.is_symlink():
            return _classified(base, "error", "pdf_raster_missing", commands=commands)
        lo_pngs.append(path)

    try:
        all_artifacts = _bounded_directory_files(job)
        artifact_bytes = sum(path.stat().st_size for path in all_artifacts)
        if artifact_bytes > config.caps.max_artifact_bytes:
            raise HarnessError("artifact_output_limit")
        page_results = []
        total_pixels = 0
        total_metric_work_units = 0
        for page_offset, (page, rxls_png, lo_png) in enumerate(
            zip(comparison_pages, rxls_pngs, lo_pngs)
        ):
            metrics = compare_pngs(
                rxls_png,
                lo_png,
                max_page_pixels=config.caps.max_page_pixels,
                max_metric_work_units=(
                    config.caps.max_metric_work_units - total_metric_work_units
                ),
            )
            total_pixels += int(metrics["pixels"])
            if total_pixels > config.caps.max_total_pixels:
                raise HarnessError("comparison_total_pixel_limit")
            total_metric_work_units += int(metrics["metric_work_units"])
            if total_metric_work_units > config.caps.max_metric_work_units:
                raise HarnessError("metric_work_limit")
            semantic = semantic_text_metrics(
                rxls_semantic_pages[page_offset].tokens,
                libreoffice_semantic_pages[page_offset],
            )
            text_boxes = text_box_metrics(
                rxls_semantic_pages[page_offset],
                libreoffice_text_box_pages[page_offset],
            )
            page_results.append(
                {
                    "sheet_index": page_offset,
                    **metrics,
                    **semantic,
                    **text_boxes,
                }
            )
        aggregate = aggregate_page_metrics(page_results)
    except (OSError, HarnessError) as error:
        classification = str(error) if isinstance(error, HarnessError) else "artifact_unreadable"
        return _classified(base, "error", classification, commands=commands)

    status = "compared"
    classification = "within_threshold"
    if (
        aggregate.get("semantic_token_libreoffice_items") == 0
        and int(aggregate.get("semantic_token_rxls_items", 0)) > 0
        and int(aggregate.get("foreground_libreoffice_pixels", 0)) == 0
    ):
        status = "skipped"
        classification = "libreoffice_oracle_empty"
    elif aggregate.get("semantic_comparable") == 0:
        status = "different"
        classification = "semantic_content_one_sided"
    elif (
        config.min_similarity_ppm is not None
        and aggregate["similarity_ppm"] < config.min_similarity_ppm
    ):
        status = "different"
        classification = "below_similarity_threshold"
    return _classified(
        base,
        status,
        classification,
        renderer=bundle.renderer,
        scenes=scene_evidence,
        metrics=aggregate,
        pages=page_results,
        artifacts={
            "rxls_pages": len(rxls_pngs),
            "libreoffice_pages": len(lo_pngs),
        },
        commands=commands,
        raster_commands=raster_facts,
        semantic_command=text_fact,
        text_box_command=bbox_fact,
        **(
            {"font_attestation": font_attestation}
            if font_attestation is not None
            else {}
        ),
        **(
            {"oracle_adapter": oracle_adapter_evidence}
            if oracle_adapter_evidence is not None
            else {}
        ),
    )


def preflight(
    config: HarnessConfig,
    backends: Backends,
    oracle_evidence: dict[str, object] | None = None,
) -> dict[str, object]:
    libreoffice_available = (
        bool(config.libreoffice_command)
        and executable_available(config.libreoffice_command[0])
        if config.libreoffice_command is not None
        else executable_available(config.libreoffice)
    )
    return {
        "rxls_command": {
            "available": bool(config.rxls_command)
            and executable_available(config.rxls_command[0]),
            "tokens": normalized_command(config.rxls_command),
            "binary_identity": config.renderer_identity,
            "print_mode": config.print_mode,
        },
        "libreoffice": {
            "available": libreoffice_available,
            "mode": (
                "adapter_command"
                if config.libreoffice_command is not None
                else "direct_executable"
            ),
            "executable": (
                Path(config.libreoffice_command[0]).name
                if config.libreoffice_command is not None
                else Path(config.libreoffice).name or config.libreoffice
            ),
            "adapter_tokens": (
                normalized_command(config.libreoffice_command)
                if config.libreoffice_command is not None
                else None
            ),
            "profile_sha256": _sha256_file(ORACLE_PROFILE_PATH, 64 * 1024),
            "print_mode": config.print_mode,
        },
        "visual_backends": {
            "numpy": _metric_numpy() is not None,
            "pillow": backends.pillow,
            "pymupdf": backends.pymupdf,
            "pdftoppm": backends.pdftoppm,
            "pdfinfo": backends.pdfinfo,
            "pdftotext": backends.pdftotext,
            "pdffonts": backends.pdffonts,
            "cairosvg": backends.cairosvg,
            "svg_command": backends.svg_command is not None,
            "missing": backends.missing(
                require_pdffonts=config.require_font_pack
            ),
        },
        "font_pack": (
            {
                "attestation_required": config.require_font_pack,
                "configured": True,
                **config.font_pack.evidence,
            }
            if config.font_pack is not None
            else {
                "attestation_required": config.require_font_pack,
                "configured": False,
            }
        ),
        "oracle_lock": (
            {"configured": True, **oracle_evidence}
            if oracle_evidence is not None
            else {"configured": False}
        ),
    }


SCORE_METRICS = (
    "similarity_ppm",
    "blurred_luma_similarity_ppm",
    "foreground_precision_ppm",
    "foreground_recall_ppm",
    "foreground_f1_ppm",
    "text_ink_precision_ppm",
    "text_ink_recall_ppm",
    "text_ink_f1_ppm",
    "semantic_token_f1_ppm",
    "semantic_codepoint_f1_ppm",
    "semantic_bigram_f1_ppm",
    "text_box_match_coverage_ppm",
    "edge_f1_ppm",
    "foreground_matched_color_similarity_ppm",
)
DELTA_METRICS = (
    "page_dimension_mismatches",
    "max_page_width_delta_pixels",
    "max_page_height_delta_pixels",
    "foreground_bbox_alignment_max_delta_pixels",
    "foreground_centroid_distance_millipixels",
    "text_ink_bbox_alignment_max_delta_pixels",
    "text_ink_centroid_distance_millipixels",
    "semantic_page_mismatches",
    "text_box_ambiguous_items",
    "text_box_unmatched_items",
    "text_box_median_error_millipoints",
    "text_box_p95_error_millipoints",
)


def _nearest_rank(values: Sequence[int], numerator: int, denominator: int) -> int:
    ordered = sorted(values)
    rank = max(1, (len(ordered) * numerator + denominator - 1) // denominator)
    return ordered[min(len(ordered) - 1, rank - 1)]


def _distribution(values: Sequence[int], *, score: bool) -> dict[str, int]:
    if not values:
        raise HarnessError("empty_distribution")
    result = {
        "count": len(values),
        "mean": (sum(values) + len(values) // 2) // len(values),
        "min": min(values),
        "max": max(values),
    }
    if score:
        result["p10"] = _nearest_rank(values, 1, 10)
    else:
        result["p50"] = _nearest_rank(values, 1, 2)
        result["p90"] = _nearest_rank(values, 9, 10)
    return result


def _cohort_metrics(results: Sequence[dict[str, object]]) -> dict[str, object]:
    metric_bearing = [
        result
        for result in results
        if result.get("status") in {"compared", "different"}
        and isinstance(result.get("metrics"), dict)
    ]
    # A one-sided semantic page is not a rendering-fidelity observation: one
    # backend produced content while the other produced an empty page.  Keep
    # the file and its raw metrics in the report, but do not let white canvas
    # area inflate or depress the per-format/feature ratchets.  Older/mock
    # metric rows without semantic evidence remain comparable.
    def has_comparable_content(result: dict[str, object]) -> bool:
        metrics = result["metrics"]
        if metrics.get("semantic_comparable", 1) != 1:
            return False
        content_counts = (
            "semantic_token_rxls_items",
            "semantic_token_libreoffice_items",
            "foreground_rxls_pixels",
            "foreground_libreoffice_pixels",
        )
        # Mock/legacy rows that predate content counts retain their original
        # behavior.  When the evidence is present, an all-white/all-empty pair
        # is useful page-geometry evidence but not a fidelity score sample.
        return not all(
            key in metrics and int(metrics[key]) == 0 for key in content_counts
        )

    comparable = [result for result in metric_bearing if has_comparable_content(result)]
    scores = {}
    deltas = {}
    for key in SCORE_METRICS:
        values = [
            int(result["metrics"][key])
            for result in comparable
            if isinstance(result["metrics"].get(key), int)
        ]
        if values:
            scores[key] = _distribution(values, score=True)
    for key in DELTA_METRICS:
        values = [
            abs(int(result["metrics"][key]))
            for result in comparable
            if isinstance(result["metrics"].get(key), int)
        ]
        if values:
            deltas[key] = _distribution(values, score=False)
    return {
        "workbooks": len(results),
        "comparable_workbooks": len(comparable),
        "scores": scores,
        "deltas": deltas,
    }


def metric_cohorts(results: Sequence[dict[str, object]]) -> dict[str, object]:
    """Build bounded per-format and per-feature ratchet evidence."""
    by_format: dict[str, list[dict[str, object]]] = {}
    by_feature: dict[str, list[dict[str, object]]] = {}
    for result in results:
        format_name = result.get("format")
        if isinstance(format_name, str) and format_name:
            by_format.setdefault(format_name, []).append(result)
        features = result.get("features")
        if isinstance(features, list):
            for feature in features[:256]:
                if isinstance(feature, str) and feature:
                    by_feature.setdefault(feature, []).append(result)
    return {
        "all": _cohort_metrics(results),
        "by_format": {
            key: _cohort_metrics(value) for key, value in sorted(by_format.items())
        },
        "by_feature": {
            key: _cohort_metrics(value) for key, value in sorted(by_feature.items())
        },
    }


def run_harness(
    cases: Sequence[InputCase],
    *,
    discovery: dict[str, object],
    config: HarnessConfig,
    backends: Backends | None = None,
    runner: CommandRunner | None = None,
) -> tuple[dict[str, object], int]:
    if config.print_mode not in PRINT_MODES:
        raise HarnessError("print_mode")
    if config.print_mode == PRINT_MODE_AUTHORED and config.libreoffice_command is None:
        raise HarnessError("authored_print_requires_container_adapter")
    backends = backends or Backends.detect(config.svg_rasterizer_command)
    runner = runner or BoundedCommandRunner()
    oracle_evidence = (
        verify_oracle_profile(
            config.oracle_profile,
            config=config,
            backends=backends,
            runner=runner,
        )
        if config.oracle_profile is not None
        else None
    )
    results = []
    total_input_bytes = 0
    with tempfile.TemporaryDirectory(prefix="rxls-render-parity-") as raw_work:
        work_root = Path(raw_work)
        for index, case in enumerate(cases):
            size, _ = _safe_stat(case.path)
            if size is not None and total_input_bytes + size > config.caps.max_total_input_bytes:
                results.append(
                    _classified(
                        _base_result(case, size),
                        "skipped",
                        "corpus_input_budget_exceeded",
                    )
                )
                continue
            if size is not None:
                total_input_bytes += size
            results.append(
                evaluate_case(
                    case,
                    index=index,
                    work_root=work_root,
                    config=config,
                    backends=backends,
                    runner=runner,
                )
            )

    counts: dict[str, int] = {}
    classifications: dict[str, int] = {}
    for result in results:
        status = str(result["status"])
        classification = str(result["classification"])
        counts[status] = counts.get(status, 0) + 1
        classifications[classification] = classifications.get(classification, 0) + 1
    container_oracle_evidence = aggregate_container_oracle_identity(
        results,
        config=config,
    )
    effective_oracle_evidence = oracle_evidence or container_oracle_evidence
    authored_rows = [
        result["authored_print"]
        for result in results
        if isinstance(result.get("authored_print"), dict)
    ]
    authored_print_summary = None
    if config.print_mode == PRINT_MODE_AUTHORED:
        modes = Counter(str(row.get("scale_mode")) for row in authored_rows)
        authored_print_summary = {
            "attested_workbooks": len(authored_rows),
            "by_scale_mode": dict(sorted(modes.items())),
            "expected_page_box_pixels": {"height": 1056, "width": 816},
            "header_footer_workbooks": sum(row.get("header_footer") is True for row in authored_rows),
            "manual_break_workbooks": sum(
                int(row.get("manual_row_breaks", 0)) > 0
                and int(row.get("manual_col_breaks", 0)) > 0
                for row in authored_rows
            ),
            "margin_workbooks": sum(row.get("margins") is True for row in authored_rows),
            "paper_size_workbooks": sum(row.get("paper_code") == 1 for row in authored_rows),
            "repeated_title_workbooks": sum(
                row.get("repeated_rows") is True and row.get("repeated_cols") is True
                for row in authored_rows
            ),
        }
    evidence = {
        "schema": EVIDENCE_SCHEMA,
        "mode": "dry_run" if config.dry_run else "compare",
        "configuration": {
            "dpi": config.dpi,
            "locale": config.locale,
            "min_similarity_ppm": config.min_similarity_ppm,
            "print_mode": config.print_mode,
            "lane_filter": {
                "formats": list(config.format_filter),
                "required_features": list(config.required_feature_filter),
            },
            "caps": {
                "input_bytes": config.caps.max_input_bytes,
                "total_input_bytes": config.caps.max_total_input_bytes,
                "command_output_bytes": config.caps.max_command_output_bytes,
                "artifact_bytes": config.caps.max_artifact_bytes,
                "svg_bytes": config.caps.max_svg_bytes,
                "pages": config.caps.max_pages,
                "page_pixels": config.caps.max_page_pixels,
                "total_pixels": config.caps.max_total_pixels,
                "metric_work_units": config.caps.max_metric_work_units,
                "semantic_codepoints": config.caps.max_semantic_codepoints,
                "semantic_tokens": config.caps.max_semantic_tokens,
                "timeout_milliseconds": int(config.caps.timeout_seconds * 1000),
            },
            "metric_policy": {
                "foreground_channel_threshold": FOREGROUND_CHANNEL_THRESHOLD,
                "edge_luma_delta": EDGE_LUMA_DELTA,
                "text_ink_max_luma": TEXT_INK_MAX_LUMA,
                "mask_match_tolerance_pixels": 1,
                "metric_work_units_per_pixel": METRIC_WORK_UNITS_PER_PIXEL,
                "text_ink_is_ocr": False,
                "semantic_text_source": (
                    "svg_data-rxls-visible-label_vs_pdftotext_layout"
                ),
                "semantic_normalization": "unicode_nfc_whitespace_tokens",
                "semantic_content_retained": False,
                "semantic_ignored_codepoints": len(SEMANTIC_IGNORED_CODEPOINTS),
                "text_box_source": "svg_clipped_glyph_bounds_vs_pdftotext_bbox_layout",
                "text_box_matching": (
                    "exact_svg_data-rxls-visible-label_nearest_unique_pdftotext_bbox_layout"
                ),
                "text_box_error_units": "millipoints",
                "text_box_content_retained": False,
                "bounding_boxes_are_inclusive": True,
                "aggregate_pages_are_stacked_vertically": True,
                "centroid_units_per_pixel": 1000,
                "both_empty_mask_score_ppm": 1_000_000,
                "one_sided_empty_mask_score_ppm": 0,
                "implementation": _metric_implementation_evidence(),
            },
            "font_pack": (
                config.font_pack.evidence if config.font_pack is not None else None
            ),
            "oracle_lock": effective_oracle_evidence,
            "renderer_binary": config.renderer_identity,
        },
        "preflight": preflight(config, backends, effective_oracle_evidence),
        "discovery": discovery,
        "summary": {
            "files": len(results),
            "input_bytes_considered": total_input_bytes,
            "by_status": dict(sorted(counts.items())),
            "by_classification": dict(sorted(classifications.items())),
            "metric_cohorts": metric_cohorts(results),
            "authored_print": authored_print_summary,
        },
        "files": results,
    }
    has_error = counts.get("error", 0) > 0 or counts.get("different", 0) > 0
    incomparable = counts.get("skipped", 0) > 0
    exit_code = 1 if has_error or (config.fail_on_incomparable and incomparable) else 0
    return evidence, exit_code


def _positive_int(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be greater than zero")
    return parsed


def _nonnegative_ppm(value: str) -> int:
    parsed = int(value)
    if not 0 <= parsed <= 1_000_000:
        raise argparse.ArgumentTypeError("must be between 0 and 1000000")
    return parsed


def _command_tokens(value: str, option: str) -> tuple[str, ...]:
    try:
        tokens = tuple(shlex.split(value, posix=(os.name != "nt")))
    except ValueError as error:
        raise HarnessError(f"{option}_invalid") from error
    if not tokens:
        raise HarnessError(f"{option}_empty")
    return tokens


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--corpus", type=Path, help="directory scanned for workbooks")
    source.add_argument(
        "--manifest",
        type=Path,
        help=(
            "render-corpus or public-corpus JSON manifest; render-corpus rows "
            "are selected fail-closed by eligibility/status"
        ),
    )
    parser.add_argument(
        "--rxls-command",
        default=os.environ.get(
            "RXLS_RENDER_COMMAND",
            "cargo run --quiet --manifest-path render/Cargo.toml --",
        ),
        help=(
            "shell-like command prefix; the harness appends a `bundle` "
            "invocation plus the verified font-pack manifest when configured"
        ),
    )
    parser.add_argument(
        "--renderer-binary-sha256",
        help="expected SHA-256 for a direct rxls-render executable command",
    )
    parser.add_argument(
        "--require-renderer-binary-identity",
        action="store_true",
        help="reject Cargo/wrapper commands and require an exactly hashed renderer binary",
    )
    parser.add_argument(
        "--libreoffice",
        default=os.environ.get("LIBREOFFICE", "soffice"),
        help="LibreOffice/soffice executable",
    )
    parser.add_argument(
        "--libreoffice-command",
        help=(
            "optional shell-like offline oracle adapter using {input}, "
            "{output_dir}, {run_id}, and {font_pack}; for example the locked "
            "container wrapper. The command is executed directly without a shell"
        ),
    )
    parser.add_argument(
        "--svg-rasterizer-command",
        help=(
            "optional shell-like command template using {input}, {output}, "
            "{width}, {height}, and/or {dpi}; defaults to local CairoSVG"
        ),
    )
    parser.add_argument(
        "--font-pack-manifest",
        type=Path,
        help=(
            "verified rxls.render-font-pack.v1 manifest; configures Fontconfig "
            "and records only path-neutral font identities"
        ),
    )
    parser.add_argument(
        "--require-font-pack",
        action="store_true",
        help=(
            "fail closed unless the font-pack manifest is valid and every "
            "LibreOffice PDF font is an embedded, subset, Unicode-mapped pack font"
        ),
    )
    parser.add_argument(
        "--pdffonts-binary-sha256",
        help=(
            "expected SHA-256 of the active pdffonts executable; required for "
            "the offline container oracle adapter"
        ),
    )
    parser.add_argument(
        "--oracle-lock",
        type=Path,
        help="verified rxls.render-oracle-lock.v1 tool identity manifest",
    )
    parser.add_argument(
        "--oracle-profile",
        help="named profile from --oracle-lock; defaults to the manifest default",
    )
    parser.add_argument(
        "--require-oracle-lock",
        action="store_true",
        help="fail before rendering unless a complete oracle lock is configured",
    )
    parser.add_argument(
        "--print-mode",
        choices=tuple(sorted(PRINT_MODES)),
        default=PRINT_MODE_SINGLE_PAGE,
        help="compare one-page-per-sheet output or retain authored pagination",
    )
    parser.add_argument(
        "--format",
        dest="formats",
        action="append",
        choices=tuple(sorted(suffix.lstrip(".") for suffix in SUPPORTED_EXTENSIONS)),
        default=[],
        help="restrict the lane to a workbook format; may be repeated",
    )
    parser.add_argument(
        "--required-feature",
        action="append",
        default=[],
        help="require a sorted manifest feature tag; may be repeated",
    )
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--report", type=Path, help="write JSON evidence here")
    parser.add_argument("--max-files", type=_positive_int, default=1000)
    parser.add_argument("--max-candidates", type=_positive_int, default=10_000)
    parser.add_argument("--shard-count", type=_positive_int, default=1)
    parser.add_argument("--shard-index", type=int, default=0)
    parser.add_argument("--max-manifest-bytes", type=_positive_int, default=16 * 1024 * 1024)
    parser.add_argument("--max-input-bytes", type=_positive_int, default=64 * 1024 * 1024)
    parser.add_argument(
        "--max-total-input-bytes", type=_positive_int, default=2 * 1024 * 1024 * 1024
    )
    parser.add_argument(
        "--max-command-output-bytes", type=_positive_int, default=1024 * 1024
    )
    parser.add_argument(
        "--max-artifact-bytes", type=_positive_int, default=256 * 1024 * 1024
    )
    parser.add_argument("--max-svg-bytes", type=_positive_int, default=64 * 1024 * 1024)
    parser.add_argument("--max-pages", type=_positive_int, default=64)
    parser.add_argument("--max-page-pixels", type=_positive_int, default=40_000_000)
    parser.add_argument("--max-total-pixels", type=_positive_int, default=200_000_000)
    parser.add_argument(
        "--max-metric-work-units", type=_positive_int, default=25_600_000_000
    )
    parser.add_argument(
        "--max-semantic-codepoints", type=_positive_int, default=1_000_000
    )
    parser.add_argument(
        "--max-semantic-tokens", type=_positive_int, default=250_000
    )
    parser.add_argument("--timeout-seconds", type=float, default=60.0)
    parser.add_argument("--dpi", type=_positive_int, default=96)
    parser.add_argument("--locale", default="C.UTF-8")
    parser.add_argument("--min-similarity-ppm", type=_nonnegative_ppm)
    parser.add_argument("--fail-on-incomparable", action="store_true")
    args = parser.parse_args(argv)
    if not math.isfinite(args.timeout_seconds) or args.timeout_seconds <= 0:
        parser.error("--timeout-seconds must be finite and greater than zero")
    if not LOCALE_RE.fullmatch(args.locale):
        parser.error("--locale must be a locale identifier, not a path or command")
    if args.shard_index < 0 or args.shard_index >= args.shard_count:
        parser.error("--shard-index must be in [0, --shard-count)")
    return args


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        rxls_command = _command_tokens(args.rxls_command, "rxls_command")
        libreoffice_command = (
            _command_tokens(args.libreoffice_command, "libreoffice_command")
            if args.libreoffice_command
            else None
        )
        svg_command = (
            _command_tokens(args.svg_rasterizer_command, "svg_rasterizer_command")
            if args.svg_rasterizer_command
            else None
        )
        if svg_command and not any("{input}" in token for token in svg_command):
            raise HarnessError("svg_rasterizer_command_requires_input")
        if svg_command and not any("{output}" in token for token in svg_command):
            raise HarnessError("svg_rasterizer_command_requires_output")
        if args.require_font_pack and args.font_pack_manifest is None:
            raise HarnessError("font_pack_required")
        if args.require_oracle_lock and args.oracle_lock is None:
            raise HarnessError("oracle_lock_required")
        if args.oracle_profile is not None and args.oracle_lock is None:
            raise HarnessError("oracle_profile_requires_lock")
        if libreoffice_command is not None and args.oracle_lock is not None:
            raise HarnessError("libreoffice_command_uses_container_lock")
        if libreoffice_command is not None and args.pdffonts_binary_sha256 is None:
            raise HarnessError("pdffonts_binary_identity_required")
        if libreoffice_command is None and args.pdffonts_binary_sha256 is not None:
            raise HarnessError("pdffonts_binary_identity_adapter_only")
        font_pack = (
            load_font_pack(args.font_pack_manifest)
            if args.font_pack_manifest is not None
            else None
        )
        if libreoffice_command is not None:
            build_libreoffice_oracle_command(
                libreoffice_command,
                Path("input.xlsx"),
                Path("oracle-output"),
                "preflight",
                font_pack,
                args.print_mode,
            )
        oracle_profile = (
            load_oracle_profile(args.oracle_lock, args.oracle_profile)
            if args.oracle_lock is not None
            else None
        )
        renderer_identity = renderer_binary_identity(
            rxls_command,
            args.renderer_binary_sha256,
            required=(
                args.require_renderer_binary_identity or args.require_oracle_lock
            ),
        )
        pdffonts_identity = pdffonts_binary_identity(
            args.pdffonts_binary_sha256,
            required=(libreoffice_command is not None),
        )
        if args.corpus is not None:
            cases, discovery = discover_corpus(
                args.corpus,
                max_candidates=args.max_candidates,
                max_files=args.max_candidates,
            )
        else:
            cases, discovery = discover_manifest(
                args.manifest,
                max_manifest_bytes=args.max_manifest_bytes,
                max_candidates=args.max_candidates,
                max_files=args.max_candidates,
            )
        cases, discovery = filter_cases(
            cases,
            discovery,
            formats=args.formats,
            required_features=args.required_feature,
        )
        cases, discovery = select_shard(
            cases,
            discovery,
            shard_count=args.shard_count,
            shard_index=args.shard_index,
            max_files=args.max_files,
        )
        if not cases:
            raise HarnessError("no_workbooks_selected")
        caps = Caps(
            max_input_bytes=args.max_input_bytes,
            max_total_input_bytes=args.max_total_input_bytes,
            max_command_output_bytes=args.max_command_output_bytes,
            max_artifact_bytes=args.max_artifact_bytes,
            max_svg_bytes=args.max_svg_bytes,
            max_pages=args.max_pages,
            max_page_pixels=args.max_page_pixels,
            max_total_pixels=args.max_total_pixels,
            max_metric_work_units=args.max_metric_work_units,
            max_semantic_codepoints=args.max_semantic_codepoints,
            max_semantic_tokens=args.max_semantic_tokens,
            timeout_seconds=args.timeout_seconds,
        )
        config = HarnessConfig(
            rxls_command=rxls_command,
            libreoffice=args.libreoffice,
            svg_rasterizer_command=svg_command,
            caps=caps,
            dpi=args.dpi,
            locale=args.locale,
            dry_run=args.dry_run,
            min_similarity_ppm=args.min_similarity_ppm,
            fail_on_incomparable=args.fail_on_incomparable,
            require_font_pack=(args.require_font_pack or oracle_profile is not None),
            font_pack=font_pack,
            oracle_profile=oracle_profile,
            renderer_identity=renderer_identity,
            libreoffice_command=libreoffice_command,
            pdffonts_identity=pdffonts_identity,
            print_mode=args.print_mode,
            format_filter=tuple(sorted(set(args.formats))),
            required_feature_filter=tuple(sorted(set(args.required_feature))),
        )
        evidence, exit_code = run_harness(cases, discovery=discovery, config=config)
        rendered = json.dumps(evidence, indent=2, sort_keys=True) + "\n"
        if args.report:
            args.report.parent.mkdir(parents=True, exist_ok=True)
            args.report.write_text(rendered, encoding="utf-8")
        else:
            sys.stdout.write(rendered)
        return exit_code
    except HarnessError as error:
        print(f"libreoffice-render-parity: {error}", file=sys.stderr)
        return 2
    except OSError:
        print("libreoffice-render-parity: filesystem_error", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
