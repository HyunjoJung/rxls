#!/usr/bin/env python3
"""Enforce immutable GitHub Actions and reproducible release tool versions."""

from __future__ import annotations

import argparse
import ast
import hashlib
import re
import shlex
import sys
from pathlib import Path


ACTION_RE = re.compile(
    r"^\s*(?:-\s*)?uses:\s*[\"']?(?P<spec>[^\s\"'#]+)[\"']?"
    r"(?:\s+#\s*(?P<comment>.+?))?\s*$"
)
FULL_SHA_RE = re.compile(r"[0-9a-f]{40}")
REMOTE_ACTION_RE = re.compile(r"[^/\s]+/[^@\s]+@.+")
RELEASE_VERSIONS = {
    "RELEASE_RUST_VERSION": "1.96.1",
    "FUZZ_NIGHTLY_VERSION": "nightly-2026-07-10",
    "CARGO_FUZZ_VERSION": "0.13.2",
}
SEMVER_CHECKS_VERSION = "0.49.0"
SEMVER_BASELINE_VERSION = "0.1.2"
SEMVER_RELEASE_TYPE = "patch"
SEMVER_FEATURE_MODES = (
    "--all-features",
    "--default-features",
    "--only-explicit-features",
)
ADDITIONAL_FEATURE_CLIPPY_COMMANDS = (
    "cargo clippy --all-targets --no-default-features --features cli --locked -- -D warnings",
    "cargo clippy --all-targets --no-default-features --features cli,xlsb --locked -- -D warnings",
    "cargo clippy --all-targets --no-default-features --features cli,ods --locked -- -D warnings",
    "cargo clippy --manifest-path bindings/wasm/Cargo.toml --all-targets --target "
    "wasm32-unknown-unknown --locked -- -D warnings",
    "cargo clippy --manifest-path bindings/render-wasm/Cargo.toml --all-targets --target "
    "wasm32-unknown-unknown --locked -- -D warnings",
)
MCP_CI_COMMANDS = (
    "python3 scripts/render_supply_chain.py notice --profile mcp --check "
    "bindings/mcp/THIRD_PARTY_NOTICES.txt",
    "cargo fmt --manifest-path bindings/mcp/Cargo.toml -- --check",
    "cargo clippy --manifest-path bindings/mcp/Cargo.toml --all-targets --locked -- -D warnings",
    "cargo test --manifest-path bindings/mcp/Cargo.toml --locked",
    "cargo doc --manifest-path bindings/mcp/Cargo.toml --no-deps --locked",
    "cargo build --manifest-path bindings/mcp/Cargo.toml --release --locked",
    "cargo package --manifest-path bindings/mcp/Cargo.toml --locked",
)
RENDER_ORACLE_PYTHON_VERSION = "3.13.14"
RENDER_ORACLE_FULL_CASES = "800"
RENDER_ORACLE_DIAGNOSTIC_CASES = "34"
RENDER_ORACLE_FULL_REPEATS = "2"
RENDER_ORACLE_FULL_SHARDS = "4"
RENDER_ORACLE_MAX_PARALLEL_SHARDS = "2"
RENDER_PACKAGE_NODE_VERSION = "24.18.0"
RENDER_PACKAGE_NPM_VERSION = "11.16.0"
RENDER_PACKAGE_WASM_BINDGEN_BUILD_RUST = "1.88.0"
RENDER_PACKAGE_WASM_BINDGEN_VERSION = "0.2.126"
REVIEWED_ACTION_ALLOWLIST = {
    "actions/setup-node": (
        "820762786026740c76f36085b0efc47a31fe5020",
        "v7.0.0",
    ),
}
ORACLE_BUILDX_VERSION = "v0.35.0"
ORACLE_PR_PILOT_LABEL = "rxls-render-oracle-pilot"
ORACLE_PR_FULL_LABEL = "rxls-render-oracle-full"
PR_HEAD_EXPRESSION = "${{ github.event.pull_request.head.sha || github.sha }}"
ORACLE_SOURCE_SHA_EXPRESSION = (
    "${{ github.event_name == 'workflow_call' && inputs.source_sha || "
    "github.event.pull_request.head.sha || github.sha }}"
)
ORACLE_HARDENED_SOURCE_VERIFIER = "\n".join(
    (
        "set -euo pipefail",
        'test "$(git rev-parse HEAD)" = "$EXPECTED_SHA"',
        "git diff --exit-code",
        "git diff --cached --exit-code",
    )
)
ORACLE_PR_JOB_CONDITION = (
    "${{ github.event_name != 'pull_request' || "
    "(github.event.action == 'labeled' && "
    f"(github.event.label.name == '{ORACLE_PR_PILOT_LABEL}' || "
    f"github.event.label.name == '{ORACLE_PR_FULL_LABEL}') && "
    "github.event.pull_request.head.repo.full_name == github.repository) }}"
)
ORACLE_CAMPAIGN_EXPRESSION = (
    "${{ github.event_name == 'pull_request' && "
    f"github.event.label.name == '{ORACLE_PR_FULL_LABEL}' && 'full' || "
    "github.event_name == 'pull_request' && "
    f"github.event.label.name == '{ORACLE_PR_PILOT_LABEL}' && 'pilot' || "
    "(github.event_name == 'workflow_dispatch' || "
    "github.event_name == 'workflow_call') && inputs.campaign || 'pilot' }}"
)
ORACLE_BASELINE_MODE_EXPRESSION = (
    "${{ (github.event_name == 'workflow_dispatch' || "
    "github.event_name == 'workflow_call') && inputs.baseline_mode || 'verify' }}"
)
ORACLE_BOOTSTRAP_EXPRESSION = (
    "${{ (github.event_name == 'workflow_dispatch' || "
    "github.event_name == 'workflow_call') && "
    "inputs.bootstrap_identities && '1' || '0' }}"
)
ORACLE_TIMEOUT_EXPRESSION = (
    "${{ ((github.event_name == 'pull_request' && "
    f"github.event.label.name == '{ORACLE_PR_FULL_LABEL}') || "
    "((github.event_name == 'workflow_dispatch' || "
    "github.event_name == 'workflow_call') && inputs.campaign == 'full')) "
    "&& 330 || 120 }}"
)
ORACLE_BUILDX_ACTION = (
    "docker/setup-buildx-action@bb05f3f5519dd87d3ba754cc423b652a5edd6d2c"
)
ORACLE_CHECKOUT_ACTION = "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1"
ORACLE_SETUP_PYTHON_ACTION = (
    "actions/setup-python@5fda3b95a4ea91299a34e894583c3862153e4b97"
)
ORACLE_UPLOAD_ARTIFACT_ACTION = (
    "actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a"
)
ORACLE_RENDER_STEP_SHA256 = (
    "042c5a9253170bd88c6496aeb307b42cb6a0ef855bdafa8e50c3eee8a3b4ba4d",
    "bb2ff7258a91fd630b1cb20e19c8276a625f295c6f52b38fe37fc4e2424e9933",
    "1266e4280f579884aef9895f70988b7a58ac80f791e5b1dae6d63bfa1b001ede",
    "f14b7dfb812098fb6d42a40b09a3eeae8fc9f8be29182170c2ae3db46477c6bb",
    "244969ec54f80c9359028bdb8fd31aabe28df43f31ce5f2ef84ec54e1a8aa129",
    "736adb4fbe36521a6ca77d28b07fa4a62106b89cca089c048893a00b712ef2ab",
    "4ec3ef9024cf7eb628ff1c524024eab211d981f4e9af9b2be97d3a3f8b454951",
    "3d924376e08eb1ecbe1718d01de461fb6d6e652d8760ead1d941bb66d785aba2",
    "0308865d11b5e8e1a6d43e19a0b5f0b942799aef63ba811d05fb0eaaec5687bc",
    "91555206ce7c99be03b1c37f9f8e174b1aec49fbf5e9f920cda7cfe5e14dbce4",
    "dc1c0348112f956e76f4efb6c9181277c6f2a155064281ef8bf08f111da4d61b",
    "dcb70c3f452ab5c7075315dbce68c38ec2da7a20ab20a22da21a2d728faa5ef3",
    "012583aec1469514a63a3616e1f8a4dd35483a2c8284831392db789c8eeaefb0",
    "dd06bf10233cf70a9dc797223cf5c3a76ebe561124a1d9db06f112983e0321b8",
    "a045ad7115eaf2b15ce19e33ff630c3716b62ab1e615dfbeb8a9a9dfac65b1ea",
    "cfc561662aad1b88ce6bcfc1387c7ebe5622025d25a7621125a0a1bc7b4d0bdc",
    "fc38b091078736b27ab95d43d5ceb8a91477e68a7a6e0b8c0261817882c68dec",
    "940f6c80f0324bc5969d03134a1d1e5448c7c8c9f455cb5979a23a81cc9b2ce0",
    "8e5d8438decff5f4995ff3a6a7681a5f709b2be9c4752f38c68fcef59adc0c24",
    "9acefc9320cb53ab9c51a58ec9b556dadfec1a4545615b2644392cee13e7582c",
    "0277dad1011ca57308140daf2434fa5dd9e2ef4a9936ed041193347904838eb4",
    "bdd84b925ad854145d404645230fb7fc341be69b76481a8fc6453b829d284bb6",
    "5f4113d9afc22d73ccc488156e0f3abcea689b74a2b261c7e8522b338029a4e6",
)
ORACLE_HARDENING_IMAGE_STEP_SHA256 = (
    "1474c388488c7cc317f8dc2b1948415ebbc06415499cde15213b8e7369b6b2d9",
    "974a8f3bf55df0faabfb0d3bbbf0bd87a9692941a3c7f2d619bd9916694bcda5",
    "244969ec54f80c9359028bdb8fd31aabe28df43f31ce5f2ef84ec54e1a8aa129",
    "5eb296aeb7a081fef5622668a2658e484191f93958a318518d4253a22f92d2bc",
    "7aa2fb46f8d33f6abd1ad0795d7c76aacaf8d47ada5305762a458ac180acad64",
    "43d6bfd32a185411e10497a570623fec6e09413f8be78adcae671f8516b43b79",
)
ORACLE_RENDER_WORKFLOW_SHA256 = (
    "dba690b0defabe7bdb4f651fa38c6ece0b0d4ece6f49919e6fe7d71046f2f6a9"
)
ORACLE_HARDENING_WORKFLOW_SHA256 = (
    "ac477662896b26fef0fb4bfe292efcb2ff1cce2f09fb76e03b43da42143ec152"
)
RENDER_PACKAGE_RELEASE_WORKFLOW_SHA256 = (
    "b125148dde44cb51b9e569c19eccce2b1be9a6dc74e4a9ea52c228d01c4bf6ca"
)
WASM_PACKAGE_RELEASE_WORKFLOW_SHA256 = (
    "02b6d4b68f43dd18d1f2b1c16165332b19346ba27480feea03ecbedfcd2cf3c2"
)
ORACLE_BUILDKIT_IMAGE = (
    "docker.io/moby/buildkit:v0.31.2@sha256:"
    "2f5adac4ecd194d9f8c10b7b5d7bceb5186853db1b26e5abd3a657af0b7e26ec"
)


def _without_commented_lines(text: str) -> str:
    """Remove fully commented lines while preserving YAML indentation and blocks."""

    return "\n".join(
        "" if line.lstrip().startswith("#") else line for line in text.splitlines()
    )


def _strip_yaml_inline_comment(text: str) -> str:
    """Strip a YAML comment without treating a hash inside quotes as a comment."""

    quote: str | None = None
    escaped = False
    for index, character in enumerate(text):
        if quote == '"':
            if escaped:
                escaped = False
            elif character == "\\":
                escaped = True
            elif character == quote:
                quote = None
        elif quote == "'":
            if character == quote:
                if index + 1 < len(text) and text[index + 1] == quote:
                    continue
                quote = None
        elif character in {"'", '"'}:
            quote = character
        elif character == "#" and (index == 0 or text[index - 1].isspace()):
            return text[:index].rstrip()
    return text.rstrip()


def _yaml_scalar_name(text: str) -> str | None:
    """Return a simple YAML string scalar used for workflow trigger names."""

    value = _strip_yaml_inline_comment(text).strip()
    if not value:
        return None
    if value.startswith("'") and value.endswith("'") and len(value) >= 2:
        return value[1:-1].replace("''", "'")
    if value.startswith('"') and value.endswith('"') and len(value) >= 2:
        inner = value[1:-1]
        if "\\" in inner:
            return None
        return inner
    if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_-]*", value):
        return value
    return None


def _yaml_top_level_parts(text: str, separator: str) -> list[str] | None:
    """Split one YAML flow collection outside quotes and nested collections."""

    parts: list[str] = []
    start = 0
    stack: list[str] = []
    quote: str | None = None
    escaped = False
    pairs = {"]": "[", "}": "{"}
    for index, character in enumerate(text):
        if quote == '"':
            if escaped:
                escaped = False
            elif character == "\\":
                escaped = True
            elif character == quote:
                quote = None
            continue
        if quote == "'":
            if character == quote:
                if index + 1 < len(text) and text[index + 1] == quote:
                    continue
                quote = None
            continue
        if character in {"'", '"'}:
            quote = character
        elif character in "[{":
            stack.append(character)
        elif character in "]}":
            if not stack or stack.pop() != pairs[character]:
                return None
        elif character == separator and not stack:
            parts.append(text[start:index].strip())
            start = index + 1
    if quote is not None or stack:
        return None
    parts.append(text[start:].strip())
    return parts


def _yaml_mapping_entry(text: str) -> tuple[str, str] | None:
    """Parse a simple YAML mapping entry, including quoted keys."""

    parts = _yaml_top_level_parts(text, ":")
    if parts is None or len(parts) < 2:
        return None
    key = _yaml_scalar_name(parts[0])
    if key is None:
        return None
    return key, ":".join(parts[1:]).strip()


def _trigger_names_from_value(value: str) -> set[str] | None:
    """Parse the scalar, flow-sequence, or flow-map form of a workflow trigger."""

    value = _strip_yaml_inline_comment(value).strip()
    if value in {"", "~", "null", "Null", "NULL"}:
        return set()
    if value.startswith("[") and value.endswith("]"):
        parts = _yaml_top_level_parts(value[1:-1], ",")
        if parts is None:
            return None
        names = {_yaml_scalar_name(part) for part in parts if part}
        return None if None in names else {name for name in names if name is not None}
    if value.startswith("{") and value.endswith("}"):
        parts = _yaml_top_level_parts(value[1:-1], ",")
        if parts is None:
            return None
        names: set[str] = set()
        for part in parts:
            if not part:
                continue
            entry = _yaml_mapping_entry(part)
            if entry is None:
                return None
            names.add(entry[0])
        return names
    name = _yaml_scalar_name(value)
    return None if name is None else {name}


def _workflow_trigger_names(text: str) -> tuple[set[str], list[str]]:
    """Read the top-level GitHub Actions trigger using YAML mapping semantics."""

    lines = text.splitlines()
    on_entries: list[tuple[int, str]] = []
    for index, line in enumerate(lines):
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if len(line) != len(line.lstrip(" ")):
            continue
        entry = _yaml_mapping_entry(_strip_yaml_inline_comment(line))
        if entry is not None and entry[0] == "on":
            on_entries.append((index, entry[1]))
    if not on_entries:
        return set(), ["workflow is missing a top-level on trigger"]
    if len(on_entries) != 1:
        return set(), ["workflow must contain exactly one top-level on trigger"]

    start, value = on_entries[0]
    if value:
        names = _trigger_names_from_value(value)
        if names is None:
            return set(), ["workflow on trigger uses an unsupported YAML value"]
        return names, []

    body: list[tuple[int, str]] = []
    for line in lines[start + 1 :]:
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        indent = len(line) - len(line.lstrip(" "))
        if indent == 0:
            break
        body.append((indent, _strip_yaml_inline_comment(line.lstrip(" "))))
    if not body:
        return set(), []

    event_indent = min(indent for indent, line in body if line)
    event_lines = [line for indent, line in body if indent == event_indent and line]
    sequence = all(line.startswith("- ") for line in event_lines)
    names: set[str] = set()
    for line in event_lines:
        if sequence:
            name = _yaml_scalar_name(line[2:])
        else:
            entry = _yaml_mapping_entry(line)
            name = None if entry is None else entry[0]
        if name is None:
            return set(), [
                "workflow on trigger block is not a supported YAML collection"
            ]
        names.add(name)
    return names, []


def _yaml_blocks(text: str, header: str, indent: int) -> list[str]:
    """Return active YAML blocks beginning with an exact, indentation-scoped header."""

    lines = _without_commented_lines(text).splitlines()
    target = " " * indent + header
    starts = [index for index, line in enumerate(lines) if line.rstrip() == target]
    blocks: list[str] = []
    for start in starts:
        end = len(lines)
        for index in range(start + 1, len(lines)):
            line = lines[index]
            if not line.strip():
                continue
            current_indent = len(line) - len(line.lstrip(" "))
            if current_indent <= indent:
                end = index
                break
        blocks.append("\n".join(lines[start:end]))
    return blocks


def _single_yaml_block(
    path: Path,
    text: str,
    header: str,
    indent: int,
    label: str,
    errors: list[str],
) -> str:
    blocks = _yaml_blocks(text, header, indent)
    if len(blocks) != 1:
        errors.append(f"{path}: expected exactly one active {label}")
        return ""
    return blocks[0]


def _yaml_mapping_entries_at_indent(
    text: str, indent: int
) -> list[tuple[str, str]]:
    """Return active mapping entries at one exact indentation level."""

    entries: list[tuple[str, str]] = []
    for line in _without_commented_lines(text).splitlines():
        if not line.strip() or len(line) - len(line.lstrip(" ")) != indent:
            continue
        entry = _yaml_mapping_entry(_strip_yaml_inline_comment(line.lstrip(" ")))
        if entry is not None:
            entries.append(entry)
    return entries


def _normalized_active_commands(text: str) -> list[str]:
    active = _without_commented_lines(text)
    normalized = re.sub(r"[ \t]*\\\r?\n[ \t]*", " ", active)
    return [line.strip() for line in normalized.splitlines() if line.strip()]


def _audit_oracle_build_retry(
    path: Path,
    step: str,
    output_path: str,
    log_path: str,
    errors: list[str],
) -> None:
    """Retry only typed transient downloads and fail closed on integrity errors."""

    required_once = {
        "build_oracle_image() {": (
            "locked image retries must call one reviewed build function"
        ),
        "retryable_oracle_download_failure() {": (
            "locked image retries must classify only reviewed download failures"
        ),
        (
            "https://mirrors.ibiblio.org/pub/mirrors/libreoffice/stable/26.2.3/"
            "deb/x86_64/LibreOffice_26.2.3_Linux_x86-64_deb.tar.gz"
        ): "locked image retries must bind failures to the exact primary artifact mirror",
        (
            "https://download.documentfoundation.org/libreoffice/stable/26.2.3/"
            "deb/x86_64/LibreOffice_26.2.3_Linux_x86-64_deb.tar.gz"
        ): "locked image retries must bind failures to the exact fallback artifact mirror",
        r"curl: \((5|6|7|16|18|28|35|52|55|56|92)\)": (
            "locked image retries must use the reviewed curl transport allowlist"
        ),
        (
            r"curl: \(22\) The requested URL returned error: "
            r"(408|429|500|502|503|504)([^0-9]|$)"
        ): "locked image retries must use the reviewed transient HTTP allowlist",
        "build_status=1": "locked image retries must initialize a failing status",
        "for build_attempt in 1 2 3; do": (
            "locked image retries must use exactly three bounded attempts"
        ),
        f"build_log={log_path}": (
            "locked image retries must use only the reviewed private diagnostic log"
        ),
        f'rm -f {output_path} "$build_log"': (
            "locked image retries must remove stale evidence before every attempt"
        ),
        'if build_oracle_image 2> "$build_log"; then': (
            "locked image retries must test the reviewed build function directly"
        ),
        "build_status=0": (
            "locked image retries must record only an actual successful build"
        ),
        "build_status=$?": (
            "locked image retries must preserve the failed builder status"
        ),
        'if ! retryable_oracle_download_failure "$build_log"; then': (
            "locked image retries must reject every unclassified failure immediately"
        ),
        'if [[ "$build_attempt" -lt 3 ]]; then': (
            "locked image retries must not sleep after the final attempt"
        ),
        "retry_delay_seconds=$((build_attempt * 5))": (
            "locked image retries must use the reviewed bounded backoff"
        ),
        'sleep "$retry_delay_seconds"': (
            "locked image retries must apply the reviewed bounded backoff"
        ),
        'if [[ "$build_status" -ne 0 ]]; then': (
            "locked image retries must fail closed after exhaustion"
        ),
        'rm -f "$build_log"': "successful retries must delete the private build log",
    }
    for snippet, message in required_once.items():
        if step.count(snippet) != 1:
            errors.append(f"{path}: {message}")
    if step.count('cat "$build_log" >&2') != 2:
        errors.append(
            f"{path}: locked image retries must preserve both success and failure logs"
        )
    if step.count('exit "$build_status"') != 2:
        errors.append(
            f"{path}: locked image retries must propagate immediate and exhausted failures"
        )
    if step.count("\n              break\n") != 1:
        errors.append(
            f"{path}: locked image retries must stop after the first successful build"
        )
    classify = step.find('if ! retryable_oracle_download_failure "$build_log"; then')
    immediate_exit = step.find('exit "$build_status"', classify)
    retry_delay = step.find('if [[ "$build_attempt" -lt 3 ]]; then', classify)
    if not 0 <= classify < immediate_exit < retry_delay:
        errors.append(
            f"{path}: unclassified locked image failures must exit before any retry"
        )


_SHELL_ASSIGNMENT_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_]*=.*", re.DOTALL)
_EXECUTABLE_WRAPPER_ARGUMENT_OPTIONS = {
    "env": {
        "-C",
        "--chdir",
        "-S",
        "--split-string",
        "-u",
        "--unset",
    },
    "sudo": {
        "-C",
        "--close-from",
        "-D",
        "--chdir",
        "-g",
        "--group",
        "-h",
        "--host",
        "-p",
        "--prompt",
        "-R",
        "--chroot",
        "-r",
        "--role",
        "-T",
        "--command-timeout",
        "-t",
        "--type",
        "-u",
        "--user",
    },
    "command": set(),
    "exec": {"-a"},
    "nice": {"-n", "--adjustment"},
    "time": {"-f", "--format", "-o", "--output"},
}
_DOCKER_GLOBAL_ARGUMENT_OPTIONS = {
    "-c",
    "--config",
    "--context",
    "-H",
    "--host",
    "-l",
    "--log-level",
    "--tlscacert",
    "--tlscert",
    "--tlskey",
}


def _shell_command_segments(command: str) -> list[list[str]] | None:
    """Tokenize executable shell segments while honoring quotes and comments.

    Redirections remain attached to their command so their operands cannot be
    mistaken for a second command.  ``None`` is deliberately distinct from an
    empty command: callers must fail closed when shell syntax cannot be parsed.
    """

    try:
        lexer = shlex.shlex(
            command,
            posix=True,
            punctuation_chars="();<>|&",
        )
        lexer.whitespace_split = True
        lexer.commenters = "#"
        tokens = list(lexer)
    except ValueError:
        return None

    segments: list[list[str]] = []
    segment: list[str] = []
    arithmetic_depth = 0
    for token in tokens:
        if token == "((":
            arithmetic_depth += 1
            continue
        if token == "))" and arithmetic_depth:
            arithmetic_depth -= 1
            continue
        if arithmetic_depth:
            continue
        if token and all(character in "();|&" for character in token):
            if segment:
                segments.append(segment)
                segment = []
            continue
        segment.append(token)
    if segment:
        segments.append(segment)
    return segments


_SHELL_VARIABLE_RE = re.compile(
    r"\$(?:[A-Za-z_][A-Za-z0-9_]*|"
    r"\{[A-Za-z_][A-Za-z0-9_]*(?:\[@\]|\[\*\])?\})\Z"
)
_UNSAFE_SHELL_EXECUTORS = {
    ".",
    "chroot",
    "eval",
    "nohup",
    "parallel",
    "setsid",
    "source",
    "stdbuf",
    "timeout",
    "watch",
    "xargs",
}
_SHELL_INTERPRETERS = {"bash", "dash", "ksh", "sh", "zsh"}
_PYTHON_INTERPRETER_RE = re.compile(r"(?:python|pypy)(?:\d+(?:\.\d+)*)?\Z")
_INLINE_EVAL_OPTIONS = {
    "lua": {"-e"},
    "node": {"-e", "--eval", "-p", "--print"},
    "perl": {"-e", "-E"},
    "php": {"-r"},
    "ruby": {"-e"},
}
_REDIRECTION_RE = re.compile(r"(?:[0-9]+)?(?:<<?|>>?|<>|>&|<&)\Z")


def _assignment_parts(token: str) -> tuple[str, str] | None:
    match = re.fullmatch(
        r"(?P<name>[A-Za-z_][A-Za-z0-9_]*)(?P<append>\+)?=(?P<value>.*)",
        token,
    )
    if match is None:
        return None
    if match.group("append") is not None:
        # The prior value is shell state.  Mark concatenation as dynamic so a
        # later executable expansion is rejected rather than mis-resolved.
        return match.group("name"), "$<concatenated>"
    return match.group("name"), match.group("value")


def _static_assignment_value(value: str) -> str | None:
    """Return a literal shell assignment value or ``None`` when it is dynamic."""

    if any(marker in value for marker in ("$", "`", "\n", "\r", "\0")):
        return None
    return value


def _variable_name(token: str) -> str | None:
    if _SHELL_VARIABLE_RE.fullmatch(token) is None:
        return None
    if not token.startswith("${"):
        return token[1:]
    name = token[2:-1]
    return re.sub(r"\[(?:@|\*)\]\Z", "", name)


def _executable_name(token: str) -> str:
    """Return a command basename without Path's special handling of ``.``."""

    return token.rsplit("/", 1)[-1]


def _python_inline_is_policy_safe(source: str) -> bool:
    """Allow only the read-only JSON snippets already used by oracle workflows."""

    try:
        tree = ast.parse(source, mode="exec")
    except (SyntaxError, ValueError):
        return False
    allowed_nodes = (
        ast.Attribute,
        ast.Call,
        ast.Constant,
        ast.Expr,
        ast.Import,
        ast.Load,
        ast.Module,
        ast.Name,
        ast.Subscript,
        ast.Tuple,
        ast.alias,
        ast.keyword,
    )
    if any(not isinstance(node, allowed_nodes) for node in ast.walk(tree)):
        return False
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            if len(node.names) != 1:
                return False
            imported = node.names[0]
            if imported.name != "json" or imported.asname is not None:
                return False
        elif isinstance(node, ast.Name):
            if node.id not in {"json", "open", "print"}:
                return False
        elif isinstance(node, ast.Call):
            function = node.func
            if isinstance(function, ast.Name):
                if function.id not in {"open", "print"}:
                    return False
                if function.id == "open" and (
                    not node.args
                    or not isinstance(node.args[0], ast.Constant)
                    or not isinstance(node.args[0].value, str)
                ):
                    return False
            elif not (
                isinstance(function, ast.Attribute)
                and isinstance(function.value, ast.Name)
                and function.value.id == "json"
                and function.attr in {"dumps", "load"}
            ):
                return False
    return True


def _inline_interpreter_is_unsafe(
    executable: str,
    arguments: list[str],
) -> bool:
    """Reject inline interpreter programs unless they match the narrow safe form."""

    if _PYTHON_INTERPRETER_RE.fullmatch(executable):
        for index, argument in enumerate(arguments):
            if argument == "-c":
                return index + 1 >= len(arguments) or not _python_inline_is_policy_safe(
                    arguments[index + 1]
                )
            if argument.startswith("-c") and len(argument) > 2:
                return not _python_inline_is_policy_safe(argument[2:])
            if "$" in argument and (index == 0 or argument.startswith("-")):
                return True
        return False
    options = _INLINE_EVAL_OPTIONS.get(executable)
    if options is None:
        return False
    for index, argument in enumerate(arguments):
        if argument in options:
            return True
        if any(
            argument.startswith(option) and len(argument) > len(option)
            for option in options
            if option.startswith("-") and not option.startswith("--")
        ):
            return True
        if index == 0 and "$" in argument:
            return True
    return False


def _expand_static_tokens(
    tokens: list[str],
    assignments: dict[str, str | None],
) -> tuple[list[str], bool]:
    """Expand exact scalar-variable tokens and report unresolved executables."""

    expanded: list[str] = []
    unresolved_executable = False
    for index, token in enumerate(tokens):
        name = _variable_name(token)
        if name is None:
            expanded.append(token)
            continue
        value = assignments.get(name)
        if value is None:
            expanded.append(token)
            if index == 0:
                unresolved_executable = True
            continue
        try:
            replacement = shlex.split(value, posix=True)
        except ValueError:
            replacement = []
        if not replacement:
            expanded.append(token)
            if index == 0:
                unresolved_executable = True
            continue
        expanded.extend(replacement)
    return expanded, unresolved_executable


def _skip_redirection(tokens: list[str], index: int) -> int | None:
    """Return the token after one leading redirection, if present."""

    if index >= len(tokens):
        return None
    token = tokens[index]
    if token.isdigit() and index + 1 < len(tokens):
        if _REDIRECTION_RE.fullmatch(tokens[index + 1]):
            return index + 3 if index + 2 < len(tokens) else None
    if _REDIRECTION_RE.fullmatch(token):
        return index + 2 if index + 1 < len(tokens) else None
    return None


def _command_index(tokens: list[str]) -> int:
    """Locate a segment's executable after assignments and redirections."""

    index = 0
    control_words = {
        "!",
        "do",
        "elif",
        "else",
        "if",
        "then",
        "until",
        "while",
    }
    while index < len(tokens):
        if tokens[index] in control_words:
            index += 1
            continue
        if _assignment_parts(tokens[index]) is not None:
            index += 1
            continue
        redirected = _skip_redirection(tokens, index)
        if redirected is not None:
            index = redirected
            continue
        break
    return index


def _unwrap_executable_wrapper(
    tokens: list[str], index: int
) -> tuple[list[str], int] | None:
    """Consume or expand one known wrapper and locate its executed command."""

    wrapper = _executable_name(tokens[index])
    if wrapper not in _EXECUTABLE_WRAPPER_ARGUMENT_OPTIONS:
        return None
    index += 1
    argument_options = _EXECUTABLE_WRAPPER_ARGUMENT_OPTIONS[wrapper]
    while index < len(tokens):
        token = tokens[index]
        if token == "--":
            return tokens, index + 1
        if (
            wrapper == "command"
            and token.startswith("-")
            and any(flag in token[1:] for flag in ("v", "V"))
        ):
            return [], 0
        option = token.split("=", 1)[0]
        if token.startswith("-"):
            if wrapper == "env" and option in {"-S", "--split-string"}:
                if "=" in token:
                    split_string = token.split("=", 1)[1]
                    remaining_index = index + 1
                elif option == "-S" and len(token) > 2:
                    split_string = token[2:]
                    remaining_index = index + 1
                elif index + 1 < len(tokens):
                    split_string = tokens[index + 1]
                    remaining_index = index + 2
                else:
                    return [], 0
                try:
                    split_tokens = shlex.split(split_string, posix=True)
                except ValueError:
                    return [], 0
                return (
                    tokens[: index - 1] + split_tokens + tokens[remaining_index:],
                    index - 1,
                )
            index += 1
            if option in argument_options and "=" not in token:
                index += 1
            continue
        if wrapper == "env" and _SHELL_ASSIGNMENT_RE.fullmatch(token):
            index += 1
            continue
        return tokens, index
    return tokens, index


def _segment_invokes_docker_build(
    tokens: list[str],
    assignments: dict[str, str | None] | None = None,
) -> bool:
    """Return whether a shell segment can bypass the reviewed build wrapper."""

    assignments = {} if assignments is None else assignments
    tokens, unresolved_executable = _expand_static_tokens(tokens, assignments)
    index = _command_index(tokens)
    if unresolved_executable or (
        index < len(tokens)
        and (_variable_name(tokens[index]) is not None or tokens[index].startswith("$"))
    ):
        # A computed executable is precisely the kind of construct this
        # lightweight policy cannot prove safe.  Oracle build workflows must
        # use literal, reviewable executables.
        return True
    while index < len(tokens):
        unwrapped = _unwrap_executable_wrapper(tokens, index)
        if unwrapped is None:
            break
        tokens, index = unwrapped
        tokens, unresolved_executable = _expand_static_tokens(tokens, assignments)
        if unresolved_executable or (
            index < len(tokens)
            and (
                _variable_name(tokens[index]) is not None
                or tokens[index].startswith("$")
            )
        ):
            return True
        while index < len(tokens) and _assignment_parts(tokens[index]) is not None:
            index += 1
    if index >= len(tokens):
        return False
    executable = _executable_name(tokens[index])
    if executable in _UNSAFE_SHELL_EXECUTORS:
        return True
    if executable in _SHELL_INTERPRETERS:
        # ``-c`` can hide arbitrary shell parsing, including additional
        # expansion and aliases.  Ban it rather than attempting a partial
        # second shell implementation.  Non-executing modes such as ``-n``
        # remain permitted.
        arguments = tokens[index + 1 :]
        return not arguments or arguments[0] != "-n"
    if _inline_interpreter_is_unsafe(executable, tokens[index + 1 :]):
        return True
    if executable == "find" and any(
        token in {"-exec", "-execdir", "-ok", "-okdir"} for token in tokens[index + 1 :]
    ):
        return True
    if executable != "docker":
        return False
    arguments = tokens[index + 1 :]
    arguments, _ = _expand_static_tokens(arguments, assignments)
    argument_index = 0
    while argument_index < len(arguments) and arguments[argument_index].startswith("-"):
        option = arguments[argument_index].split("=", 1)[0]
        argument_index += 1
        if (
            option in _DOCKER_GLOBAL_ARGUMENT_OPTIONS
            and "=" not in arguments[argument_index - 1]
        ):
            argument_index += 1
    arguments = arguments[argument_index:]
    if not arguments:
        return False
    if _variable_name(arguments[0]) is not None or "$" in arguments[0]:
        return True
    if (
        arguments[0] in {"builder", "buildx", "compose", "image"}
        and len(arguments) >= 2
        and (_variable_name(arguments[1]) is not None or "$" in arguments[1])
    ):
        return True
    return (
        arguments[0] == "build"
        or len(arguments) >= 2
        and arguments[:2]
        in (
            ["builder", "build"],
            ["buildx", "bake"],
            ["buildx", "build"],
            ["compose", "build"],
            ["image", "build"],
        )
    )


def _block_scalar_text(
    block: list[str],
    parent_index: int,
    parent_indent: int,
    style: str,
) -> str:
    """Resolve the command-relevant line folding of one YAML block scalar."""

    raw_lines: list[tuple[int, str]] = []
    for line in block[parent_index + 1 :]:
        if line.strip():
            indent = len(line) - len(line.lstrip(" "))
            if indent <= parent_indent:
                break
            raw_lines.append((indent, line))
        else:
            raw_lines.append((parent_indent + 1, ""))
    nonempty_indents = [indent for indent, line in raw_lines if line.strip()]
    if not nonempty_indents:
        return ""
    content_indent = min(nonempty_indents)
    content = [
        (
            indent,
            line[content_indent:] if line.strip() else "",
        )
        for indent, line in raw_lines
    ]
    if style == "|":
        return "\n".join(line for _, line in content)

    folded = ""
    previous_indent: int | None = None
    previous_line: str | None = None
    for indent, line in content:
        if previous_line is not None:
            if (
                previous_line
                and line
                and previous_indent == content_indent
                and indent == content_indent
            ):
                folded += " "
            else:
                folded += "\n"
        folded += line
        previous_indent = indent
        previous_line = line
    return folded


def _workflow_run_scripts(text: str) -> list[str]:
    """Resolve active inline and block-style run scalars from workflow steps."""

    scripts: list[str] = []
    for line in _without_commented_lines(text).splitlines():
        if not line.strip():
            continue
        entry = _yaml_mapping_entry(_strip_yaml_inline_comment(line.lstrip(" ")))
        if entry is not None and entry[0] == "steps" and entry[1] != "":
            scripts.append("\0")
    for blocks in _workflow_step_sequences(text):
        for step_indent, block in blocks:
            entries = _step_mapping_entries(step_indent, block)
            mapping_indent = step_indent + 2
            has_unreadable_step = False
            for index, line in enumerate(block):
                indent = len(line) - len(line.lstrip(" "))
                content = line.lstrip(" ")
                effective_indent = indent
                if index == 0 and indent == step_indent and content.startswith("-"):
                    content = content[1:].lstrip(" ")
                    effective_indent = mapping_indent
                if effective_indent != mapping_indent or not content:
                    continue
                if _yaml_mapping_entry(_strip_yaml_inline_comment(content)) is None:
                    has_unreadable_step = True
            names = [name for name, _, _, _ in entries]
            if len(names) != len(set(names)):
                has_unreadable_step = True
            if has_unreadable_step:
                scripts.append("\0")
            for name, value, line_index, parent_indent in entries:
                if name != "run":
                    continue
                scalar = _strip_yaml_inline_comment(value).strip()
                block_match = re.fullmatch(r"(?P<style>[|>])[-+0-9]*", scalar)
                if block_match is None:
                    scripts.append(_yaml_unquote_scalar(scalar))
                else:
                    scripts.append(
                        _block_scalar_text(
                            block,
                            line_index,
                            parent_indent,
                            block_match.group("style"),
                        )
                    )
    return scripts


_HEREDOC_RE = re.compile(
    r"<<(?P<tabs>-)?\s*(?P<quote>['\"]?)(?P<delimiter>[A-Za-z_][A-Za-z0-9_]*)"
    r"(?P=quote)"
)


def _without_heredoc_bodies(script: str) -> str:
    """Remove shell heredoc data so inert text is not audited as execution."""

    active_lines: list[str] = []
    pending: list[tuple[str, bool]] = []
    for line in script.splitlines():
        if pending:
            delimiter, strip_tabs = pending[0]
            candidate = line.lstrip("\t") if strip_tabs else line
            if candidate == delimiter:
                pending.pop(0)
            continue
        active_lines.append(line)
        pending.extend(
            (match.group("delimiter"), match.group("tabs") is not None)
            for match in _HEREDOC_RE.finditer(line)
        )
    return "\n".join(active_lines)


def _direct_docker_build_commands(text: str) -> list[str]:
    """Find active or unprovable commands bypassing the reviewed wrapper."""

    commands: list[str] = []
    for script in _workflow_run_scripts(text):
        assignments: dict[str, str | None] = {}
        if "\0" in script:
            commands.append("<unreadable run scalar>")
            continue
        active = _without_heredoc_bodies(script)
        normalized = re.sub(r"[ \t]*\\\r?\n[ \t]*", " ", active)
        for line in normalized.splitlines():
            command = line.strip()
            if not command:
                continue
            if "`" in command:
                commands.append(command)
                continue
            substitution_assignment = re.fullmatch(
                r"(?P<name>[A-Za-z_][A-Za-z0-9_]*)="
                r"(?P<quote>[\"']?)\$\((?P<tail>.*)",
                command,
            )
            if substitution_assignment is not None:
                assignments[substitution_assignment.group("name")] = None
                command = substitution_assignment.group("tail").strip()
                suffix = ")" + substitution_assignment.group("quote")
                if command.endswith(suffix):
                    command = command[: -len(suffix)].rstrip()
                if not command:
                    continue
            if re.fullmatch(r"\)+[\"']?", command):
                continue
            array_assignment = re.fullmatch(
                r"(?P<name>[A-Za-z_][A-Za-z0-9_]*)(?P<append>\+)?="
                r"\((?P<value>.*)\)",
                command,
            )
            if array_assignment is not None:
                value = array_assignment.group("value")
                if "$(" in value or "`" in value:
                    commands.append(command)
                    continue
                if array_assignment.group("append") is None:
                    try:
                        values = shlex.split(value, posix=True)
                    except ValueError:
                        commands.append(command)
                        continue
                    assignments[array_assignment.group("name")] = (
                        " ".join(values)
                        if all(
                            _static_assignment_value(item) is not None
                            for item in values
                        )
                        else None
                    )
                continue
            segments = _shell_command_segments(command)
            if segments is None:
                commands.append(command)
                continue
            violation = False
            for segment in segments:
                local_assignments = dict(assignments)
                for token in segment:
                    parts = _assignment_parts(token)
                    if parts is None:
                        break
                    name, value = parts
                    local_assignments[name] = _static_assignment_value(value)
                if _segment_invokes_docker_build(segment, local_assignments):
                    violation = True
                if _command_index(segment) >= len(segment):
                    assignments.update(local_assignments)
                elif all(_assignment_parts(token) is not None for token in segment):
                    assignments.update(local_assignments)
            if violation:
                commands.append(command)
    return commands


def _audit_oracle_buildx_setup(path: Path, text: str, errors: list[str]) -> None:
    setup = _single_yaml_block(
        path,
        text,
        "- name: Set up the pinned Buildx client",
        6,
        "pinned oracle Buildx setup step",
        errors,
    )
    required = (
        f"uses: {ORACLE_BUILDX_ACTION} # v4.2.0",
        "name: rxls-oracle-client",
        f"version: {ORACLE_BUILDX_VERSION}",
        "driver: docker-container",
        "platforms: linux/amd64",
        f"image={ORACLE_BUILDKIT_IMAGE}",
        "provenance-add-gha=false",
        "buildkitd-flags: --oci-worker-snapshotter=native",
    )
    if any(setup.count(snippet) != 1 for snippet in required):
        errors.append(
            f"{path}: oracle builds must pin Buildx, BuildKit, linux/amd64, "
            "native snapshotting, and disabled GitHub provenance"
        )
    if _direct_docker_build_commands(text):
        errors.append(
            f"{path}: oracle workflows must build only through the reviewed wrapper"
        )


def _audit_exact_wasm_bindgen_install(
    path: Path,
    workflow_text: str,
    install_step: str,
    build_step: str,
    build_command: str,
    label: str,
) -> list[str]:
    """Require an exact wasm-bindgen CLI rebuilt into an isolated temporary root."""

    errors: list[str] = []
    expected_root = (
        'tool_root="$RUNNER_TEMP/rxls-wasm-bindgen-cli-$WASM_BINDGEN_VERSION"'
    )
    expected_remove = 'rm -rf "$tool_root"'
    expected_mkdir = 'mkdir -p "$tool_root"'
    expected_rustup = (
        'rustup toolchain install "$WASM_BINDGEN_BUILD_RUST" --profile minimal'
    )
    expected_cargo = (
        'cargo "+$WASM_BINDGEN_BUILD_RUST" install wasm-bindgen-cli '
        '--version "$WASM_BINDGEN_VERSION" --locked --root "$tool_root"'
    )
    expected_version = (
        'test "$("$tool_root/bin/wasm-bindgen" --version)" = '
        '"wasm-bindgen $WASM_BINDGEN_VERSION"'
    )
    expected_github_path = 'echo "$tool_root/bin" >> "$GITHUB_PATH"'
    expected_path_export = (
        'export PATH="$RUNNER_TEMP/rxls-wasm-bindgen-cli-'
        '$WASM_BINDGEN_VERSION/bin:$PATH"'
    )
    expected_resolution = (
        'test "$(command -v wasm-bindgen)" = '
        '"$RUNNER_TEMP/rxls-wasm-bindgen-cli-'
        '$WASM_BINDGEN_VERSION/bin/wasm-bindgen"'
    )
    step_commands = _normalized_active_commands(install_step)
    build_commands = _normalized_active_commands(build_step)
    workflow_commands = _normalized_active_commands(workflow_text)
    step_installs = [
        command for command in step_commands if "install wasm-bindgen-cli" in command
    ]
    workflow_installs = [
        command
        for command in workflow_commands
        if "install wasm-bindgen-cli" in command
    ]
    step_roots = [
        command for command in step_commands if command.startswith("tool_root=")
    ]
    step_github_paths = [
        command for command in step_commands if "$GITHUB_PATH" in command
    ]
    build_path_exports = [
        command for command in build_commands if command.startswith("export PATH=")
    ]
    required_install_commands = (
        "set -euo pipefail",
        'test -n "$RUNNER_TEMP"',
        expected_root,
        expected_remove,
        expected_mkdir,
        expected_rustup,
        expected_cargo,
        expected_version,
        expected_github_path,
    )
    if step_commands.count("shell: bash") != 1:
        errors.append(f"{path}: {label} must run under an explicit Bash shell")
    if any(step_commands.count(command) != 1 for command in required_install_commands):
        errors.append(
            f"{path}: {label} must create one fresh RUNNER_TEMP tool root and "
            "verify the exact build-only Rust/wasm-bindgen tool"
        )
    if step_installs != [expected_cargo]:
        errors.append(
            f"{path}: {label} must install wasm-bindgen-cli only into its "
            "fresh dedicated root"
        )
    if workflow_installs != [expected_cargo]:
        errors.append(
            f"{path}: workflow must contain exactly one active, isolated, pinned "
            "wasm-bindgen-cli install"
        )
    if step_roots != [expected_root] or step_github_paths != [expected_github_path]:
        errors.append(
            f"{path}: {label} must expose only the fresh RUNNER_TEMP tool bin "
            "through GITHUB_PATH"
        )
    install_positions = [
        step_commands.index(command)
        for command in required_install_commands
        if step_commands.count(command) == 1
    ]
    if len(install_positions) != len(
        required_install_commands
    ) or install_positions != sorted(install_positions):
        errors.append(
            f"{path}: {label} must clean the temporary root before installing and "
            "export it only after exact-version verification"
        )
    scoped_commands = (
        "set -euo pipefail",
        'test -n "$RUNNER_TEMP"',
        expected_path_export,
        expected_resolution,
        build_command,
    )
    if build_commands.count("shell: bash") != 1:
        errors.append(f"{path}: {label} build must run under an explicit Bash shell")
    if build_path_exports != [expected_path_export]:
        errors.append(
            f"{path}: {label} build must prepend only the isolated tool bin to PATH"
        )
    if any(build_commands.count(command) != 1 for command in scoped_commands):
        errors.append(
            f"{path}: {label} must prepend only the isolated tool bin to PATH, "
            "verify command resolution, and build exactly once"
        )
    build_positions = [
        build_commands.index(command)
        for command in scoped_commands
        if build_commands.count(command) == 1
    ]
    if len(build_positions) != len(scoped_commands) or build_positions != sorted(
        build_positions
    ):
        errors.append(
            f"{path}: {label} must export and verify the isolated PATH before building"
        )
    if any("--force" in command for command in workflow_installs):
        errors.append(f"{path}: forced wasm-bindgen-cli installation is forbidden")
    return errors


def audit_action_pins(path: Path, text: str) -> list[str]:
    """Return policy violations for remote action references in one workflow."""

    errors: list[str] = []
    for line_number, line in enumerate(text.splitlines(), start=1):
        if "uses:" not in line:
            continue
        match = ACTION_RE.match(line)
        if match is None:
            errors.append(f"{path}:{line_number}: unreadable uses entry")
            continue
        spec = match.group("spec")
        if spec.startswith("./"):
            continue
        if not REMOTE_ACTION_RE.fullmatch(spec):
            errors.append(f"{path}:{line_number}: invalid remote action {spec!r}")
            continue
        action, ref = spec.rsplit("@", 1)
        if FULL_SHA_RE.fullmatch(ref) is None:
            errors.append(
                f"{path}:{line_number}: {action} must use a full immutable commit SHA"
            )
        comment = match.group("comment")
        if comment is None or not comment.strip():
            errors.append(
                f"{path}:{line_number}: pinned action {action} needs a version comment"
            )
        reviewed = REVIEWED_ACTION_ALLOWLIST.get(action)
        if reviewed is not None:
            expected_ref, expected_comment = reviewed
            if ref != expected_ref:
                errors.append(
                    f"{path}:{line_number}: {action} must use the reviewed immutable "
                    "commit SHA"
                )
            if comment is None or comment.strip() != expected_comment:
                errors.append(
                    f"{path}:{line_number}: {action} must use the reviewed version "
                    "comment"
                )
    return errors


def _workflow_step_sequences(text: str) -> list[list[tuple[int, list[str]]]]:
    """Return each active steps sequence as indentation-aware YAML blocks."""

    lines = _without_commented_lines(text).splitlines()
    sequences: list[list[tuple[int, list[str]]]] = []
    for header_index, line in enumerate(lines):
        indent = len(line) - len(line.lstrip(" "))
        entry = _yaml_mapping_entry(_strip_yaml_inline_comment(line.lstrip(" ")))
        if entry is None or entry != ("steps", ""):
            continue
        body_end = len(lines)
        for index in range(header_index + 1, len(lines)):
            candidate = lines[index]
            if not candidate.strip():
                continue
            candidate_indent = len(candidate) - len(candidate.lstrip(" "))
            if candidate_indent <= indent:
                body_end = index
                break
        starts = [
            index
            for index in range(header_index + 1, body_end)
            if re.match(r"^\s*-(?:\s|$)", lines[index])
        ]
        if not starts:
            sequences.append([])
            continue
        step_indent = min(
            len(lines[index]) - len(lines[index].lstrip(" ")) for index in starts
        )
        starts = [
            index
            for index in starts
            if len(lines[index]) - len(lines[index].lstrip(" ")) == step_indent
        ]
        blocks: list[tuple[int, list[str]]] = []
        for position, start in enumerate(starts):
            end = starts[position + 1] if position + 1 < len(starts) else body_end
            blocks.append((step_indent, lines[start:end]))
        sequences.append(blocks)
    return sequences


def _workflow_job_step_sequences(
    text: str,
) -> tuple[list[tuple[str, list[tuple[int, list[str]]]]], list[str]]:
    """Return the single inline step sequence for every workflow job.

    This intentionally accepts only the block mapping form used by reviewed
    workflows. Flow mappings, duplicate ``jobs`` keys, and jobs whose execution
    cannot be scoped are rejected instead of being silently omitted from the
    PR-head policy. A job may contain either one inline ``steps`` sequence or
    one repository-local reusable-workflow ``uses`` target.
    """

    lines = _without_commented_lines(text).splitlines()
    jobs_entries: list[int] = []
    for index, line in enumerate(lines):
        if not line.strip() or len(line) != len(line.lstrip(" ")):
            continue
        entry = _yaml_mapping_entry(_strip_yaml_inline_comment(line))
        if entry == ("jobs", ""):
            jobs_entries.append(index)
        elif entry is not None and entry[0] == "jobs":
            return [], ["jobs must use a supported block mapping"]
    if len(jobs_entries) != 1:
        return [], ["workflow must contain exactly one block-mapped jobs section"]

    start = jobs_entries[0]
    end = len(lines)
    for index in range(start + 1, len(lines)):
        line = lines[index]
        if line.strip() and len(line) == len(line.lstrip(" ")):
            end = index
            break
    body = [
        (index, line)
        for index, line in enumerate(lines[start + 1 : end], start=start + 1)
        if line.strip()
    ]
    if not body:
        return [], ["jobs section is empty"]
    job_indent = min(len(line) - len(line.lstrip(" ")) for _, line in body)
    starts: list[tuple[int, str]] = []
    for index, line in body:
        indent = len(line) - len(line.lstrip(" "))
        if indent != job_indent:
            continue
        entry = _yaml_mapping_entry(_strip_yaml_inline_comment(line.lstrip(" ")))
        if entry is None or entry[1] != "":
            return [], ["every job must use a supported block mapping"]
        starts.append((index, entry[0]))
    if not starts:
        return [], ["jobs section has no supported jobs"]
    if len({name for _, name in starts}) != len(starts):
        return [], ["job names must be unique"]

    jobs: list[tuple[str, list[tuple[int, list[str]]]]] = []
    errors: list[str] = []
    for position, (job_start, name) in enumerate(starts):
        job_end = starts[position + 1][0] if position + 1 < len(starts) else end
        job_lines = lines[job_start:job_end]
        job_text = "\n".join(job_lines)
        sequences = _workflow_step_sequences(job_text)
        if len(sequences) == 0:
            job_mapping_indent = job_indent + 2
            uses = []
            for line in job_lines[1:]:
                if not line.strip():
                    continue
                indent = len(line) - len(line.lstrip(" "))
                if indent != job_mapping_indent:
                    continue
                entry = _yaml_mapping_entry(
                    _strip_yaml_inline_comment(line.lstrip(" "))
                )
                if entry is not None and entry[0] == "uses":
                    uses.append(_yaml_unquote_scalar(entry[1]))
            if (
                len(uses) == 1
                and uses[0].startswith("./.github/workflows/")
                and not uses[0].endswith(("/", "\\"))
            ):
                jobs.append((name, []))
                continue
        if len(sequences) != 1:
            errors.append(
                f"job {name!r} must contain exactly one inline steps sequence"
            )
            continue
        jobs.append((name, sequences[0]))
    return jobs, errors


def _yaml_unquote_scalar(text: str) -> str:
    value = _strip_yaml_inline_comment(text).strip()
    if value.startswith("'") and value.endswith("'") and len(value) >= 2:
        return value[1:-1].replace("''", "'")
    if value.startswith('"') and value.endswith('"') and len(value) >= 2:
        return value[1:-1]
    return value


def _step_mapping_entries(
    step_indent: int, block: list[str]
) -> list[tuple[str, str, int, int]]:
    """Return top-level mapping entries from one YAML step."""

    entries: list[tuple[str, str, int, int]] = []
    mapping_indent = step_indent + 2
    for index, line in enumerate(block):
        indent = len(line) - len(line.lstrip(" "))
        content = line.lstrip(" ")
        effective_indent = indent
        if index == 0 and indent == step_indent and content.startswith("-"):
            content = content[1:].lstrip(" ")
            effective_indent = mapping_indent
        if effective_indent != mapping_indent or not content:
            continue
        entry = _yaml_mapping_entry(_strip_yaml_inline_comment(content))
        if entry is not None:
            entries.append((*entry, index, effective_indent))
    return entries


def _nested_mapping_values(
    block: list[str],
    parent_index: int,
    parent_indent: int,
    key: str,
) -> list[str]:
    """Return exact child values from one block-style YAML mapping."""

    child_lines: list[tuple[int, str]] = []
    for line in block[parent_index + 1 :]:
        if not line.strip():
            continue
        indent = len(line) - len(line.lstrip(" "))
        if indent <= parent_indent:
            break
        child_lines.append((indent, line.lstrip(" ")))
    if not child_lines:
        return []
    child_indent = min(indent for indent, _ in child_lines)
    values: list[str] = []
    for indent, content in child_lines:
        if indent != child_indent:
            continue
        entry = _yaml_mapping_entry(_strip_yaml_inline_comment(content))
        if entry is not None and entry[0] == key:
            values.append(_yaml_unquote_scalar(entry[1]))
    return values


def _step_values(entries: list[tuple[str, str, int, int]], key: str) -> list[str]:
    return [_yaml_unquote_scalar(value) for name, value, _, _ in entries if name == key]


def _audit_exact_job_step_sequence(
    path: Path,
    text: str,
    job_name: str,
    expected_uses: tuple[str, ...],
    expected_step_sha256: tuple[str, ...],
    errors: list[str],
) -> None:
    """Authenticate every complete step and action in one oracle-build job."""

    jobs, job_errors = _workflow_job_step_sequences(text)
    if job_errors:
        errors.extend(f"{path}: {error}" for error in job_errors)
        return
    matches = [blocks for name, blocks in jobs if name == job_name]
    if len(matches) != 1:
        errors.append(f"{path}: expected exactly one supported {job_name!r} job")
        return

    job_blocks = _yaml_blocks(text, f"{job_name}:", 2)
    if len(job_blocks) != 1:
        errors.append(f"{path}: expected exactly one block-mapped {job_name!r} job")
        return
    for line in job_blocks[0].splitlines()[1:]:
        if not line.strip():
            continue
        indent = len(line) - len(line.lstrip(" "))
        if indent != 4:
            continue
        entry = _yaml_mapping_entry(_strip_yaml_inline_comment(line.lstrip(" ")))
        if entry is not None and entry[0] == "uses":
            errors.append(
                f"{path}: oracle-build job {job_name!r} cannot call a reusable workflow"
            )

    actual: list[str] = []
    actual_step_sha256: list[str] = []
    unsupported = False
    for step_indent, block in matches[0]:
        canonical_block = "\n".join(_strip_yaml_inline_comment(line) for line in block)
        actual_step_sha256.append(
            hashlib.sha256(canonical_block.encode("utf-8")).hexdigest()
        )
        entries = _step_mapping_entries(step_indent, block)
        mapping_indent = step_indent + 2
        for index, line in enumerate(block):
            if not line.strip():
                continue
            indent = len(line) - len(line.lstrip(" "))
            content = line.lstrip(" ")
            effective_indent = indent
            if index == 0 and indent == step_indent and content.startswith("-"):
                content = content[1:].lstrip(" ")
                effective_indent = mapping_indent
            if effective_indent != mapping_indent:
                continue
            if (
                not content
                or _yaml_mapping_entry(_strip_yaml_inline_comment(content)) is None
            ):
                unsupported = True
        names = [name for name, _, _, _ in entries]
        if len(names) != len(set(names)):
            unsupported = True
        uses = _step_values(entries, "uses")
        if len(uses) > 1:
            unsupported = True
        elif uses:
            actual.append(uses[0])
    if unsupported:
        errors.append(
            f"{path}: oracle-build job {job_name!r} contains an unsupported step mapping"
        )
    if tuple(actual) != expected_uses:
        errors.append(
            f"{path}: oracle-build job {job_name!r} must use only the exact "
            "ordered action allowlist"
        )
    if tuple(actual_step_sha256) != expected_step_sha256:
        errors.append(
            f"{path}: oracle-build job {job_name!r} must preserve the exact "
            "reviewed step count, order, and complete step contents"
        )


def _audit_exact_workflow_sha256(
    path: Path,
    text: str,
    expected_sha256: str,
    errors: list[str],
) -> None:
    """Authenticate every byte of a reviewed oracle workflow."""

    actual_sha256 = hashlib.sha256(text.encode("utf-8")).hexdigest()
    if actual_sha256 != expected_sha256:
        errors.append(
            f"{path}: complete active workflow and execution context must match "
            "the reviewed SHA-256"
        )


def _audit_snapshot_apt_block(
    path: Path,
    block: str,
    label: str,
    scopes: tuple[str, ...],
    errors: list[str],
) -> None:
    """Require one isolated, immutable Ubuntu snapshot acquisition."""

    required_once = {
        'APT_ROOT="$PWD/target/render-oracle-apt"': (
            "must use the reviewed job-local APT root"
        ),
        'mkdir -p "$APT_ROOT/lists/partial" "$APT_ROOT/cache/archives/partial"': (
            "must create only isolated package-index and archive caches"
        ),
        "python3 scripts/render-oracle-host-tools.py apt-sources \\": (
            "must generate sources from the validated host-tools lock"
        ),
        '> "$APT_ROOT/ubuntu.sources"': (
            "must store the generated snapshot source inside the job-local root"
        ),
        '-o "Dir::Etc::sourcelist=$APT_ROOT/ubuntu.sources"': (
            "must use only the generated snapshot source"
        ),
        '-o "Dir::Etc::sourceparts=-"': (
            "must disable every runner-provided source part"
        ),
        '-o "Dir::State::lists=$APT_ROOT/lists"': (
            "must isolate package indices from the runner image"
        ),
        '-o "Dir::Cache::archives=$APT_ROOT/cache/archives"': (
            "must isolate downloaded package archives"
        ),
        '-o "Acquire::Retries=3"': (
            "must use only bounded snapshot acquisition retries"
        ),
    }
    for snippet, message in required_once.items():
        if block.count(snippet) != 1:
            errors.append(f"{path}: {label} {message}")
    for scope in scopes:
        command = (
            f"python3 scripts/render-oracle-host-tools.py apt-specs --scope {scope}"
        )
        if block.count(command) != 1:
            errors.append(
                f"{path}: {label} must request the exact {scope!r} package closure"
            )

    commands = _normalized_active_commands(block)
    apt_commands = [
        command for command in commands if command.startswith("sudo apt-get ")
    ]
    if apt_commands != [
        'sudo apt-get "${APT_OPTIONS[@]}" update',
        (
            'sudo apt-get "${APT_OPTIONS[@]}" install --yes '
            "--no-install-recommends --allow-downgrades "
            '"${SYSTEM_PACKAGES[@]}"'
        ),
    ]:
        errors.append(
            f"{path}: {label} must update and install only through the isolated "
            "snapshot options"
        )
    forbidden = (
        "archive.ubuntu.com",
        "azure.archive.ubuntu.com",
        "security.ubuntu.com",
        "apt-mirrors.txt",
        "apt-get upgrade",
        "apt-get dist-upgrade",
    )
    if any(value in block for value in forbidden):
        errors.append(f"{path}: {label} cannot fall back to a live package source")


def _checkout_step_is_exact(
    step_indent: int,
    block: list[str],
    expected_expression: str,
) -> bool:
    entries = _step_mapping_entries(step_indent, block)
    if [name for name, _, _, _ in entries] != ["uses", "with"]:
        return False
    with_entries = [entry for entry in entries if entry[0] == "with"]
    if len(with_entries) != 1 or _yaml_unquote_scalar(with_entries[0][1]) != "":
        return False
    _, _, parent_index, parent_indent = with_entries[0]
    return _nested_mapping_values(
        block,
        parent_index,
        parent_indent,
        "ref",
    ) == [expected_expression]


def _verifier_step_is_exact(
    step_indent: int,
    block: list[str],
    expected_expression: str,
) -> bool:
    entries = _step_mapping_entries(step_indent, block)
    if [name for name, _, _, _ in entries] != ["name", "shell", "env", "run"]:
        return False
    if _step_values(entries, "name") != ["Verify exact source revision"]:
        return False
    if _step_values(entries, "shell") != ["bash"]:
        return False
    run_entries = [entry for entry in entries if entry[0] == "run"]
    if len(run_entries) != 1:
        return False
    _, raw_run, line_index, parent_indent = run_entries[0]
    scalar = _strip_yaml_inline_comment(raw_run).strip()
    block_match = re.fullmatch(r"(?P<style>[|>])[-+0-9]*", scalar)
    if block_match is None:
        run_script = _yaml_unquote_scalar(scalar)
    else:
        run_script = _block_scalar_text(
            block,
            line_index,
            parent_indent,
            block_match.group("style"),
        )
    if run_script not in {
        'test "$(git rev-parse HEAD)" = "$EXPECTED_SHA"',
        ORACLE_HARDENED_SOURCE_VERIFIER,
    }:
        return False
    env_entries = [entry for entry in entries if entry[0] == "env"]
    if len(env_entries) != 1 or _yaml_unquote_scalar(env_entries[0][1]) != "":
        return False
    _, _, parent_index, parent_indent = env_entries[0]
    return _nested_mapping_values(
        block,
        parent_index,
        parent_indent,
        "EXPECTED_SHA",
    ) == [expected_expression]


def audit_pr_head_checkouts(path: Path, text: str) -> list[str]:
    """Require every PR job to test the actual head commit, never a merge ref."""

    active = _without_commented_lines(text)
    triggers, trigger_errors = _workflow_trigger_names(active)
    errors = [f"{path}: {error}" for error in trigger_errors]
    if "pull_request_target" in triggers:
        errors.append(
            f"{path}: pull_request_target is forbidden by the exact PR-head policy"
        )
    if "pull_request" not in triggers:
        return errors
    expected_expression = (
        ORACLE_SOURCE_SHA_EXPRESSION
        if path.name == "render-oracle.yml"
        else PR_HEAD_EXPRESSION
    )
    jobs, job_errors = _workflow_job_step_sequences(active)
    errors.extend(f"{path}: {error}" for error in job_errors)
    for job_name, blocks in jobs:
        if not blocks:
            continue
        checkout_count = 0
        for index, (step_indent, block) in enumerate(blocks):
            entries = _step_mapping_entries(step_indent, block)
            uses = _step_values(entries, "uses")
            if not any(value.startswith("actions/checkout@") for value in uses):
                continue
            checkout_count += 1
            if not _checkout_step_is_exact(
                step_indent,
                block,
                expected_expression,
            ):
                errors.append(
                    f"{path}: job {job_name!r} checkout must use the exact "
                    "pull-request head SHA"
                )
            if index + 1 >= len(blocks):
                errors.append(
                    f"{path}: job {job_name!r} checkout needs an immediate "
                    "exact-SHA verifier"
                )
                continue
            verifier_indent, verifier = blocks[index + 1]
            if not _verifier_step_is_exact(
                verifier_indent,
                verifier,
                expected_expression,
            ):
                errors.append(
                    f"{path}: job {job_name!r} checkout needs an immediate "
                    "exact-SHA verifier"
                )
            if "$GITHUB_SHA" in "\n".join(verifier):
                errors.append(
                    f"{path}: PR source verification must not use the synthetic GITHUB_SHA"
                )
        if checkout_count == 0:
            errors.append(
                f"{path}: pull-request job {job_name!r} has no guarded checkout step"
            )
    return errors


def _cargo_fuzz_commands(text: str) -> list[tuple[int, str]]:
    lines = text.splitlines()
    commands: list[tuple[int, str]] = []
    index = 0
    while index < len(lines):
        line = lines[index]
        if re.search(r"\bcargo\s+install\s+cargo-fuzz\b", line):
            start = index + 1
            command = line.strip()
            while command.rstrip().endswith("\\") and index + 1 < len(lines):
                index += 1
                command = command.rstrip()[:-1] + " " + lines[index].strip()
            commands.append((start, command))
        index += 1
    return commands


def _audit_exact_assignments(
    path: Path, text: str, names: tuple[str, ...]
) -> list[str]:
    errors: list[str] = []
    for name in names:
        expected = RELEASE_VERSIONS[name]
        assignment = re.compile(
            rf"^\s*{re.escape(name)}:\s*[\"']?{re.escape(expected)}[\"']?\s*$",
            re.MULTILINE,
        )
        if assignment.search(text) is None:
            errors.append(f"{path}: expected exact {name}={expected}")
    return errors


def audit_fuzz_tools(
    path: Path, text: str, required_assignments: tuple[str, ...]
) -> list[str]:
    """Return violations for a workflow that installs and invokes cargo-fuzz."""

    errors = _audit_exact_assignments(path, text, required_assignments)

    commands = _cargo_fuzz_commands(text)
    if not commands:
        errors.append(f"{path}: fuzzing workflow must install cargo-fuzz")
    errors.extend(audit_tool_commands(path, text))

    return errors


def audit_tool_commands(path: Path, text: str) -> list[str]:
    """Reject mutable nightly/cargo-fuzz commands in any hosted workflow."""

    errors: list[str] = []
    for line_number, command in _cargo_fuzz_commands(text):
        if not re.search(
            r"--version(?:=|\s+)(?:[\"']?\$\{?CARGO_FUZZ_VERSION\}?|"
            + re.escape(RELEASE_VERSIONS["CARGO_FUZZ_VERSION"])
            + r")[\"']?(?:\s|$)",
            command,
        ):
            errors.append(
                f"{path}:{line_number}: cargo-fuzz install must use exact version "
                f"{RELEASE_VERSIONS['CARGO_FUZZ_VERSION']}"
            )

    if re.search(r"rustup\s+toolchain\s+install\s+nightly(?:\s|$)", text):
        errors.append(f"{path}: workflow must not install mutable nightly")
    if re.search(r"cargo\s+\+nightly(?:\s|$)", text):
        errors.append(f"{path}: workflow must not invoke mutable nightly")
    return errors


def audit_release_versions(path: Path, text: str) -> list[str]:
    """Return violations for release toolchain and cargo-fuzz version pins."""

    return audit_fuzz_tools(path, text, tuple(RELEASE_VERSIONS))


def audit_core_release_evidence(path: Path, text: str) -> list[str]:
    """Require dry-run and immutable provenance evidence in the public bundle."""

    active = _without_commented_lines(text)
    errors: list[str] = []
    required = {
        "scripts/check_cargo_publish_dry_run.py": (
            "must use the dependency-free crates.io dry-run runner and verifier"
        ),
        "target/package/release-cargo-publish-dry-run.json": (
            "must retain the crates.io dry-run receipt"
        ),
        "dist/release-cargo-publish-dry-run.json": (
            "must bind the crates.io dry-run receipt into the release manifest"
        ),
        "target/reproducibility/rxls-release-candidate-manifest.json": (
            "must preserve the exact candidate manifest with reproducibility evidence"
        ),
        "target/reproducibility/rxls-release-reproducibility.json": (
            "must retain the two-candidate comparison"
        ),
        "target/reproducibility/rxls-release-candidate-attestation.json": (
            "must retain the immutable two-candidate attestation"
        ),
        "target/publication-attestation/rxls-tag-release-comparison.json": (
            "must retain the exact tag-to-candidate comparison"
        ),
        "- name: Bind publication provenance into release manifest": (
            "must assemble public provenance before registry publication"
        ),
        "cp target/publication-attestation/rxls-release-candidate-manifest.json dist/": (
            "public bundle must contain the attested candidate manifest"
        ),
        "cp target/publication-attestation/rxls-release-reproducibility.json dist/": (
            "public bundle must contain the two-candidate comparison"
        ),
        "cp target/publication-attestation/rxls-release-candidate-attestation.json dist/": (
            "public bundle must contain the candidate attestation"
        ),
        "cp target/publication-attestation/rxls-tag-release-comparison.json dist/": (
            "public bundle must contain the tag comparison"
        ),
        "target/release-public-hygiene-publication.json": (
            "must rerun public hygiene after adding provenance"
        ),
        "! -name rxls-release-manifest.json": (
            "final manifest assembly must exclude only its own output"
        ),
        "! -name public-hygiene.json": (
            "final manifest assembly must bind hygiene through its evidence record"
        ),
        "[[ ${#artifacts[@]} -eq 50 ]]": (
            "must fail closed on the exact final manifest input count"
        ),
        '"${artifacts[@]}"': ("must bind every enumerated publication artifact"),
        "group: core-release-${{ github.ref }}": (
            "must serialize publication attempts for the same ref"
        ),
        "cancel-in-progress: false": (
            "must not cancel an in-flight external publication"
        ),
        'test "$(git rev-parse --is-shallow-repository)" = "false"': (
            "must reject shallow history before enforcing the single-root release"
        ),
        'test "$(git rev-list --max-parents=0 --count HEAD)" = "1"': (
            "must require exactly one parentless public root"
        ),
        'test "$(git rev-parse origin/main)" = "$GITHUB_SHA"': (
            "must require the release tag to equal the exact public main head"
        ),
        "name: rxls-${{ steps.release.outputs.version }}-release-${{ github.run_attempt }}": (
            "must make candidate bundles unique per workflow attempt"
        ),
        '--name "rxls-${version}-release-${baseline_run_attempt}"': (
            "must download the exact baseline attempt bundle"
        ),
        '--name "rxls-${version}-release-${selected_attempt}"': (
            "must download the attested comparison attempt bundle"
        ),
        '"baseline_run_attempt": int(os.environ["BASELINE_RUN_ATTEMPT"])': (
            "must attest the exact baseline workflow attempt"
        ),
        'ARTIFACT_NAME="$artifact"': (
            "must bind comparison evidence to its attempt-specific artifact name"
        ),
        '[[ "$current_attempt" == "$selected_attempt" ]] || continue': (
            "must reject stale comparison artifacts from prior rerun attempts"
        ),
        '"$baseline_attempt" == "$selected_baseline_attempt"': (
            "must require the attested baseline attempt to remain current and successful"
        ),
        "CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}": (
            "must pass the crates.io token without embedding it in command argv"
        ),
        "cargo publish --locked --registry crates-io": (
            "must bind real publication to the attested crates.io registry"
        ),
        'git fetch origin "refs/tags/$GITHUB_REF_NAME" --no-tags': (
            "must refetch the hosted release tag before irreversible publication"
        ),
        'test "$(git rev-parse \'FETCH_HEAD^{commit}\')" = "$GITHUB_SHA"': (
            "must bind the refetched release tag to the publication commit"
        ),
    }
    for snippet, message in required.items():
        if snippet not in active:
            errors.append(f"{path}: {message}")

    checkout_header = f"- uses: {ORACLE_CHECKOUT_ACTION} # v7.0.1"
    checkout_step = _single_yaml_block(
        path,
        active,
        checkout_header,
        6,
        "release checkout step",
        errors,
    )
    if checkout_step.count("fetch-depth: 0") != 1:
        errors.append(f"{path}: release checkout must fetch complete history")
    if checkout_step.count("persist-credentials: false") != 1:
        errors.append(f"{path}: release checkout must not retain write credentials")
    step_headers = [
        (match.start(), match.group(1))
        for match in re.finditer(
            r"^ {6}(- (?:name|uses|run):[^\r\n]+)$", active, re.MULTILINE
        )
    ]
    checkout_positions = [
        index for index, (_, header) in enumerate(step_headers) if header == checkout_header
    ]
    if len(checkout_positions) == 1:
        checkout_position = checkout_positions[0]
        next_header = (
            step_headers[checkout_position + 1][1]
            if checkout_position + 1 < len(step_headers)
            else ""
        )
        if next_header != "- name: Validate release identity":
            errors.append(
                f"{path}: release identity must be the first step after checkout"
            )

    run_pattern = re.compile(
        r"^[ \t]*python3 scripts/check_cargo_publish_dry_run\.py run [\\]\n"
        r"[ \t]+--manifest Cargo\.toml [\\]\n"
        r'[ \t]+--git-sha "\$GITHUB_SHA" [\\]\n'
        r"[ \t]+--output target/package/"
        r"release-cargo-publish-dry-run\.json[ \t]*$",
        re.MULTILINE,
    )
    verify_pattern = re.compile(
        r"^[ \t]*python3 scripts/check_cargo_publish_dry_run\.py verify [\\]\n"
        r"[ \t]+--manifest Cargo\.toml [\\]\n"
        r'[ \t]+--git-sha "\$GITHUB_SHA" [\\]\n'
        r"[ \t]+--receipt ([^\r\n]+?)[ \t]*$",
        re.MULTILINE,
    )
    if len(run_pattern.findall(active)) != 1:
        errors.append(
            f"{path}: expected exactly one dependency-free crates.io dry-run runner"
        )
    if len(verify_pattern.findall(active)) != 7:
        errors.append(
            f"{path}: expected seven exact crates.io dry-run evidence verifications"
        )
    if "cargo publish --dry-run" in active:
        errors.append(
            f"{path}: bare or inline cargo publish dry runs are forbidden; "
            "the evidence runner must execute the exact argv"
        )
    if "cargo publish --locked --token" in active:
        errors.append(
            f"{path}: crates.io token must not be interpolated into command argv"
        )

    identity_step = _single_yaml_block(
        path,
        active,
        "- name: Validate release identity",
        6,
        "release identity step",
        errors,
    )
    exact_main = 'test "$(git rev-parse origin/main)" = "$GITHUB_SHA"'
    if identity_step.count(exact_main) != 1:
        errors.append(
            f"{path}: release identity step must bind the tag to exact origin/main"
        )
    if active.count(exact_main) != 2:
        errors.append(
            f"{path}: exact origin/main must be checked at tag validation and again "
            "immediately before crates.io publication"
        )
    if "git merge-base --is-ancestor" in identity_step:
        errors.append(f"{path}: ancestor-only release tag validation is forbidden")
    one_root = 'test "$(git rev-list --max-parents=0 --count HEAD)" = "1"'
    if identity_step.count(one_root) != 1:
        errors.append(
            f"{path}: release identity step must enforce one complete parentless root"
        )
    total_commit_probes = [
        line
        for line in identity_step.splitlines()
        if "git rev-list" in line
        and "--count" in line
        and "HEAD" in line
        and "--max-parents=0" not in line
    ]
    if total_commit_probes:
        errors.append(
            f"{path}: release identity must not restrict the public history to a "
            "fixed total commit count"
        )
    shallow_check = 'test "$(git rev-parse --is-shallow-repository)" = "false"'
    if identity_step.count(shallow_check) != 1:
        errors.append(f"{path}: release identity must reject shallow history")
    if "scripts/" in identity_step:
        errors.append(f"{path}: release identity must run before repository scripts")

    python_dependencies_step = _single_yaml_block(
        path,
        active,
        "- name: Install public reference readers and render-harness libraries",
        6,
        "release Python dependency step",
        errors,
    )
    if python_dependencies_step.count("python3 -m pip install") != 1:
        errors.append(f"{path}: release must install Python dependencies exactly once")
    for requirement in (
        "CairoSVG==2.9.0",
        "numpy==2.4.4",
        "openpyxl==3.1.5",
        "Pillow==12.3.0",
        "pyxlsb==1.0.10",
        "xlrd==2.0.2",
    ):
        if python_dependencies_step.count(f'"{requirement}"') != 1:
            errors.append(
                f"{path}: release Python dependencies must pin {requirement} exactly once"
            )

    canonical_step = _single_yaml_block(
        path,
        active,
        "- name: Run canonical release gate",
        6,
        "canonical release gate step",
        errors,
    )
    package_gate = (
        "python3 scripts/check_core_package.py target/package/rxls-0.1.3.crate"
    )
    release_build = "cargo build --release --all-features --locked"
    package_index = canonical_step.find(package_gate)
    runner_match = run_pattern.search(canonical_step)
    build_index = canonical_step.find(release_build)
    if not (
        package_index >= 0
        and runner_match is not None
        and package_index < runner_match.start() < build_index
    ):
        errors.append(
            f"{path}: the exact crates.io dry-run runner must follow package validation"
        )
    if runner_match is not None and build_index >= 0:
        dry_run_slice = canonical_step[runner_match.start() : build_index]
        if "<<" in dry_run_slice or "write_text" in dry_run_slice:
            errors.append(f"{path}: inline or heredoc dry-run receipts are forbidden")

    verification_contracts = (
        (
            "- name: Generate release evidence",
            ["dist/release-cargo-publish-dry-run.json"],
            "candidate evidence generation",
        ),
        (
            "- name: Compare clean release candidates",
            [
                "target/baseline-release/release-cargo-publish-dry-run.json",
                "dist/release-cargo-publish-dry-run.json",
            ],
            "candidate comparison",
        ),
        (
            "- name: Require exact-SHA two-candidate publication attestation",
            [
                "target/attested-candidate-release/release-cargo-publish-dry-run.json",
                "dist/release-cargo-publish-dry-run.json",
            ],
            "tag authorization",
        ),
        (
            "- name: Bind publication provenance into release manifest",
            ["dist/release-cargo-publish-dry-run.json"],
            "final publication assembly",
        ),
        (
            "- name: Verify published crate, WASM, docs, assets, and checksums",
            ['"$smoke/assets/release-cargo-publish-dry-run.json"'],
            "post-download release verification",
        ),
    )
    verification_steps: dict[str, str] = {}
    for header, expected_receipts, label in verification_contracts:
        step = _single_yaml_block(path, active, header, 6, label, errors)
        verification_steps[header] = step
        actual_receipts = verify_pattern.findall(step)
        if actual_receipts != expected_receipts:
            errors.append(f"{path}: {label} must verify exactly {expected_receipts!r}")
        if "continue-on-error:" in step:
            errors.append(f"{path}: {label} must fail closed")

    generation_step = verification_steps.get("- name: Generate release evidence", "")
    crate_copy_index = generation_step.find(
        'cp "target/package/rxls-${version}.crate" dist/'
    )
    receipt_copy_index = generation_step.find(
        "cp target/package/release-cargo-publish-dry-run.json dist/"
    )
    generation_verify = verify_pattern.search(generation_step)
    if not (
        crate_copy_index >= 0
        and receipt_copy_index >= 0
        and generation_verify is not None
        and crate_copy_index < generation_verify.start()
        and receipt_copy_index < generation_verify.start()
    ):
        errors.append(
            f"{path}: candidate evidence must verify the copied adjacent crate and receipt"
        )

    comparison_step = verification_steps.get(
        "- name: Compare clean release candidates", ""
    )
    if not (
        0
        <= comparison_step.find('gh run download "$baseline_run_id"')
        < (
            verify_pattern.search(comparison_step).start()
            if verify_pattern.search(comparison_step)
            else -1
        )
    ):
        errors.append(
            f"{path}: candidate comparison must verify evidence after artifact download"
        )

    authorization_step = verification_steps.get(
        "- name: Require exact-SHA two-candidate publication attestation", ""
    )
    authorization_verify = verify_pattern.search(authorization_step)
    if not (
        0
        <= authorization_step.find('gh run download "$selected_run"')
        < (authorization_verify.start() if authorization_verify else -1)
    ):
        errors.append(
            f"{path}: tag authorization must verify the downloaded candidate receipt"
        )

    final_step = verification_steps.get(
        "- name: Bind publication provenance into release manifest", ""
    )
    final_verify = verify_pattern.search(final_step)
    if final_verify is None or not (
        final_verify.start()
        < final_step.find("python3 scripts/public_hygiene_audit.py --json dist")
        < final_step.find("python3 scripts/release_manifest.py")
    ):
        errors.append(
            f"{path}: final assembly must verify dry-run evidence before hygiene and manifest generation"
        )

    post_step = verification_steps.get(
        "- name: Verify published crate, WASM, docs, assets, and checksums", ""
    )
    post_verify = verify_pattern.search(post_step)
    if post_verify is None or not (
        0
        <= post_step.find('gh release download "$tag"')
        < post_verify.start()
        < post_step.find("python3 scripts/release_manifest.py")
    ):
        errors.append(
            f"{path}: post-download verification must validate dry-run evidence before the bundle"
        )

    if active.count("--expected-files 48") != 2:
        errors.append(
            f"{path}: candidate bundle must be verified twice at exactly 48 files"
        )
    if active.count("--expected-files 52") != 3:
        errors.append(
            f"{path}: public bundle must be assembled, published, and downloaded "
            "at exactly 52 files"
        )
    if re.search(r"--expected-files\s+47\b", active):
        errors.append(f"{path}: stale pre-evidence release-bundle count is forbidden")

    candidate_manifest_upload = (
        "            target/reproducibility/rxls-release-candidate-manifest.json"
    )
    if active.count(candidate_manifest_upload) != 1:
        errors.append(
            f"{path}: reproducibility upload must contain one candidate-manifest copy"
        )

    provenance_index = active.find(
        "- name: Bind publication provenance into release manifest"
    )
    crates_publish_index = active.find("- name: Publish to crates.io")
    github_release_index = active.find("- name: Create or update GitHub release")
    if not (0 <= provenance_index < crates_publish_index < github_release_index):
        errors.append(
            f"{path}: provenance binding must precede registry and GitHub publication"
        )

    provenance_step = final_step
    if "continue-on-error:" in provenance_step:
        errors.append(f"{path}: publication provenance assembly must fail closed")
    for command in (
        "python3 scripts/public_hygiene_audit.py --json dist",
        "python3 scripts/release_manifest.py",
        "--verify-bundle dist",
        "--expected-files 52",
    ):
        if command not in provenance_step:
            errors.append(f"{path}: publication provenance step is missing {command!r}")

    crates_publish_step = _single_yaml_block(
        path,
        active,
        "- name: Publish to crates.io",
        6,
        "crates.io publication step",
        errors,
    )
    publish_commands = _normalized_active_commands(crates_publish_step)
    prepublication_commands = (
        "git fetch origin main --no-tags",
        'test "$(git rev-parse origin/main)" = "$GITHUB_SHA"',
        'git fetch origin "refs/tags/$GITHUB_REF_NAME" --no-tags',
        'test "$(git rev-parse \'FETCH_HEAD^{commit}\')" = "$GITHUB_SHA"',
    )
    publish_command = "cargo publish --locked --registry crates-io"
    if any(publish_commands.count(command) != 1 for command in prepublication_commands):
        errors.append(
            f"{path}: crates.io publication must revalidate exact remote main and tag refs"
        )
    if publish_commands.count(publish_command) != 1:
        errors.append(
            f"{path}: crates.io publication must use one exact registry-bound command"
        )
    command_positions = [
        crates_publish_step.find(command)
        for command in (*prepublication_commands, publish_command)
    ]
    if not all(
        left >= 0 and left < right
        for left, right in zip(command_positions, command_positions[1:])
    ):
        errors.append(
            f"{path}: remote ref revalidation must immediately precede crates.io publication"
        )

    github_release_step = _single_yaml_block(
        path,
        active,
        "- name: Create or update GitHub release",
        6,
        "exact GitHub Release reconciliation step",
        errors,
    )
    required_release_step = {
        "if: github.event_name == 'push'": (
            "GitHub Release reconciliation must run only for a tag push"
        ),
        "GH_TOKEN: ${{ github.token }}": (
            "GitHub Release reconciliation must use the scoped workflow token"
        ),
        "shell: bash": "GitHub Release reconciliation must use Bash",
        "set -euo pipefail": "GitHub Release reconciliation must fail closed",
        'tag="$GITHUB_REF_NAME"': (
            "GitHub Release reconciliation must bind the pushed tag"
        ),
    }
    for snippet, message in required_release_step.items():
        if github_release_step.count(snippet) != 1:
            errors.append(f"{path}: {message}")
    exact_reconciliation = (
        "python3 scripts/reconcile_github_release.py "
        '--repository "$GITHUB_REPOSITORY" '
        '--tag "$tag" '
        '--target-commitish "$GITHUB_SHA" '
        "--dist dist "
        "--expected-files 52 "
        "--token-env GH_TOKEN"
    )
    commands = _normalized_active_commands(github_release_step)
    if commands.count(exact_reconciliation) != 1:
        errors.append(
            f"{path}: GitHub Release reconciliation must retain every exact binding"
        )
    if "continue-on-error:" in github_release_step:
        errors.append(f"{path}: GitHub Release reconciliation must not be bypassable")
    if re.search(
        r"\bgh\s+release\s+(?:create|edit|upload|delete)\b", github_release_step
    ):
        errors.append(
            f"{path}: unchecked gh release mutation must not bypass the reconciler"
        )
    if "dist/*" in github_release_step:
        errors.append(
            f"{path}: wildcard GitHub Release upload must not bypass exact inventory"
        )
    return errors


def audit_github_release_reconciler(path: Path, text: str) -> list[str]:
    """Freeze the fail-closed behaviors behind GitHub Release publication."""

    active = _without_commented_lines(text)
    errors: list[str] = []
    required = {
        "if len(entries) != expected_files:": (
            "must inventory the exact local file count"
        ),
        "if path.is_symlink() or not path.is_file():": (
            "must reject non-regular and symlinked local assets"
        ),
        "asset_ids = validate_reconcilable_remote_assets(current_assets)": (
            "must validate the complete hosted inventory before mutation"
        ),
        "client.delete_release_asset(asset_id)": (
            "must delete stale, duplicate-name, and replaced hosted assets"
        ),
        "if remaining_assets != []:": (
            "must prove hosted assets were cleared before replacement"
        ),
        "client.upload_release_asset(release_id, local_assets[name])": (
            "must upload every expected local asset"
        ),
        '{"draft": False, "prerelease": False}': (
            "must normalize draft and prerelease state"
        ),
        "require_published=True": (
            "must require a published release after normalization"
        ),
        "validate_published_assets(": ("must run the exact hosted-asset verifier"),
        "if len(remote_assets) != len(local_assets):": (
            "must verify the exact hosted asset count"
        ),
        "if name in seen_names:": ("must reject duplicate hosted asset names"),
        'if raw.get("state") != "uploaded":': (
            "must require uploaded hosted asset state"
        ),
        "size != local.size": ("must compare hosted and local byte sizes"),
        "DIGEST_RE.fullmatch(digest) is None": (
            "must require canonical hosted SHA-256 metadata"
        ),
        "if digest != local.digest:": ("must compare hosted and local SHA-256 digests"),
        "if seen_names != set(local_assets):": (
            "must compare the exact hosted and local asset-name sets"
        ),
        "SHA_RE.fullmatch(target_commitish) is None": (
            "must require a canonical expected commit SHA"
        ),
        'self._api(f"/git/ref/tags/{encoded_tag}")': (
            "must resolve the explicit hosted tag-ref namespace"
        ),
        "client.get_tag_commit_sha(tag) != target_commitish": (
            "must verify that the hosted tag resolves to the expected commit"
        ),
        "if already_exact:": (
            "must leave an exact published release unchanged on rerun"
        ),
        "if immutable is True:": ("must fail closed for an inexact immutable release"),
        'if release.get("draft") is not True:': (
            "must replace hosted assets only while the release is an explicit draft"
        ),
    }
    for snippet, message in required.items():
        if snippet not in active:
            errors.append(f"{path}: GitHub Release reconciler {message}")
    if active.count("client.get_tag_commit_sha(tag) != target_commitish") != 3:
        errors.append(
            f"{path}: GitHub Release reconciler must verify the tag commit "
            "before mutation and after either idempotent or mutable publication"
        )
    if active.count("require_published=True") != 2:
        errors.append(
            f"{path}: GitHub Release reconciler must require published state "
            "for both idempotent and mutable completion"
        )
    if "        return\n    immutable = release.get" not in active:
        errors.append(
            f"{path}: GitHub Release reconciler exact-release rerun must be a no-op"
        )
    draft_guard = active.find('if release.get("draft") is not True:')
    mutation_start = active.find(
        "asset_ids = validate_reconcilable_remote_assets(current_assets)"
    )
    if draft_guard < 0 or mutation_start < 0 or draft_guard >= mutation_start:
        errors.append(
            f"{path}: GitHub Release reconciler must reject a non-draft release "
            "before any destructive asset mutation"
        )

    try:
        tree = ast.parse(text, filename=str(path))
    except SyntaxError as error:
        errors.append(f"{path}: GitHub Release reconciler is invalid Python: {error}")
        return errors
    allowed_imports = {
        "__future__",
        "argparse",
        "dataclasses",
        "datetime",
        "hashlib",
        "json",
        "os",
        "pathlib",
        "re",
        "ssl",
        "sys",
        "time",
        "typing",
        "urllib",
    }
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            modules = [alias.name.split(".", 1)[0] for alias in node.names]
        elif isinstance(node, ast.ImportFrom):
            modules = [node.module.split(".", 1)[0] if node.module else ""]
        else:
            continue
        for module in modules:
            if module not in allowed_imports:
                errors.append(
                    f"{path}: GitHub Release reconciler dependency {module!r} is forbidden"
                )
    return errors


def audit_semver_gate(path: Path, text: str) -> list[str]:
    """Require the frozen registry baseline on every supported feature surface."""

    active = _without_commented_lines(text)
    errors: list[str] = []
    assignment = re.compile(
        rf'^\s*CARGO_SEMVER_CHECKS_VERSION:\s*["\']?'
        rf'{re.escape(SEMVER_CHECKS_VERSION)}["\']?\s*$',
        re.MULTILINE,
    )
    if assignment.search(active) is None:
        errors.append(
            f"{path}: expected exact CARGO_SEMVER_CHECKS_VERSION={SEMVER_CHECKS_VERSION}"
        )

    install = re.compile(
        r"cargo\s+install\s+cargo-semver-checks\s+"
        r"--version\s+(?:[\"']?\$\{?CARGO_SEMVER_CHECKS_VERSION\}?[\"']?|"
        + re.escape(SEMVER_CHECKS_VERSION)
        + r")\s+--locked(?:\s|$)"
    )
    if install.search(active) is None:
        errors.append(
            f"{path}: cargo-semver-checks install must use exact version "
            f"{SEMVER_CHECKS_VERSION} with --locked"
        )

    prefix = (
        "cargo semver-checks check-release --manifest-path Cargo.toml "
        f"--baseline-version {SEMVER_BASELINE_VERSION} "
        f"--release-type {SEMVER_RELEASE_TYPE}"
    )
    for mode in SEMVER_FEATURE_MODES:
        command = f"{prefix} {mode}"
        if active.count(command) != 1:
            errors.append(
                f"{path}: expected exactly one registry SemVer gate for {mode}"
            )
    return errors


def audit_fuzz_workflow(path: Path, text: str) -> list[str]:
    """Return violations for the standalone hosted fuzz workflow."""

    errors = audit_fuzz_tools(
        path, text, ("FUZZ_NIGHTLY_VERSION", "CARGO_FUZZ_VERSION")
    )
    active = _without_commented_lines(text)
    required = {
        "      target:\n        description: Run the ordinary fuzz campaign or the branch-local Render Oracle workflow\n        required: true\n        default: fuzz\n        type: choice\n        options:\n          - fuzz\n          - render-oracle": (
            "manual dispatch target must default to ordinary fuzz and explicitly "
            "select the oracle bridge"
        ),
        "      baseline_mode:\n        description: Verify a tracked baseline or bootstrap a hosted full candidate\n        required: true\n        default: verify\n        type: choice\n        options:\n          - verify\n          - candidate": (
            "oracle bridge must expose an exact candidate/verify choice"
        ),
        "      campaign:\n        description: Render Oracle campaign (ignored for ordinary fuzz runs)\n        required: true\n        default: pilot\n        type: choice\n        options:\n          - pilot\n          - full\n          - ooxml-row-diagnostic": (
            "oracle bridge must expose the exact pilot/full/OOXML diagnostic choice"
        ),
        "    if: ${{ github.event_name != 'workflow_dispatch' || inputs.target == 'fuzz' }}": (
            "ordinary fuzz must run unchanged for PR/schedule/default manual events"
        ),
        "    if: ${{ github.event_name == 'workflow_dispatch' && inputs.target == 'render-oracle' }}": (
            "oracle bridge must run only after explicit manual selection"
        ),
        "    permissions:\n      contents: read\n    uses: ./.github/workflows/render-oracle.yml": (
            "oracle bridge must use the same-commit local workflow with read-only contents"
        ),
        "      baseline_mode: ${{ inputs.baseline_mode }}": (
            "oracle bridge must pass the selected baseline mode"
        ),
        "      bootstrap_identities: ${{ inputs.bootstrap_identities }}": (
            "oracle bridge must pass the explicit identity bootstrap flag"
        ),
        "      campaign: ${{ inputs.campaign }}": (
            "oracle bridge must pass the selected campaign"
        ),
        "      source_sha: ${{ github.sha }}": (
            "oracle bridge must bind the exact dispatched branch commit"
        ),
    }
    for snippet, message in required.items():
        if active.count(snippet) != 1:
            errors.append(f"{path}: {message}")
    oracle_job = _single_yaml_block(
        path,
        active,
        "render-oracle:",
        2,
        "render-oracle reusable job",
        errors,
    )
    if (
        "steps:" in oracle_job
        or "secrets:" in oracle_job
        or re.findall(r"^\s+uses:\s*(\S+)\s*$", oracle_job, re.MULTILINE)
        != ["./.github/workflows/render-oracle.yml"]
    ):
        errors.append(
            f"{path}: oracle bridge must contain only the reviewed local reusable call"
        )
    return errors


def audit_render_oracle_workflow(path: Path, text: str) -> list[str]:
    """Require exact identities and bounded release/diagnostic campaigns."""

    errors: list[str] = []
    _audit_exact_workflow_sha256(
        path,
        text,
        ORACLE_RENDER_WORKFLOW_SHA256,
        errors,
    )
    active = _without_commented_lines(text)
    _audit_exact_job_step_sequence(
        path,
        active,
        "locked-linux-oracle",
        (
            ORACLE_CHECKOUT_ACTION,
            ORACLE_SETUP_PYTHON_ACTION,
            ORACLE_BUILDX_ACTION,
            ORACLE_UPLOAD_ARTIFACT_ACTION,
            ORACLE_UPLOAD_ARTIFACT_ACTION,
        ),
        ORACLE_RENDER_STEP_SHA256,
        errors,
    )
    _audit_oracle_buildx_setup(path, active, errors)
    oracle_image_build = _single_yaml_block(
        path,
        active,
        "- name: Build and inspect the locked oracle image",
        6,
        "locked oracle image build step",
        errors,
    )
    _audit_oracle_build_retry(
        path,
        oracle_image_build,
        "target/render-oracle-hosted/build.json",
        "target/render-oracle-hosted/build.stderr",
        errors,
    )
    pinned_type0_pdf_gate = _single_yaml_block(
        path,
        active,
        "- name: Run the project-native Type0 PDF Poppler smoke",
        6,
        "pinned Type0 PDF descriptor and Poppler gate step",
        errors,
    )
    expected_pinned_type0_pdf_gate = (
        "      - name: Run the project-native Type0 PDF Poppler smoke\n"
        "        if: ${{ env.RXLS_IDENTITY_BOOTSTRAP != '1' }}\n"
        "        shell: bash\n"
        "        env:\n"
        '          RXLS_REQUIRE_POPPLER: "1"\n'
        "          RXLS_TEST_FONT_PACK_MANIFEST: "
        "${{ github.workspace }}/local/render-fonts/pack/manifest.json\n"
        "        run: |\n"
        "          set -euo pipefail\n"
        '          [[ "$RXLS_TEST_FONT_PACK_MANIFEST" = "$GITHUB_WORKSPACE/"* ]]\n'
        '          test -f "$RXLS_TEST_FONT_PACK_MANIFEST"\n'
        "          command -v pdffonts\n"
        "          command -v pdfinfo\n"
        "          command -v pdftoppm\n"
        "          command -v pdftotext\n"
        "          cargo +1.85.0 test --locked --release --manifest-path "
        "render/Cargo.toml \\\n"
        "            --lib "
        "pdf::tests::project_font_pack_type0_pdf_exposes_exact_poppler_word_tokens \\\n"
        "            -- --exact --list \\\n"
        "            | grep -Fqx "
        "'pdf::tests::project_font_pack_type0_pdf_exposes_exact_poppler_word_tokens: test'\n"
        "          cargo +1.85.0 test --locked --release --manifest-path "
        "render/Cargo.toml \\\n"
        "            --lib "
        "pdf::tests::project_font_pack_type0_pdf_exposes_exact_poppler_word_tokens \\\n"
        "            -- --exact\n"
        "          cargo +1.85.0 test --locked --release --manifest-path "
        "render/Cargo.toml \\\n"
        "            --lib "
        "embed::tests::pinned_arimo_and_noto_faces_match_libreoffice_descriptor_metrics \\\n"
        "            -- --exact --list \\\n"
        "            | grep -Fqx "
        "'embed::tests::pinned_arimo_and_noto_faces_match_libreoffice_descriptor_metrics: test'\n"
        "          cargo +1.85.0 test --locked --release --manifest-path "
        "render/Cargo.toml \\\n"
        "            --lib "
        "embed::tests::pinned_arimo_and_noto_faces_match_libreoffice_descriptor_metrics \\\n"
        "            -- --exact\n"
        "          cargo +1.85.0 test --locked --release --manifest-path "
        "render/Cargo.toml \\\n"
        "            --lib "
        "pdf::tests::pinned_arimo_and_noto_descriptors_match_libreoffice_pdf_metrics \\\n"
        "            -- --exact --list \\\n"
        "            | grep -Fqx "
        "'pdf::tests::pinned_arimo_and_noto_descriptors_match_libreoffice_pdf_metrics: test'\n"
        "          cargo +1.85.0 test --locked --release --manifest-path "
        "render/Cargo.toml \\\n"
        "            --lib "
        "pdf::tests::pinned_arimo_and_noto_descriptors_match_libreoffice_pdf_metrics \\\n"
        "            -- --exact\n"
        "          cargo +1.85.0 test --locked --release --manifest-path "
        "render/Cargo.toml \\\n"
        "            --lib "
        "pdf::tests::pinned_type0_poppler_boxes_follow_libreoffice_descriptor_metrics \\\n"
        "            -- --exact --list \\\n"
        "            | grep -Fqx "
        "'pdf::tests::pinned_type0_poppler_boxes_follow_libreoffice_descriptor_metrics: test'\n"
        "          cargo +1.85.0 test --locked --release --manifest-path "
        "render/Cargo.toml \\\n"
        "            --lib "
        "pdf::tests::pinned_type0_poppler_boxes_follow_libreoffice_descriptor_metrics \\\n"
        "            -- --exact\n"
        "          cargo +1.85.0 test --locked --release --manifest-path "
        "render/Cargo.toml \\\n"
        "            --test printing "
        "deterministic_pdf_reopens_has_exact_page_count_and_extractable_text \\\n"
        "            -- --exact --list \\\n"
        "            | grep -Fqx "
        "'deterministic_pdf_reopens_has_exact_page_count_and_extractable_text: test'\n"
        "          cargo +1.85.0 test --locked --release --manifest-path "
        "render/Cargo.toml \\\n"
        "            --test printing "
        "deterministic_pdf_reopens_has_exact_page_count_and_extractable_text \\\n"
        "            -- --exact"
    )
    if pinned_type0_pdf_gate != expected_pinned_type0_pdf_gate:
        errors.append(
            f"{path}: pinned Type0 PDF descriptor and Poppler gate must retain "
            "the exact verified manifest, fail-closed Poppler environment, and "
            "reviewed exact tests"
        )
    pinned_font_cli_regression = _single_yaml_block(
        path,
        active,
        "- name: Run the pinned-font SinglePageSheets CLI geometry regression",
        6,
        "pinned-font SinglePageSheets CLI geometry regression step",
        errors,
    )
    expected_pinned_font_cli_regression = (
        "      - name: Run the pinned-font SinglePageSheets CLI geometry regression\n"
        "        if: ${{ env.RXLS_IDENTITY_BOOTSTRAP != '1' }}\n"
        "        timeout-minutes: 15\n"
        "        shell: bash\n"
        "        env:\n"
        "          RXLS_TEST_FONT_PACK_MANIFEST: "
        "${{ github.workspace }}/local/render-fonts/pack/manifest.json\n"
        "          RXLS_TEST_FONT_FAMILY: Arimo\n"
        "        run: |\n"
        "          set -euo pipefail\n"
        '          [[ "$RXLS_TEST_FONT_PACK_MANIFEST" = "$GITHUB_WORKSPACE/"* ]]\n'
        '          test -f "$RXLS_TEST_FONT_PACK_MANIFEST"\n'
        "          cargo +1.85.0 test --locked --release --manifest-path "
        "render/Cargo.toml \\\n"
        "            --lib "
        "layout::tests::pinned_calc_ctl_base_face_produces_the_verified_mixed_rtl_row_height \\\n"
        "            -- --exact --list \\\n"
        "            | grep -Fqx "
        "'layout::tests::pinned_calc_ctl_base_face_produces_the_verified_mixed_rtl_row_height: test'\n"
        "          cargo +1.85.0 test --locked --release --manifest-path "
        "render/Cargo.toml \\\n"
        "            --lib "
        "layout::tests::pinned_calc_ctl_base_face_produces_the_verified_mixed_rtl_row_height \\\n"
        "            -- --exact\n"
        "          cargo +1.85.0 test --locked --release --manifest-path "
        "render/Cargo.toml \\\n"
        "            --test printing "
        "cli_single_page_terminal_drawing_keeps_every_geometry_contract_in_sync \\\n"
        "            -- --exact --list \\\n"
        "            | grep -Fqx "
        "'cli_single_page_terminal_drawing_keeps_every_geometry_contract_in_sync: test'\n"
        "          cargo +1.85.0 test --locked --release --manifest-path "
        "render/Cargo.toml \\\n"
        "            --test printing "
        "cli_single_page_terminal_drawing_keeps_every_geometry_contract_in_sync \\\n"
        "            -- --exact"
    )
    if pinned_font_cli_regression != expected_pinned_font_cli_regression:
        errors.append(
            f"{path}: pinned-font SinglePageSheets CLI regression must retain the "
            "exact verified-manifest CTL and Arimo tests bounded to fifteen minutes"
        )
    type0_smoke_index = active.find(
        "- name: Run the project-native Type0 PDF Poppler smoke"
    )
    font_acquisition_index = active.find(
        "- name: Acquire fonts and generate the selected deterministic corpus"
    )
    pinned_font_regression_index = active.find(
        "- name: Run the pinned-font SinglePageSheets CLI geometry regression"
    )
    oracle_image_build_index = active.find(
        "- name: Build and inspect the locked oracle image"
    )
    if not (
        0
        <= font_acquisition_index
        < type0_smoke_index
        < pinned_font_regression_index
        < oracle_image_build_index
    ):
        errors.append(
            f"{path}: pinned Type0 and CLI regressions must run after verified font "
            "pack acquisition and before the oracle image build"
        )
    host_acquisition = _single_yaml_block(
        path,
        active,
        "- name: Acquire exact comparison dependencies",
        6,
        "host comparison acquisition step",
        errors,
    )
    _audit_snapshot_apt_block(
        path,
        host_acquisition,
        "host comparison acquisition",
        ("bootstrap", "all"),
        errors,
    )
    required = {
        '      - "scripts/render_parity_geometry_gate.py"': (
            "must trigger when the shared render-parity geometry gate changes"
        ),
        '      - "scripts/strict_json_contract.py"': (
            "must trigger when the shared type-exact JSON contract changes"
        ),
        f'python-version: "{RENDER_ORACLE_PYTHON_VERSION}"': (
            "must pin the complete Python patch version"
        ),
        "--no-deps": "must disable dependency resolution for the hashed closure",
        "--force-reinstall": "must materialize the exact wheel contents on every run",
        "--only-binary=:all:": "must install binary wheels only",
        "--require-hashes": "must require wheel hashes",
        "--requirement scripts/render-oracle-host-requirements.txt": (
            "must install the checked-in comparison closure"
        ),
        "scripts/render-oracle-host-tools.py verify": (
            "must verify the hosted comparison identity"
        ),
        "scripts/render-oracle-host-tools.py apt-specs --scope all": (
            "normal installs must use the pinned native package closure"
        ),
        "scripts/render-oracle-host-tools.py apt-specs --scope bootstrap": (
            "bootstrap installs must use the snapshot-pinned top-level tools"
        ),
        "scripts/render-oracle-host-tools.py apt-sources": (
            "native packages must come from the locked Ubuntu snapshot"
        ),
        "--output target/render-oracle-hosted/host-tools.json": (
            "must emit path-neutral hosted identity evidence"
        ),
        "python3 scripts/test_generate_ooxml_row_oracle.py": (
            "must test the isolated OOXML row generator before campaign execution"
        ),
        "python3 scripts/test_check_ooxml_row_oracle.py": (
            "must test the privacy-safe OOXML row reducer before campaign execution"
        ),
        'OOXML_ROW_DIAGNOSTIC_CASE_COUNT: "34"': (
            "must keep the OOXML row diagnostic at exactly thirty-four workbooks"
        ),
        'test "$RXLS_BASELINE_MODE" = "verify"': (
            "diagnostic runs must never enter candidate or ratchet mode"
        ),
        'test "$RXLS_IDENTITY_BOOTSTRAP" = "0"': (
            "diagnostic runs must use the already pinned identities"
        ),
        "python3 scripts/generate-ooxml-row-oracle.py --generate": (
            "must generate the isolated project-owned OOXML row corpus"
        ),
        "python3 scripts/generate-ooxml-row-oracle.py --verify": (
            "must verify every generated OOXML row workbook before comparison"
        ),
        '"088db320a0d35494fa8e0a8c33ba95e12a824cfe1b7163c2071cf70528c5d0a2"': (
            "must pin the exact thirty-four-case OOXML row manifest bytes"
        ),
        "lane_args+=(--format xlsx --required-feature ooxml-implicit-row)": (
            "must isolate the diagnostic to the reviewed XLSX feature lane"
        ),
        'test "$OOXML_ROW_DIAGNOSTIC_CASE_COUNT" = "34"': (
            "must run the diagnostic as one exact thirty-four-case campaign"
        ),
        "python3 scripts/check-ooxml-row-oracle.py \\": (
            "must reduce diagnostic geometry through the reviewed privacy gate"
        ),
        "--campaign-manifest local/render-corpus-generated/ooxml-row-diagnostic/manifest.json": (
            "must bind diagnostic evidence to the exact isolated manifest"
        ),
        "--output target/render-oracle-hosted/ooxml-row-oracle.json": (
            "must emit only the reviewed diagnostic aggregate"
        ),
        (
            "if: ${{ env.RXLS_IDENTITY_BOOTSTRAP != '1' && "
            "env.RXLS_ORACLE_CAMPAIGN != 'ooxml-row-diagnostic' }}"
        ): (
            "must keep diagnostic evidence out of release baseline and ratchet aggregation"
        ),
        "- name: Verify and minimize OOXML row diagnostic evidence": (
            "must independently validate and minimize the diagnostic aggregate"
        ),
        'checker["_validate_output"](aggregate)': (
            "must revalidate the exact diagnostic output contract before upload"
        ),
        'assert aggregate["schema"] == "rxls.ooxml-row-oracle.v4"': (
            "must require the expanded diagnostic aggregate schema"
        ),
        '"threshold_max_absolute_height_delta_millipoints": 50': (
            "must retain the accepted sixteen-case baseline regression threshold"
        ),
        "report_path.unlink()": (
            "must delete the raw diagnostic report before staging success evidence"
        ),
        '== {"ooxml-row-oracle.json"}': (
            "diagnostic staging must contain only the aggregate geometry artifact"
        ),
        'assert document["image_identity_status"] == "pinned_match"': (
            "normal oracle builds must require pinned_match"
        ),
        'assert document["schema"] == "rxls.render-oracle-container-build.v3"': (
            "must verify the versioned reproducible image-build evidence"
        ),
        'assert reproducibility["build_count"] == 2': (
            "must require two isolated canonical image builds"
        ),
        'assert len(reproducibility["identities"]) == 2': (
            "must require two complete normalized image identities"
        ),
        'assert reproducibility["identities"][0] == reproducibility["identities"][1]': (
            "must compare both complete normalized image identities"
        ),
        'assert reproducibility["config_ids"] == [image_id, image_id]': (
            "must bind both image config digests to the loaded image ID"
        ),
        'assert len(reproducibility["identity_sha256"]) == 2': (
            "must require exactly two complete image identity hashes"
        ),
        'assert len(set(reproducibility["identity_sha256"])) == 1': (
            "must require identical complete image identities"
        ),
        'assert reproducibility["manifest_digests"] == [manifest_digest, manifest_digest]': (
            "must require identical complete image manifest digests"
        ),
        'assert reproducibility["descriptor_digests"] == [manifest_digest, manifest_digest]': (
            "must bind both image descriptors to the manifest digest"
        ),
        'assert len(reproducibility["descriptor_media_types"]) == 2': (
            "must require exactly two descriptor media types"
        ),
        'assert len(reproducibility["descriptor_sizes"]) == 2': (
            "must require exactly two descriptor sizes"
        ),
        'assert len(reproducibility["rootfs_diff_ids_sha256"]) == 2': (
            "must require exactly two root filesystem DiffID sequences"
        ),
        'assert len(set(reproducibility["rootfs_diff_ids_sha256"])) == 1': (
            "must require identical root filesystem DiffID sequences"
        ),
        'assert document["expected_manifest_digest"] == manifest_digest': (
            "normal oracle builds must require the pinned manifest digest"
        ),
        'assert document["image_identity_status"] == "bootstrap_capture_required"': (
            "bootstrap mode must accept only an initially unpinned image"
        ),
        'assert document["expected_manifest_digest"] is None': (
            "bootstrap mode must not claim a reviewed manifest digest"
        ),
        '"rxls.render-oracle-container-execution.v3"': (
            "campaign rows must require the paired runtime image identity schema"
        ),
        'row["image"]["manifest_digest"] for row in adapters': (
            "campaign rows must bind the observed image manifest digest"
        ),
        'row["image"]["expected_manifest_digest"]': (
            "campaign rows must bind the expected image manifest digest"
        ),
        'oracle_lock["schema"] == "rxls.render-oracle-container-identity.v2"': (
            "campaign configuration must require the paired image identity schema"
        ),
        '"expected_manifest_digest": build["built_manifest_digest"]': (
            "campaign configuration must bind the expected manifest pin"
        ),
        '"manifest_digest": build["built_manifest_digest"]': (
            "campaign configuration must bind the observed manifest digest"
        ),
        'assert host_tools["identity_status"] == "pinned_match"': (
            "normal campaigns must require the pinned host identity"
        ),
        "- name: Stage validated bootstrap identity evidence": (
            "must sanitize bootstrap identity evidence into the isolated upload stage"
        ),
        "target/render-oracle-upload": (
            "must stage only already validated aggregate evidence for upload"
        ),
        "if: ${{ success() }}": (
            "must never upload aggregates after a failed validator or gate"
        ),
        'assert "path" not in normalized_key': (
            "bootstrap evidence must reject every key containing path"
        ),
        'or "path" not in normalized_key': (
            "aggregate evidence must reject source_path_sha256, "
            "host_path_digest, and equivalent key variants"
        ),
        'assert "\\\\" not in value': (
            "must reject backslash-bearing path strings before staging"
        ),
        (
            "approved_retention_policy = (\n"
            "                          allow_retention_policy\n"
            "                          and child_path\n"
            '                          == ("metric_policy", '
            '"paths_or_content_retained")\n'
            "                          and item is False\n"
            "                      )"
        ): ("must allow only the exact false repeatability retention assertion"),
        (
            "allow_retention_policy=(\n"
            '                          aggregate_path.name == "repeatability.json"\n'
            "                      )"
        ): ("must scope the retention-key exception to repeatability evidence"),
        "item, (*key_path, index), allow_retention_policy": (
            "must preserve list position so the retention exception applies "
            "only at the exact direct key path"
        ),
        "traversal.search(value) is None": (
            "must reject relative path traversal before staging"
        ),
        "artifact_extension.search(value) is None": (
            "must reject relative workbook and render artifact names before staging"
        ),
        "aggregate_contracts = {": (
            "must define an exact schema and top-level key contract for every aggregate"
        ),
        "assert set(document) == expected_keys": (
            "must reject injected aggregate top-level fields before upload"
        ),
        "pull_request:\n    types: [labeled]": (
            "must expose only the narrowly filtered pull-request label trigger"
        ),
        "  workflow_call:\n    inputs:": (
            "must expose the exact branch-local reusable workflow entrypoint"
        ),
        "      source_sha:\n        description: Exact caller commit to check out and bind into evidence\n        required: true\n        type: string": (
            "reusable workflow callers must provide an exact source SHA"
        ),
        f"    if: {ORACLE_PR_JOB_CONDITION}": (
            "must reject fork heads and every pull-request label except the exact "
            "pilot/full campaign labels before checkout"
        ),
        f"RXLS_ORACLE_CAMPAIGN: {ORACLE_CAMPAIGN_EXPRESSION}": (
            "pull-request labels must select their exact pilot/full campaign while "
            "push and schedule runs stay on pilot"
        ),
        f"RXLS_BASELINE_MODE: {ORACLE_BASELINE_MODE_EXPRESSION}": (
            "baseline candidate/verify mode must be explicit only for manual or "
            "reusable-workflow runs"
        ),
        f"RXLS_IDENTITY_BOOTSTRAP: {ORACLE_BOOTSTRAP_EXPRESSION}": (
            "identity bootstrap must remain available only to explicit manual runs"
        ),
        f"          ref: {ORACLE_SOURCE_SHA_EXPRESSION}": (
            "must check out the immutable pull-request head SHA"
        ),
        "          persist-credentials: false": (
            "must disable persisted Git credentials in the oracle checkout"
        ),
        f"          EXPECTED_SHA: {ORACLE_SOURCE_SHA_EXPRESSION}": (
            "must bind immediate source verification to the pull-request head SHA"
        ),
        (
            "run: |\n"
            "          set -euo pipefail\n"
            '          test "$(git rev-parse HEAD)" = "$EXPECTED_SHA"\n'
            "          git diff --exit-code\n"
            "          git diff --cached --exit-code"
        ): (
            "must verify the exact source revision and reject tracked or staged "
            "checkout drift under strict Bash"
        ),
        f"timeout-minutes: {ORACLE_TIMEOUT_EXPRESSION}": (
            "must keep pilots at 120 minutes and bound manual or pull-request full "
            "campaigns at 330"
        ),
        f"EXPECTED_SOURCE_SHA: {ORACLE_SOURCE_SHA_EXPRESSION}": (
            "must bind the reproducible image build to the pull-request head SHA"
        ),
        f"EXPECTED_HEAD_SHA: {ORACLE_SOURCE_SHA_EXPRESSION}": (
            "must bind aggregate evidence to the pull-request head SHA"
        ),
        (
            "name: render-oracle-"
            f"{ORACLE_SOURCE_SHA_EXPRESSION}-"
            "${{ github.run_id }}-${{ github.run_attempt }}-"
            f"{ORACLE_CAMPAIGN_EXPRESSION}-"
            f"{ORACLE_BASELINE_MODE_EXPRESSION}"
        ): (
            "must bind the aggregate artifact name to the source SHA, run identity, "
            "and selected campaign"
        ),
        '--profile "$RXLS_ORACLE_CAMPAIGN"': (
            "must generate and verify the selected deterministic profile"
        ),
        'assert set(rows) == {"pdffonts", "pdfinfo", "pdftoppm", "pdftotext"}': (
            "must select the exact four-tool Poppler set from verified host evidence"
        ),
        '--pdffonts-binary-sha256 "$PDFFONTS_SHA256"': (
            "must bind PDF font inspection to the verified host binary"
        ),
        '--host-tools-identity-sha256 "$HOST_TOOLS_IDENTITY_SHA256"': (
            "must bind reports to the complete verified host-tools closure"
        ),
        "- name: Run the project-native Type0 PDF Poppler smoke": (
            "must run the project-owned native PDF Poppler smoke before campaigns"
        ),
        "- name: Smoke the locked oracle runtime": (
            "must run one real locked runtime fixture before the 40-case campaign"
        ),
        (
            "if: ${{ env.RXLS_IDENTITY_BOOTSTRAP != '1' "
            "&& env.RXLS_ORACLE_CAMPAIGN == 'pilot' }}"
        ): ("runtime smoke must run only for normal 40-case pilot campaigns"),
        "python3 scripts/smoke-render-oracle-runtime.py \\": (
            "runtime smoke must use the reviewed bounded adapter preflight"
        ),
        "--lock scripts/render-oracle-container/lock.json \\": (
            "runtime smoke must authenticate the locked wrapper contract"
        ),
        "--manifest local/render-corpus-generated/pilot/manifest.json \\": (
            "runtime smoke must select from the generated project-owned pilot"
        ),
        "--font-pack local/render-fonts/pack \\": (
            "runtime smoke must use the exact verified generated font pack"
        ),
        '--image "$IMAGE_ID"': (
            "runtime smoke must execute the exact image identity produced above"
        ),
        "pdf::tests::project_font_pack_type0_pdf_exposes_exact_poppler_word_tokens": (
            "must attest Type0 embedded text and exact Poppler word boxes"
        ),
        '          RXLS_REQUIRE_POPPLER: "1"': (
            "must fail closed when the native PDF Poppler tools are unavailable"
        ),
        "embed::tests::pinned_arimo_and_noto_faces_match_libreoffice_descriptor_metrics": (
            "must attest raw Arimo and Noto OS/2 Windows descriptor metrics"
        ),
        "pdf::tests::pinned_arimo_and_noto_descriptors_match_libreoffice_pdf_metrics": (
            "must attest scaled Arimo and Noto PDF descriptor metrics"
        ),
        "pdf::tests::pinned_type0_poppler_boxes_follow_libreoffice_descriptor_metrics": (
            "must attest Arimo and Noto Poppler boxes from the pinned descriptors"
        ),
        "deterministic_pdf_reopens_has_exact_page_count_and_extractable_text": (
            "must attest project-native PDF page order and extractable text"
        ),
        "          command -v pdftoppm": (
            "native PDF smoke must require the common Poppler raster backend"
        ),
        '--shard-count "$shard_count"': (
            "full campaigns must use the harness content-identity sharder"
        ),
        'if int(row["sha256"][:16], 16) % 4 == shard_index': (
            "must preflight the same deterministic content-identity shards"
        ),
        "expected_shard_format_counts = (": (
            "full shards must bind the deterministic corpus partition"
        ),
        "(46, 47, 39, 54),": (
            "full shards must retain the exact low-tail format partition"
        ),
        "assert shard_format_counts == expected_shard_format_counts": (
            "every full shard must match the exact deterministic format matrix"
        ),
        "python3 scripts/merge-render-parity-reports.py": (
            "must fail closed while merging complete full-corpus shards"
        ),
        "python3 scripts/check-render-fidelity-targets.py": (
            "must enforce the absolute rendering-fidelity gate"
        ),
        "python3 scripts/compare-render-parity-runs.py": (
            "must compare the two complete same-SHA full campaigns"
        ),
        "python3 scripts/check-render-parity-baseline.py": (
            "must ratchet each complete hosted full campaign against reviewed evidence"
        ),
        "python3 scripts/check-authored-print-parity.py": (
            "must enforce the dedicated authored-print differential gate"
        ),
        "--format xlsx": (
            "authored-print evidence must stay on the attested OOXML lane"
        ),
        "--required-feature print-settings": (
            "authored-print evidence must require explicit print settings"
        ),
        "--print-mode authored": (
            "authored-print evidence must preserve workbook print intent"
        ),
        "--baseline scripts/render-parity-baseline-full.json": (
            "must use the checked-in reviewed full-campaign baseline"
        ),
        "--campaign-manifest local/render-corpus-generated/full/manifest.json": (
            "must bind ratchets to the generated 800-workbook hosted corpus"
        ),
        "--require-hosted-full-800": (
            "must reject acquired-corpus or incorrectly sized baseline evidence"
        ),
        '--candidate-baseline "target/render-oracle-hosted/baseline-candidate-${label}.json"': (
            "must preserve path-neutral baseline candidates for review"
        ),
        "for label in a b; do": (
            "must apply the reviewed ratchet to both same-SHA full campaigns"
        ),
        'test "$(cat target/render-oracle-hosted/gate-status.txt)" = "0"': (
            "must fail closed after detailed campaign reports are removed"
        ),
        'rm -- "${shard_reports[@]}"': (
            "must remove detailed shard reports after exact merging"
        ),
        "for report_path in report_paths:": (
            "must remove detailed campaign reports after aggregation"
        ),
        "authored_report_path.unlink()": (
            "must remove the detailed authored-print report after aggregation"
        ),
        'assert authored_gate["schema"] == "rxls.authored-print-parity.v2"': (
            "must verify the aggregate authored-print gate schema"
        ),
        'assert authored_gate["passed"] is True': (
            "must reject failed authored-print aggregate evidence"
        ),
        '"fit": expected_authored_print // 2': (
            "must bind half of authored-print workbooks to fit-to-page mode"
        ),
        '"scale": expected_authored_print // 2': (
            "must bind half of authored-print workbooks to explicit-scale mode"
        ),
        "== expected_authored_print_pages": (
            "must validate the mixed authored-print page total"
        ),
        "== expected_authored_print_page_count_histogram": (
            "must validate authored-print page counts by pagination mode"
        ),
        '"pages_per_workbook_by_scale_mode"': (
            "must preserve the fit-one-page and scale-four-page contract"
        ),
        'authored_gate["evidence"]["oracle_libreoffice_artifact_sha256"]': (
            "must bind authored-print evidence to the locked LibreOffice artifact"
        ),
        "for name, digest in poppler_sha256.items():": (
            "must cross-bind every gate to all four exact Poppler executables"
        ),
        'authored_gate["evidence"][f"{name}_sha256"] == digest': (
            "must bind authored-print evidence to every pinned Poppler executable"
        ),
        '"schema": "rxls.render-oracle-hosted-campaign.v7"': (
            "must emit the aggregate-only hosted campaign contract"
        ),
        '"rxls.render-parity-observed-candidate.v1"': (
            "hosted baseline candidates must retain raw-derived histograms"
        ),
        '"source_evidence"': (
            "baseline gates must bind the exact source report receipt"
        ),
        'gate["evidence"]["bytes"] == len(report_payload)': (
            "fidelity gates must bind the exact source report byte count"
        ),
        '"baseline_candidate_bytes": len(': (
            "hosted evidence runs must bind baseline candidate bytes"
        ),
        '"baseline_gate_bytes": len(gate_payload)': (
            "hosted evidence runs must bind baseline gate bytes"
        ),
        '"fidelity_gate_bytes": len(gate_payload)': (
            "hosted evidence runs must bind fidelity gate bytes"
        ),
        '"5c6466a53e4328bb50f04cd3c63d102bf53da1a6b3478380f3724574c31b248d"': (
            "full hosted evidence must bind the authoritative manifest bytes"
        ),
        '"45dfaaac5e94e98da038c561d98eed48e8785f56749760d39bac8a720b132db9"': (
            "full hosted evidence must bind the authoritative campaign input identity"
        ),
        '"0ed4f623a243da0b3bee6f6a5d05359fca2e5b7ce51c79e399f0a720a10ebd89"': (
            "full hosted evidence must bind the authoritative renderer input digest set"
        ),
        '"559cf641df08738419af941f30c35a831ca9d000e85ab1e5753c391486f0d251"': (
            "observed candidates must bind the exact correlated group topology"
        ),
        "reject_path_bearing_strings(\n                          key,": (
            "aggregate privacy validation must inspect dictionary keys as data"
        ),
        '"baseline_mode": baseline_mode': (
            "aggregate evidence must bind candidate/verify baseline mode"
        ),
        '"acquired_corpus_included": False': (
            "must distinguish the 800-case hosted corpus from acquired-corpus evidence"
        ),
        '"scope": "project_generated_hosted_acceptance"': (
            "must label the bounded hosted acceptance corpus explicitly"
        ),
        'assert warning_policy["unclassified_codes"] == []': (
            "must reject every warning code absent from the reviewed baseline"
        ),
        '"reviewed_baseline_available": (': (
            "must distinguish a reviewed ratchet from a bootstrap candidate"
        ),
        "test ! -e scripts/render-parity-baseline-full.json || STATUS=1": (
            "candidate mode must require the reviewed baseline to be absent"
        ),
        "git ls-files --error-unmatch scripts/render-parity-baseline-full.json": (
            "verify mode must require a tracked reviewed baseline"
        ),
        "cmp --silent": (
            "candidate bootstrap must authenticate the emitted review candidate"
        ),
        'gate["evidence"]["oracle_build_contract_sha256"]': (
            "must bind absolute-gate evidence to the exact container build"
        ),
        'gate["evidence"]["oracle_image_config_digest"]': (
            "must bind absolute-gate evidence to the pinned OCI image"
        ),
        'gate["evidence"]["oracle_image_manifest_digest"]': (
            "must bind absolute-gate evidence to the pinned image manifest"
        ),
        '"source_commit": build["source_commit"]': (
            "aggregate evidence must preserve the exact source commit"
        ),
        '"wrapper_sha256": build["wrapper_sha256"]': (
            "aggregate evidence must preserve the authenticated wrapper identity"
        ),
        'gate["evidence"][f"{name}_sha256"] == digest': (
            "must bind absolute-gate evidence to every pinned Poppler executable"
        ),
        'report["configuration"]["manifest_binding"] == corpus_binding': (
            "must bind each full report to the exact generated corpus mapping"
        ),
        'authored_report["configuration"]["manifest_binding"] == authored_binding': (
            "must bind authored-print reports to the deterministic manifest subset"
        ),
        'gate["evidence"]["host_tools_identity_sha256"]': (
            "must preserve the complete host-tools closure through aggregate gates"
        ),
        "validate_native_pdf_and_page_evidence(": (
            "must verify native Type3, page-order, bbox, and point geometry evidence"
        ),
        'gate["metrics"]["pdf_point_geometry_mismatches"] == 0': (
            "must reject direct PDF point-geometry mismatches"
        ),
        "- name: Verify evidence source remained exact and clean": (
            "must reverify the exact clean source immediately before upload"
        ),
        'test -z "$(git status --porcelain=v1 --untracked-files=all)"': (
            "must reject late tracked or untracked source-tree drift"
        ),
        "compression-level: 9": "must bound aggregate artifact transfer size",
        "python3 scripts/summarize-render-oracle-failure.py \\": (
            "must reduce failed detailed reports through the reviewed sanitizer"
        ),
        "python3 scripts/test_summarize_render_oracle_failure.py": (
            "must test the failure sanitizer before any campaign can invoke it"
        ),
        (
            "python3 -m unittest "
            "scripts.test_check_render_oracle_release_evidence."
            "RenderOracleReleaseEvidenceTests."
            "test_failure_summary_validator_is_bound_private_and_fail_closed"
        ): (
            "must test the independent failure-summary consumer contract "
            "before any campaign can invoke it"
        ),
        "--input-root target/render-oracle-hosted \\": (
            "failure diagnostics may read only the fixed hosted report root"
        ),
        "--output target/render-oracle-failure/render-oracle-failure-summary.json": (
            "must stage only the canonical sanitized failure summary"
        ),
        (
            "name: render-oracle-failure-"
            f"{ORACLE_SOURCE_SHA_EXPRESSION}-"
            "${{ github.run_id }}-${{ github.run_attempt }}"
        ): (
            "failure diagnostics must bind their artifact to the exact source SHA, "
            "run ID, and attempt"
        ),
        "steps.render_oracle_failure_evidence.outcome == 'success'": (
            "must upload failure diagnostics only after successful validation"
        ),
        "validate_failure_summary(": (
            "must independently validate the exact generated failure artifact"
        ),
    }
    for snippet, message in required.items():
        if snippet not in active:
            errors.append(f"{path}: {message}")
    path_guard_definitions = {
        "artifact_extension = re.compile(": (
            "must define the artifact-extension guard independently in both "
            "evidence sanitizers"
        ),
        'traversal = re.compile(r"(?:^|[\\\\/])\\.\\.(?:$|[\\\\/])")': (
            "must define the traversal guard independently in both evidence "
            "sanitizers"
        ),
    }
    for snippet, message in path_guard_definitions.items():
        if active.count(snippet) != 2:
            errors.append(f"{path}: {message}")
    if re.search(r"python-version:\s*[\"']?3\.13[\"']?\s*$", text, re.MULTILINE):
        errors.append(f"{path}: mutable Python minor selectors are forbidden")
    if "runtime_verified_unpinned" in text or "runtime_verified" in text:
        errors.append(
            f"{path}: normal oracle gates must not accept unpinned identities"
        )
    candidate_branch = re.search(
        r"(?ms)^\s{14}candidate\)\s*$"
        r"(?P<body>.*?)"
        r"^\s{16};;\s*$",
        active,
    )
    if (
        active.count("--create") != 1
        or candidate_branch is None
        or candidate_branch.group("body").count("--create") != 1
        or "scripts/render-parity-baseline-full.json"
        not in candidate_branch.group("body")
    ):
        errors.append(
            f"{path}: baseline creation must be isolated to the fail-closed "
            "candidate branch"
        )

    exact_assignments = {
        "FULL_CASE_COUNT": RENDER_ORACLE_FULL_CASES,
        "FULL_REPEAT_COUNT": RENDER_ORACLE_FULL_REPEATS,
        "FULL_SHARD_COUNT": RENDER_ORACLE_FULL_SHARDS,
        "MAX_PARALLEL_SHARDS": RENDER_ORACLE_MAX_PARALLEL_SHARDS,
        "OOXML_ROW_DIAGNOSTIC_CASE_COUNT": RENDER_ORACLE_DIAGNOSTIC_CASES,
    }
    for name, value in exact_assignments.items():
        assignment = re.compile(
            rf"^\s*{re.escape(name)}:\s*[\"']?{re.escape(value)}[\"']?\s*$",
            re.MULTILINE,
        )
        if len(assignment.findall(text)) != 1:
            errors.append(f"{path}: expected exact {name}={value}")

    campaign_input = re.search(
        r"(?ms)^\s{6}campaign:\s*$"
        r"(?P<body>.*?)(?=^\s{6}baseline_mode:\s*$)",
        text,
    )
    if campaign_input is None:
        errors.append(
            f"{path}: missing workflow_dispatch release/diagnostic campaign choice"
        )
    else:
        body = campaign_input.group("body")
        if (
            "type: choice" not in body
            or "default: pilot" not in body
            or len(re.findall(r"^\s+- pilot\s*$", body, re.MULTILINE)) != 1
            or len(re.findall(r"^\s+- full\s*$", body, re.MULTILINE)) != 1
            or len(
                re.findall(
                    r"^\s+- ooxml-row-diagnostic\s*$",
                    body,
                    re.MULTILINE,
                )
            )
            != 1
        ):
            errors.append(
                f"{path}: workflow_dispatch campaign must be the exact "
                "pilot/full/OOXML diagnostic choice"
            )
    baseline_input = re.search(
        r"(?ms)^\s{6}baseline_mode:\s*$"
        r"(?P<body>.*?)(?=^\s{6}bootstrap_identities:\s*$)",
        text,
    )
    if baseline_input is None:
        errors.append(
            f"{path}: missing workflow_dispatch candidate/verify baseline choice"
        )
    else:
        body = baseline_input.group("body")
        if (
            "type: choice" not in body
            or "default: verify" not in body
            or len(re.findall(r"^\s+- verify\s*$", body, re.MULTILINE)) != 1
            or len(re.findall(r"^\s+- candidate\s*$", body, re.MULTILINE)) != 1
        ):
            errors.append(
                f"{path}: workflow_dispatch baseline mode must be an exact "
                "candidate/verify choice"
            )

    if re.search(r"--max-(?:similarity|blur|mask)-drift-ppm(?:=|\s)", text):
        errors.append(
            f"{path}: same-SHA drift thresholds must use the calibrated checked-in defaults"
        )
    if text.count('test "$FULL_REPEAT_COUNT" = "2"') != 1:
        errors.append(f"{path}: full mode must require exactly two same-SHA campaigns")
    if text.count('test "$FULL_SHARD_COUNT" = "4"') != 1:
        errors.append(
            f"{path}: full mode must require exactly four deterministic shards"
        )
    if text.count('test "$MAX_PARALLEL_SHARDS" = "2"') != 1:
        errors.append(f"{path}: full mode must cap concurrent shard processes at two")
    if (
        len(
            re.findall(
                r"^\s*python3 scripts/check-render-fidelity-targets\.py\s+\\$",
                text,
                re.MULTILINE,
            )
        )
        != 2
    ):
        errors.append(
            f"{path}: pilot/full evidence needs one absolute gate per campaign"
        )
    if text.count("| tee target/render-oracle-hosted/fidelity-") != 2:
        errors.append(
            f"{path}: absolute-gate aggregate diagnostics must remain visible on failure"
        )
    if text.count("| tee target/render-oracle-hosted/authored-print-gate.json") != 1:
        errors.append(
            f"{path}: authored-print aggregate diagnostics must remain visible on failure"
        )

    late_clean_marker = "      - name: Verify evidence source remained exact and clean"
    upload_marker = "      - name: Upload path-neutral aggregate identities only"
    late_clean_index = active.find(late_clean_marker)
    upload_index = active.find(upload_marker)
    next_step = (
        active.find("\n      - ", late_clean_index + len(late_clean_marker))
        if late_clean_index >= 0
        else -1
    )
    if (
        active.count(late_clean_marker) != 1
        or active.count(upload_marker) != 1
        or upload_index < 0
        or next_step != upload_index - 1
    ):
        errors.append(
            f"{path}: the exact clean-source verifier must be the final step before upload"
        )
    if active.count("          persist-credentials: false") != 1:
        errors.append(
            f"{path}: oracle checkout must disable persisted credentials exactly once"
        )

    upload = re.search(
        r"(?ms)^\s+- name: Upload path-neutral aggregate identities only\s*$"
        r".*?^\s+path:\s*\|\s*$\n(?P<paths>(?:\s+target/[^\n]+\n)+)"
        r"\s+compression-level:\s*9\s*$",
        text,
    )
    allowed_artifacts = {
        "target/render-oracle-upload/authored-print-gate.json",
        "target/render-oracle-upload/baseline-candidate-a.json",
        "target/render-oracle-upload/baseline-candidate-b.json",
        "target/render-oracle-upload/baseline-gate-a.json",
        "target/render-oracle-upload/baseline-gate-b.json",
        "target/render-oracle-upload/build.json",
        "target/render-oracle-upload/fidelity-a.json",
        "target/render-oracle-upload/fidelity-b.json",
        "target/render-oracle-upload/hosted-summary.json",
        "target/render-oracle-upload/host-tools.json",
        "target/render-oracle-upload/ooxml-row-oracle.json",
        "target/render-oracle-upload/repeatability.json",
        "target/render-oracle-upload/renderer.json",
    }
    if upload is None:
        errors.append(f"{path}: aggregate-only artifact allowlist is missing")
    else:
        uploaded = {
            line.strip() for line in upload.group("paths").splitlines() if line.strip()
        }
        if uploaded != allowed_artifacts:
            errors.append(
                f"{path}: hosted artifacts must use the exact aggregate-only allowlist"
            )

    failure_evidence = _single_yaml_block(
        path,
        active,
        ("- name: Generate and validate sanitized Render Oracle failure evidence"),
        6,
        "Render Oracle failure-evidence generation step",
        errors,
    )
    failure_evidence_required = {
        "id: render_oracle_failure_evidence",
        "if: ${{ failure() && env.RXLS_IDENTITY_BOOTSTRAP != '1' }}",
        "shell: bash",
        f"EXPECTED_HEAD_SHA: {ORACLE_SOURCE_SHA_EXPRESSION}",
        "set -euo pipefail",
        "python3 scripts/summarize-render-oracle-failure.py \\",
        "--input-root target/render-oracle-hosted \\",
        '--profile "$RXLS_ORACLE_CAMPAIGN" \\',
        '--baseline-mode "$RXLS_BASELINE_MODE" \\',
        '--head-sha "$EXPECTED_HEAD_SHA" \\',
        "--output target/render-oracle-failure/render-oracle-failure-summary.json",
        (
            'python3 - "$EXPECTED_HEAD_SHA" "$RXLS_ORACLE_CAMPAIGN" '
            "\"$RXLS_BASELINE_MODE\" <<'PY'"
        ),
        "from scripts.check_render_oracle_release_evidence import (",
        "validate_failure_summary,",
        "validate_failure_summary(",
        "head_sha=sys.argv[1]",
        "profile=sys.argv[2]",
        "baseline_mode=sys.argv[3]",
    }
    if any(value not in failure_evidence for value in failure_evidence_required):
        errors.append(
            f"{path}: failed reports must pass through the exact bounded "
            "path-neutral summary and independent validation commands"
        )

    failure_upload = _single_yaml_block(
        path,
        active,
        "- name: Upload sanitized Render Oracle failure summary",
        6,
        "Render Oracle failure-summary upload",
        errors,
    )
    failure_upload_required = {
        "id: render_oracle_failure_upload",
        (
            "if: ${{ failure() && env.RXLS_IDENTITY_BOOTSTRAP != '1' && "
            "steps.render_oracle_failure_evidence.outcome == 'success' }}"
        ),
        f"uses: {ORACLE_UPLOAD_ARTIFACT_ACTION}",
        (
            "name: render-oracle-failure-"
            f"{ORACLE_SOURCE_SHA_EXPRESSION}-"
            "${{ github.run_id }}-${{ github.run_attempt }}"
        ),
        "path: target/render-oracle-failure/render-oracle-failure-summary.json",
        "compression-level: 9",
        "if-no-files-found: error",
        "retention-days: 14",
    }
    if any(value not in failure_upload for value in failure_upload_required):
        errors.append(
            f"{path}: failure upload must contain only the exact-SHA sanitized "
            "summary artifact"
        )
    if (
        failure_upload.count(
            "path: target/render-oracle-failure/render-oracle-failure-summary.json"
        )
        != 1
        or "target/render-oracle-hosted/" in failure_upload
        or "local/" in failure_upload
        or "*.json" in failure_upload
    ):
        errors.append(
            f"{path}: failure upload cannot include raw reports, corpora, or wildcards"
        )

    failure_overview = _single_yaml_block(
        path,
        active,
        "- name: Append bounded Render Oracle failure overview",
        6,
        "Render Oracle failure overview",
        errors,
    )
    failure_overview_required = {
        (
            "if: ${{ failure() && env.RXLS_IDENTITY_BOOTSTRAP != '1' && "
            "steps.render_oracle_failure_evidence.outcome == 'success' && "
            "steps.render_oracle_failure_upload.outcome == 'success' }}"
        ),
        "continue-on-error: true",
        "shell: bash",
        "set -euo pipefail",
        "python3 - \"$GITHUB_STEP_SUMMARY\" <<'PY'",
        '"### Sanitized Render Oracle failure evidence"',
        'summary["reports"]',
        "fidelity['semantic_visible_characters']['f1_ppm']",
        "fidelity['poppler_words']['f1_ppm']",
        "fidelity['poppler_lines']['f1_ppm']",
        "fidelity['raster']['similarity_ppm']",
    }
    if any(value not in failure_overview for value in failure_overview_required):
        errors.append(
            f"{path}: the failure overview must be bounded, non-blocking, "
            "and consume only successfully uploaded sanitized evidence"
        )
    if (
        "cat target/render-oracle-failure/"
        "render-oracle-failure-summary.json" in failure_overview
    ):
        errors.append(
            f"{path}: the bounded job overview cannot append the complete "
            "failure JSON to GITHUB_STEP_SUMMARY"
        )
    evidence_position = active.find(
        "- name: Generate and validate sanitized Render Oracle failure evidence"
    )
    upload_position = active.find(
        "- name: Upload sanitized Render Oracle failure summary"
    )
    overview_position = active.find(
        "- name: Append bounded Render Oracle failure overview"
    )
    if not (0 <= evidence_position < upload_position < overview_position):
        errors.append(
            f"{path}: validated failure evidence must be uploaded before "
            "any best-effort presentation"
        )

    apt_lines = [line for line in active.splitlines() if "apt-get " in line]
    if len(apt_lines) != 2 or any(
        '"${APT_OPTIONS[@]}"' not in line for line in apt_lines
    ):
        errors.append(
            f"{path}: apt must be confined to the exact snapshot acquisition step"
        )
    if "bootstrap_identities:" not in text or "--bootstrap-identities" not in text:
        errors.append(f"{path}: missing deliberate identity bootstrap path")
    if text.count("python3 -m pip install") != 1:
        errors.append(f"{path}: comparison dependencies need one hashed pip install")
    if text.count("if: ${{ env.RXLS_IDENTITY_BOOTSTRAP != '1' }}") < 4:
        errors.append(f"{path}: bootstrap runs must not execute parity campaign gates")
    return errors


def audit_render_hardening_workflow(path: Path, text: str) -> list[str]:
    """Require scoped, fail-closed host and OCI rendering identity gates."""

    errors: list[str] = []
    _audit_exact_workflow_sha256(
        path,
        text,
        ORACLE_HARDENING_WORKFLOW_SHA256,
        errors,
    )
    active = _without_commented_lines(text)

    pull_request = _single_yaml_block(
        path, active, "pull_request:", 2, "pull_request trigger", errors
    )
    for trigger_path in (
        '      - "scripts/render-oracle-container/**"',
        '      - "scripts/run-render-oracle-container.py"',
        '      - "scripts/test_render_oracle_container.py"',
        '      - "scripts/test_workflow_policy.py"',
    ):
        if trigger_path not in pull_request.splitlines():
            errors.append(
                f"{path}: pull requests must trigger hardening for {trigger_path.strip()[2:]}"
            )

    pdf_job = _single_yaml_block(path, active, "pdf:", 2, "pdf job", errors)
    pdf_runners = re.findall(r"^\s{4}runs-on:\s*(\S+)\s*$", pdf_job, re.MULTILINE)
    if pdf_runners != ["ubuntu-24.04"]:
        errors.append(f"{path}: PDF hardening must use only ubuntu-24.04")
    if pdf_job.count('      RXLS_REQUIRE_POPPLER: "1"') != 1:
        errors.append(
            f"{path}: PDF hardening tests must fail closed on the pinned Poppler tools"
        )
    if 'python-version: "3.13.14"' not in pdf_job:
        errors.append(
            f"{path}: PDF hardening must match the render-oracle Python identity"
        )
    pdf_policy_step = _single_yaml_block(
        path,
        pdf_job,
        "- name: Enforce hosted workflow policy",
        6,
        "PDF policy step",
        errors,
    )
    expected_pdf_policy_step = (
        "      - name: Enforce hosted workflow policy\n"
        "        shell: bash\n"
        "        run: |\n"
        "          set -euo pipefail\n"
        "          python3 scripts/check_workflow_policy.py\n"
        "          python3 scripts/test_workflow_policy.py"
    )
    if pdf_policy_step != expected_pdf_policy_step:
        errors.append(
            f"{path}: PDF job must fail closed through the checker and its "
            "focused mutation suite"
        )

    host_bootstrap = _single_yaml_block(
        path,
        pdf_job,
        "- name: Capture an unpinned host identity and fail closed",
        6,
        "host identity bootstrap step",
        errors,
    )
    for snippet, message in {
        'if [[ "$EXPECTED_IDENTITY" != "null" ]]; then': (
            "host bootstrap must run only while the reviewed identity is absent"
        ),
        "scripts/render-oracle-host-tools.py apt-specs --scope bootstrap": (
            "host bootstrap must use the exact snapshot-pinned tools"
        ),
        'echo "Review and pin the uploaded host identity before this gate can pass." >&2': (
            "host bootstrap must explain the deliberate failure"
        ),
    }.items():
        if snippet not in host_bootstrap:
            errors.append(f"{path}: {message}")
    host_bootstrap_commands = _normalized_active_commands(host_bootstrap)
    for command, message in {
        (
            "python3 -m pip install --disable-pip-version-check --force-reinstall "
            "--no-deps --only-binary=:all: --require-hashes --requirement "
            "scripts/render-oracle-host-requirements.txt"
        ): "host bootstrap must install the exact hash-locked Python wheel closure",
        (
            "python3 scripts/render-oracle-host-tools.py verify --scope all "
            "--bootstrap-identities --output target/poppler-identity.json"
        ): "host bootstrap must emit complete typed identity evidence",
    }.items():
        if command not in host_bootstrap_commands:
            errors.append(f"{path}: {message}")
    if host_bootstrap_commands.count("exit 1") != 1:
        errors.append(f"{path}: unpinned host identity capture must fail closed")
    _audit_snapshot_apt_block(
        path,
        host_bootstrap,
        "host identity bootstrap",
        ("bootstrap",),
        errors,
    )

    strict_host = _single_yaml_block(
        path,
        pdf_job,
        "- name: Verify the pinned Poppler PDF gate and complete native closure",
        6,
        "strict Poppler verification step",
        errors,
    )
    strict_commands = _normalized_active_commands(strict_host)
    for command, message in {
        "python3 scripts/render-oracle-host-tools.py apt-specs --scope poppler": (
            "strict PDF gate must install the pinned Poppler closure"
        ),
        (
            'sudo apt-get "${APT_OPTIONS[@]}" install --yes '
            "--no-install-recommends --allow-downgrades "
            '"${SYSTEM_PACKAGES[@]}"'
        ): ("strict PDF gate must install only exact locked package specs"),
        (
            "python3 scripts/render-oracle-host-tools.py verify --scope poppler "
            "--output target/poppler-identity.json"
        ): "strict PDF gate must verify and record the complete Poppler closure",
    }.items():
        if command not in strict_commands:
            errors.append(f"{path}: {message}")
    _audit_snapshot_apt_block(
        path,
        strict_host,
        "strict Poppler verification",
        ("poppler",),
        errors,
    )
    bootstrap_index = pdf_job.find("Capture an unpinned host identity and fail closed")
    strict_index = pdf_job.find("Verify the pinned Poppler PDF gate")
    if bootstrap_index < 0 or strict_index < 0 or bootstrap_index >= strict_index:
        errors.append(f"{path}: host bootstrap must precede the strict PDF gate")

    pdf_exact_test_gate = _single_yaml_block(
        path,
        pdf_job,
        "- name: Reopen deterministic PDFs and extract Latin, Korean, and RTL text",
        6,
        "deterministic PDF exact-test gate",
        errors,
    )
    expected_pdf_exact_test_gate = (
        "      - name: Reopen deterministic PDFs and extract Latin, Korean, and RTL text\n"
        "        run: |\n"
        "          set -euo pipefail\n"
        "          cargo test --locked --manifest-path render/Cargo.toml \\\n"
        "            --lib "
        "pdf::tests::clipped_ods_paragraph_group_retains_full_semantics_without_changing_paint \\\n"
        "            -- --exact --list \\\n"
        "            | grep -Fqx "
        "'pdf::tests::clipped_ods_paragraph_group_retains_full_semantics_without_changing_paint: test'\n"
        "          cargo test --locked --manifest-path render/Cargo.toml \\\n"
        "            --lib "
        "pdf::tests::clipped_ods_paragraph_group_retains_full_semantics_without_changing_paint \\\n"
        "            -- --exact\n"
        "          cargo test --locked --manifest-path render/Cargo.toml \\\n"
        "            --test printing "
        "deterministic_pdf_reopens_has_exact_page_count_and_extractable_text \\\n"
        "            -- --exact --list \\\n"
        "            | grep -Fqx "
        "'deterministic_pdf_reopens_has_exact_page_count_and_extractable_text: test'\n"
        "          cargo test --locked --manifest-path render/Cargo.toml \\\n"
        "            --test printing "
        "deterministic_pdf_reopens_has_exact_page_count_and_extractable_text \\\n"
        "            -- --exact"
    )
    if pdf_exact_test_gate != expected_pdf_exact_test_gate:
        errors.append(
            f"{path}: pinned Poppler exact-test gate must discover and execute the "
            "ODS semantic regression before the preserved printing regression"
        )

    image_job = _single_yaml_block(
        path, active, "oracle-image:", 2, "oracle-image job", errors
    )
    _audit_exact_job_step_sequence(
        path,
        active,
        "oracle-image",
        (
            ORACLE_CHECKOUT_ACTION,
            ORACLE_BUILDX_ACTION,
            ORACLE_UPLOAD_ARTIFACT_ACTION,
        ),
        ORACLE_HARDENING_IMAGE_STEP_SHA256,
        errors,
    )
    image_runners = re.findall(r"^\s{4}runs-on:\s*(\S+)\s*$", image_job, re.MULTILINE)
    if image_runners != ["ubuntu-24.04"]:
        errors.append(f"{path}: oracle-image job must use only ubuntu-24.04")
    if "    name: locked LibreOffice oracle image" not in image_job.splitlines():
        errors.append(f"{path}: oracle-image job must retain its reviewed identity")
    _audit_oracle_buildx_setup(path, image_job, errors)
    image_policy_step = _single_yaml_block(
        path,
        image_job,
        "- name: Enforce hosted workflow policy",
        6,
        "oracle-image policy step",
        errors,
    )
    if "run: python3 scripts/check_workflow_policy.py" not in image_policy_step:
        errors.append(
            f"{path}: oracle-image job must actively enforce hosted workflow policy"
        )
    image_build = _single_yaml_block(
        path,
        image_job,
        "- name: Build and verify the locked oracle image",
        6,
        "oracle-image build step",
        errors,
    )
    _audit_oracle_build_retry(
        path,
        image_build,
        "target/render-oracle-image-build.json",
        "target/render-oracle-image-build.stderr",
        errors,
    )
    for snippet, message in {
        'if [[ "$EXPECTED_IMAGE_ID" == "null" ]]; then': (
            "oracle image bootstrap must run only while the pin is absent"
        ),
        "BOOTSTRAP_ARGS+=(--bootstrap-identities)": (
            "oracle image bootstrap must pass the explicit bootstrap argument"
        ),
        'test "$(git rev-parse HEAD)" = "$EXPECTED_SOURCE_SHA"': (
            "oracle image evidence must bind to the exact checked-out source"
        ),
        'test -z "$(git status --porcelain=v1 --untracked-files=all)"': (
            "oracle image evidence must come from a clean source tree"
        ),
        'assert evidence["source_commit"] == expected_source, evidence': (
            "oracle image evidence must authenticate its exact source commit"
        ),
        (
            'assert evidence["wrapper_sha256"] == live_wrapper_sha256 '
            '== lock["wrapper"]["sha256"], evidence'
        ): "oracle image evidence must authenticate the reviewed wrapper",
        'assert evidence["image_identity_status"] == "bootstrap_capture_required", evidence': (
            "unpinned image evidence must have the bootstrap status"
        ),
        'assert evidence["schema"] == "rxls.render-oracle-container-build.v3", evidence': (
            "oracle image evidence must use the reproducible-build schema"
        ),
        'assert reproducibility["build_count"] == 2, reproducibility': (
            "oracle image evidence must prove two isolated builds"
        ),
        'assert len(reproducibility["identities"]) == 2, reproducibility': (
            "oracle image evidence must contain both normalized identities"
        ),
        'assert reproducibility["identities"][0] == reproducibility["identities"][1], reproducibility': (
            "normalized image identities must match across isolated builds"
        ),
        'assert reproducibility["config_ids"] == [evidence["built_image_id"]] * 2, reproducibility': (
            "both image config digests must bind to the loaded image ID"
        ),
        'assert len(reproducibility["identity_sha256"]) == 2, reproducibility': (
            "oracle image evidence must contain exactly two identity hashes"
        ),
        'assert len(set(reproducibility["identity_sha256"])) == 1, reproducibility': (
            "complete image identities must match across isolated builds"
        ),
        'assert reproducibility["manifest_digests"] == [evidence["built_manifest_digest"]] * 2, reproducibility': (
            "both image manifest digests must bind to the built manifest"
        ),
        'assert reproducibility["descriptor_digests"] == reproducibility["manifest_digests"], reproducibility': (
            "image descriptors must bind the matching manifest digests"
        ),
        'assert len(reproducibility["descriptor_media_types"]) == 2, reproducibility': (
            "oracle image evidence must contain exactly two descriptor media types"
        ),
        'assert len(reproducibility["descriptor_sizes"]) == 2, reproducibility': (
            "oracle image evidence must contain exactly two descriptor sizes"
        ),
        'assert len(reproducibility["rootfs_diff_ids_sha256"]) == 2, reproducibility': (
            "oracle image evidence must contain exactly two DiffID hashes"
        ),
        'assert len(set(reproducibility["rootfs_diff_ids_sha256"])) == 1, reproducibility': (
            "root filesystem DiffIDs must match across isolated builds"
        ),
        'assert evidence["built_manifest_digest"] == reproducibility["manifest_digests"][0], evidence': (
            "build evidence must expose the matched manifest digest"
        ),
        'assert evidence["expected_image_id"] is None, evidence': (
            "unpinned image evidence must not claim a reviewed identity"
        ),
        'assert evidence["expected_manifest_digest"] is None, evidence': (
            "unpinned image evidence must not claim a reviewed manifest"
        ),
        'assert evidence["expected_manifest_digest"] == expected_manifest == evidence["built_manifest_digest"], evidence': (
            "pinned image evidence must match the reviewed manifest digest"
        ),
        "raise SystemExit(1)": "unpinned oracle image capture must fail closed",
        'assert evidence["image_identity_status"] == "pinned_match", evidence': (
            "pinned oracle image evidence must require pinned_match"
        ),
        'assert evidence["expected_image_id"] == expected == evidence["built_image_id"], evidence': (
            "pinned oracle image evidence must match expected and built identities"
        ),
    }.items():
        if snippet not in image_build:
            errors.append(f"{path}: {message}")
    image_commands = _normalized_active_commands(image_build)
    for command, message in {
        (
            'python3 scripts/run-render-oracle-container.py verify-lock "${BOOTSTRAP_ARGS[@]}"'
        ): "oracle image gate must verify the reproducible build contract",
        (
            "python3 scripts/run-render-oracle-container.py build --engine docker "
            "--image rxls-render-oracle:lo-26.2.3 --execute "
            '"${BOOTSTRAP_ARGS[@]}" > target/render-oracle-image-build.json'
        ): "oracle image gate must execute and record the reproducible OCI build",
    }.items():
        if command not in image_commands:
            errors.append(f"{path}: {message}")
    if image_build.count('"${BOOTSTRAP_ARGS[@]}"') != 2:
        errors.append(
            f"{path}: verify-lock and build must consume the same bootstrap argument array"
        )
    image_upload = _single_yaml_block(
        path,
        image_job,
        "- name: Upload oracle image identity evidence",
        6,
        "oracle-image evidence upload step",
        errors,
    )
    if (
        "if: always()" not in image_upload
        or (
            "name: render-oracle-image-"
            "${{ github.event.pull_request.head.sha || github.sha }}-"
            "${{ github.run_id }}-${{ github.run_attempt }}"
        )
        not in image_upload
        or "path: target/render-oracle-image-build.json" not in image_upload
        or "if-no-files-found: error" not in image_upload
    ):
        errors.append(f"{path}: oracle-image identity evidence must always upload")

    apt_lines = [line for line in active.splitlines() if "apt-get " in line]
    if (
        len(apt_lines) != 4
        or any('"${APT_OPTIONS[@]}"' not in line for line in apt_lines)
        or active.count("scripts/render-oracle-host-tools.py apt-sources") != 2
        or "libcairo2 poppler-utils" in active
    ):
        errors.append(
            f"{path}: PDF apt inputs must use only the isolated locked snapshot"
        )
    if "poppler-version.txt" in active or "command -v pdfinfo |" in active:
        errors.append(f"{path}: path-bearing Poppler evidence is forbidden")
    return errors


def audit_codeql_workflow(path: Path, text: str) -> list[str]:
    """Require explicit CodeQL builds for every shipped Rust surface."""

    errors: list[str] = []
    active = _without_commented_lines(text)
    push_trigger = _single_yaml_block(
        path, active, "push:", 2, "CodeQL push trigger", errors
    )
    if re.search(r"^ {4}paths(?:-ignore)?:", push_trigger, re.MULTILINE):
        errors.append(
            f"{path}: CodeQL main pushes must be unconditional because public "
            "single-root rewrites exceed bounded path-filter diff evaluation"
        )
    normalized = re.sub(r"[ \t]*\\\r?\n[ \t]*", " ", text)
    commands = (
        "cargo build --all-targets --all-features --locked",
        "cargo build --manifest-path render/Cargo.toml --all-targets --locked",
        "cargo build --manifest-path bindings/render-wasm/Cargo.toml --all-targets --locked",
        "cargo build --manifest-path bindings/mcp/Cargo.toml --all-targets --locked",
    )
    for command in commands:
        if normalized.count(command) != 1:
            errors.append(f"{path}: CodeQL must build exactly once with `{command}`")
    if "github/codeql-action/autobuild@" in text:
        errors.append(f"{path}: CodeQL autobuild cannot replace explicit nested builds")
    init_index = normalized.find("github/codeql-action/init@")
    analyze_index = normalized.find("github/codeql-action/analyze@")
    build_indices = [normalized.find(command) for command in commands]
    if (
        init_index < 0
        or analyze_index < 0
        or any(index < 0 for index in build_indices)
        or init_index >= min(build_indices)
        or max(build_indices) >= analyze_index
    ):
        errors.append(
            f"{path}: explicit Rust builds must run between CodeQL init and analysis"
        )
    return errors


def audit_ci_feature_matrix(path: Path, text: str) -> list[str]:
    """Keep otherwise-uncovered feature and wasm surfaces warning-clean."""

    normalized = re.sub(r"[ \t]*\\\r?\n[ \t]*", " ", _without_commented_lines(text))
    errors = [
        f"{path}: CI must run exactly once with `{command}`"
        for command in ADDITIONAL_FEATURE_CLIPPY_COMMANDS
        if normalized.count(command) != 1
    ]
    errors.extend(
        f"{path}: CI must run exactly once with `{command}`"
        for command in MCP_CI_COMMANDS
        if normalized.count(command) != 1
    )
    required_mcp_fragments = (
        "name: Local MCP server (MSRV 1.88)",
        "toolchain: 1.88.0",
        "manifest-path: ./bindings/mcp/Cargo.toml",
        "bindings/mcp/target/release/rxls-mcp --version",
        "scripts/generate-sbom.py --manifest-path Cargo.toml "
        "--manifest-path bindings/wasm/Cargo.toml "
        "--manifest-path bindings/mcp/Cargo.toml "
        "--output target/rxls-sbom.cdx.json",
        "bindings/mcp/Cargo.lock",
    )
    errors.extend(
        f"{path}: CI MCP gate is missing `{fragment}`"
        for fragment in required_mcp_fragments
        if fragment not in normalized
    )
    return errors


def audit_render_browser_workflow(path: Path, text: str) -> list[str]:
    """Require the browser lane to build wasm-bindgen with its exact Rust pin."""

    errors: list[str] = []
    active = _without_commented_lines(text)
    push_trigger = _single_yaml_block(
        path, active, "push:", 2, "render-browser push trigger", errors
    )
    if re.search(r"^ {4}paths(?:-ignore)?:", push_trigger, re.MULTILINE):
        errors.append(
            f"{path}: render-browser main pushes must be unconditional because "
            "public single-root rewrites exceed bounded path-filter diff evaluation"
        )
    baseline_trigger = '      - "scripts/render-parity-baseline-full.json"'
    if active.count(baseline_trigger) != 1:
        errors.append(
            f"{path}: the pull-request browser lane must track the reviewed render "
            "baseline; public main pushes are intentionally unconditional"
        )
    for name, value in {
        "WASM_BINDGEN_BUILD_RUST": RENDER_PACKAGE_WASM_BINDGEN_BUILD_RUST,
        "WASM_BINDGEN_VERSION": RENDER_PACKAGE_WASM_BINDGEN_VERSION,
    }.items():
        assignment = re.compile(
            rf"^\s*{re.escape(name)}:\s*[\"']?{re.escape(value)}[\"']?\s*$",
            re.MULTILINE,
        )
        if len(assignment.findall(active)) != 1:
            errors.append(f"{path}: expected exact {name}={value}")

    worker_job = _single_yaml_block(
        path, active, "worker-wasm:", 2, "worker-wasm job", errors
    )
    if re.findall(r"^\s{4}runs-on:\s*(\S+)\s*$", worker_job, re.MULTILINE) != [
        "ubuntu-24.04"
    ]:
        errors.append(f"{path}: browser worker must pin ubuntu-24.04")
    if worker_job.count("          persist-credentials: false") != 1:
        errors.append(
            f"{path}: browser checkout must disable persisted Git credentials"
        )
    metadata_step = _single_yaml_block(
        path,
        worker_job,
        "- name: Verify publishable package and pinned toolchain metadata",
        6,
        "browser toolchain metadata step",
        errors,
    )
    if (
        "l.wasmBindgen.buildRust !== process.env.WASM_BINDGEN_BUILD_RUST"
        not in metadata_step
    ):
        errors.append(
            f"{path}: browser metadata gate must bind wasm-bindgen to its build Rust pin"
        )
    install_step = _single_yaml_block(
        path,
        worker_job,
        "- name: Install exact wasm-bindgen CLI",
        6,
        "browser wasm-bindgen install step",
        errors,
    )
    build_step = _single_yaml_block(
        path,
        worker_job,
        "- name: Build exact wasm32 package",
        6,
        "browser wasm package build step",
        errors,
    )
    errors.extend(
        _audit_exact_wasm_bindgen_install(
            path,
            active,
            install_step,
            build_step,
            "npm run build:wasm",
            "browser wasm-bindgen install step",
        )
    )
    installed_browser_command = (
        "node --experimental-websocket \\\n"
        '            "$GITHUB_WORKSPACE/bindings/render-wasm/tests/browser/run.mjs"'
    )
    if worker_job.count(installed_browser_command) != 1:
        errors.append(
            f"{path}: installed Node 20 browser smoke must explicitly enable WebSocket"
        )
    chrome_step = _single_yaml_block(
        path,
        worker_job,
        "- name: Install exact Chrome for Testing",
        6,
        "exact Chrome installation step",
        errors,
    )
    for snippet, message in {
        "        shell: bash": "Chrome installation must use explicit Bash",
        "          set -euo pipefail": (
            "Chrome installation must fail closed across every pipeline"
        ),
        'sudo chown root:root "$chrome_root/chrome_sandbox"': (
            "Chrome sandbox must be owned by root"
        ),
        'sudo chmod 4755 "$chrome_root/chrome_sandbox"': (
            "Chrome sandbox must retain setuid mode 4755"
        ),
        'test "$(stat --format=%u "$chrome_root/chrome_sandbox")" = "0"': (
            "Chrome sandbox owner must be verified"
        ),
        'test "$(stat --format=%a "$chrome_root/chrome_sandbox")" = "4755"': (
            "Chrome sandbox mode must be verified"
        ),
        'ldd "$chrome_root/chrome" | tee "$RUNNER_TEMP/rxls-chromium-ldd.txt"': (
            "Chrome runtime dependencies must be inspected"
        ),
        'grep -Fq "not found" "$RUNNER_TEMP/rxls-chromium-ldd.txt"': (
            "Chrome runtime inspection must fail on unresolved libraries"
        ),
        (
            "printf '%s\\n' \"PASS pinned Chromium runtime closure resolved\" "
            "\\\n            > target/render-browser-evidence/chromium-runtime.txt"
        ): "Chrome runtime preflight must retain only path-neutral PASS evidence",
        (
            'echo "CHROME_DEVEL_SANDBOX=$GITHUB_WORKSPACE/target/render-chrome/'
            'chrome-linux64/chrome_sandbox" >> "$GITHUB_ENV"'
        ): "Chrome launch must export the exact verified sandbox",
    }.items():
        if snippet not in chrome_step:
            errors.append(f"{path}: {message}")
    for step_name, pipeline in (
        (
            "- name: Exercise worker under strict CSP in pinned Chromium",
            (
                "npm run test:browser 2>&1 | tee "
                "../../target/render-browser-evidence/chromium.log"
            ),
        ),
        (
            "- name: Pack and consume the publishable artifact",
            ("2>&1 | tee ../render-browser-evidence/installed-package-chromium.log"),
        ),
    ):
        step = _single_yaml_block(
            path,
            worker_job,
            step_name,
            6,
            f"{step_name[8:]} step",
            errors,
        )
        if "        shell: bash\n" not in step:
            errors.append(f"{path}: {step_name[8:]} must use explicit Bash")
        if "          set -euo pipefail\n" not in step:
            errors.append(
                f"{path}: {step_name[8:]} must fail closed across tee pipelines"
            )
        if pipeline not in step:
            errors.append(
                f"{path}: {step_name[8:]} must retain its authenticated pipeline"
            )
    pack_step = _single_yaml_block(
        path,
        worker_job,
        "- name: Pack and consume the publishable artifact",
        6,
        "publishable browser package step",
        errors,
    )
    pack_requirements = {
        'version="$(node -p \'require("./package.json").version\')"': (
            "browser package archive must derive its version from reviewed metadata"
        ),
        (
            'archive="$GITHUB_WORKSPACE/target/render-browser-evidence/'
            'rxls-render-worker-$version.tgz"'
        ): "browser package archive path must remain version-neutral",
        'python3 "$GITHUB_WORKSPACE/scripts/check_render_package.py" .': (
            "browser package must use the shared full artifact contract checker"
        ),
        '--archive "$archive"': (
            "browser package checker must inspect the exact consumed archive"
        ),
        "--npm-pack ../../target/render-browser-evidence/npm-pack.json": (
            "browser package checker must bind the npm pack receipt"
        ),
        '--git-rev "$(git rev-parse HEAD)"': (
            "browser package checker must bind the checked-out revision"
        ),
        (
            "--write-report ../../target/render-browser-evidence/"
            "package-report.json"
        ): "browser package checker must emit a reviewable report",
        '            "$archive"': (
            "installed browser smoke must consume the checked dynamic archive"
        ),
        'sha256sum "$archive"': (
            "browser package digest must cover the checked dynamic archive"
        ),
    }
    for snippet, message in pack_requirements.items():
        if snippet not in pack_step:
            errors.append(f"{path}: {message}")
    if re.search(r"rxls-render-worker-[0-9]+\.[0-9]+\.[0-9]+\.tgz", pack_step):
        errors.append(f"{path}: browser package step must not pin a stale package version")
    final_source_step = _single_yaml_block(
        path,
        worker_job,
        "- name: Verify evidence source remained exact and clean",
        6,
        "late browser source verifier",
        errors,
    )
    for snippet, message in {
        "        shell: bash": "late source verification must use explicit Bash",
        f"          EXPECTED_SHA: {PR_HEAD_EXPRESSION}": (
            "late source verification must bind the expected immutable HEAD"
        ),
        "          set -euo pipefail": (
            "late source verification must use strict Bash"
        ),
        'test "$(git rev-parse HEAD)" = "$EXPECTED_SHA"': (
            "late source verification must re-check exact HEAD"
        ),
        "          git diff --exit-code": (
            "late source verification must reject unstaged drift"
        ),
        "          git diff --cached --exit-code": (
            "late source verification must reject staged drift"
        ),
    }.items():
        if snippet not in final_source_step:
            errors.append(f"{path}: {message}")
    final_index = worker_job.find(
        "- name: Verify evidence source remained exact and clean"
    )
    summary_name = "- name: Build path-neutral exact-SHA browser evidence"
    summary_step = _single_yaml_block(
        path,
        worker_job,
        summary_name,
        6,
        "path-neutral exact-SHA browser evidence step",
        errors,
    )
    summary_top_keys = []
    for line in summary_step.splitlines()[1:]:
        if len(line) - len(line.lstrip(" ")) != 8:
            continue
        entry = _yaml_mapping_entry(_strip_yaml_inline_comment(line.lstrip(" ")))
        if entry is not None:
            summary_top_keys.append(entry[0])
    if summary_top_keys != ["shell", "env", "run"]:
        errors.append(
            f"{path}: browser evidence build must use only exact shell, env, and run fields"
        )
    summary_requirements = {
        "        shell: bash": ("browser evidence build must run under explicit Bash"),
        f"          EXPECTED_SHA: {PR_HEAD_EXPRESSION}": (
            "browser evidence build must bind the immutable source SHA"
        ),
        "          set -euo pipefail": ("browser evidence build must use strict Bash"),
        'test "$(git rev-parse HEAD)" = "$EXPECTED_SHA"': (
            "browser evidence build must reverify immutable HEAD"
        ),
        "python3 scripts/check_render_browser_release_evidence.py build": (
            "browser evidence must be built by the checked verifier"
        ),
        "--source-log target/render-browser-evidence/chromium.log": (
            "browser evidence must bind the source-package browser log"
        ),
        (
            "--installed-log target/render-browser-evidence/"
            "installed-package-chromium.log"
        ): "browser evidence must bind the installed-package browser log",
        "--runtime-evidence target/render-browser-evidence/chromium-runtime.txt": (
            "browser evidence must bind the pinned runtime preflight"
        ),
        "--npm-pack target/render-browser-evidence/npm-pack.json": (
            "browser evidence must bind exact npm pack metadata"
        ),
        (
            '--npm-archive "target/render-browser-evidence/'
            'rxls-render-worker-${version}.tgz"'
        ): "browser evidence must bind the packed archive bytes",
        '--head-sha "$EXPECTED_SHA"': (
            "browser evidence build and verify must bind exact HEAD"
        ),
        "--platform linux": (
            "browser evidence build and verify must bind the hosted Linux gate"
        ),
        '--repository "$GITHUB_REPOSITORY"': (
            "browser evidence build and verify must bind the repository"
        ),
        '--workflow-run-id "$GITHUB_RUN_ID"': (
            "browser evidence build and verify must bind the run ID"
        ),
        '--workflow-run-attempt "$GITHUB_RUN_ATTEMPT"': (
            "browser evidence build and verify must bind the run attempt"
        ),
        "--output target/render-browser-evidence/browser-summary.json": (
            "browser evidence must emit only the path-neutral summary"
        ),
        "python3 scripts/check_render_browser_release_evidence.py verify": (
            "browser evidence must be independently reverified before upload"
        ),
        "--summary target/render-browser-evidence/browser-summary.json": (
            "browser evidence verifier must consume the exact staged summary"
        ),
        "          git diff --exit-code": (
            "browser evidence build must reject late unstaged drift"
        ),
        "          git diff --cached --exit-code": (
            "browser evidence build must reject late staged drift"
        ),
    }
    for snippet, message in summary_requirements.items():
        if snippet not in summary_step:
            errors.append(f"{path}: {message}")
    for repeated in (
        '--head-sha "$EXPECTED_SHA"',
        '--repository "$GITHUB_REPOSITORY"',
        '--workflow-run-id "$GITHUB_RUN_ID"',
        '--workflow-run-attempt "$GITHUB_RUN_ATTEMPT"',
        "--platform linux",
    ):
        if summary_step.count(repeated) != 2:
            errors.append(
                f"{path}: browser evidence build and verify must each use {repeated}"
            )
    ordered_summary_commands = (
        'test "$(git rev-parse HEAD)" = "$EXPECTED_SHA"',
        "python3 scripts/check_render_browser_release_evidence.py build",
        "python3 scripts/check_render_browser_release_evidence.py verify",
        "git diff --exit-code",
        "git diff --cached --exit-code",
    )
    summary_positions = [
        summary_step.find(command) for command in ordered_summary_commands
    ]
    if any(index < 0 for index in summary_positions) or summary_positions != sorted(
        summary_positions
    ):
        errors.append(
            f"{path}: browser evidence must build, verify, and late-clean in exact order"
        )

    upload_name = "- name: Upload browser-rendering evidence"
    upload_step = _single_yaml_block(
        path,
        worker_job,
        upload_name,
        6,
        "browser summary upload step",
        errors,
    )
    upload_requirements = {
        "        if: ${{ success() }}": ("browser summary upload must be success-only"),
        f"        uses: {ORACLE_UPLOAD_ARTIFACT_ACTION}": (
            "browser summary upload must use the pinned artifact action"
        ),
        (
            "          name: render-browser-"
            f"{PR_HEAD_EXPRESSION}-"
            "${{ github.run_id }}-${{ github.run_attempt }}"
        ): "browser artifact name must bind SHA, run ID, and run attempt",
        "          path: target/render-browser-evidence/browser-summary.json": (
            "browser upload must contain only the path-neutral summary"
        ),
        "          if-no-files-found: error": (
            "browser summary upload must fail when evidence is absent"
        ),
        "          retention-days: 14": (
            "browser evidence retention must remain bounded"
        ),
        "          compression-level: 9": (
            "browser summary transfer must retain bounded compression"
        ),
    }
    for snippet, message in upload_requirements.items():
        if snippet not in upload_step:
            errors.append(f"{path}: {message}")
    summary_index = worker_job.find(summary_name)
    upload_index = worker_job.find(upload_name)
    next_after_source = (
        worker_job.find(
            "\n      - ",
            final_index
            + len("- name: Verify evidence source remained exact and clean"),
        )
        if final_index >= 0
        else -1
    )
    next_after_summary = (
        worker_job.find("\n      - ", summary_index + len(summary_name))
        if summary_index >= 0
        else -1
    )
    if final_index < 0 or summary_index < 0 or next_after_source + 7 != summary_index:
        errors.append(
            f"{path}: late exact-source verification must immediately precede summary build"
        )
    if summary_index < 0 or upload_index < 0 or next_after_summary + 7 != upload_index:
        errors.append(
            f"{path}: verified browser summary build must immediately precede upload"
        )
    return errors


def audit_render_package_release_workflow(path: Path, text: str) -> list[str]:
    """Require a verification-only dispatch and protected, exact-tag npm publish."""

    errors: list[str] = []
    _audit_exact_workflow_sha256(
        path,
        text,
        RENDER_PACKAGE_RELEASE_WORKFLOW_SHA256,
        errors,
    )
    text = _without_commented_lines(text)
    if re.search(r"^\s+continue-on-error\s*:", text, re.MULTILINE):
        errors.append(
            f"{path}: render package verification and publication must fail closed"
        )
    if re.search(
        r"^\s*set\s+\+e\s*$|\|\|\s*(?:true|:)(?:\s|$)",
        text,
        re.MULTILINE,
    ):
        errors.append(f"{path}: release shell commands must not disable fail-closed mode")
    trigger_names, trigger_errors = _workflow_trigger_names(text)
    errors.extend(f"{path}: {error}" for error in trigger_errors)
    if not trigger_errors and trigger_names != {"push", "workflow_dispatch"}:
        errors.append(
            f"{path}: render package release must have only push and "
            "workflow_dispatch triggers"
        )
    required = {
        'tags:\n      - "render-v*"': "must use the render-package-only tag namespace",
        "workflow_dispatch:": "must provide a verification-only manual dry run",
        'test "$GITHUB_REF_NAME" = "render-v$version"': (
            "must bind publication to the exact package version tag"
        ),
        'test "$GITHUB_REPOSITORY" = "HyunjoJung/rxls"': (
            "must reject publication from repository forks"
        ),
        'test "$(git rev-parse origin/main)" = "$GITHUB_SHA"': (
            "must require the tagged commit to equal the exact public main head"
        ),
        "require_successful_run ci.yml .github/workflows/ci.yml push CI": (
            "must require exact-SHA push CI"
        ),
        "require_successful_run codeql.yml .github/workflows/codeql.yml push CodeQL": (
            "must require exact-SHA push CodeQL"
        ),
        (
            "render-hardening.yml \\\n"
            "            .github/workflows/render-hardening.yml \\\n"
            "            workflow_dispatch"
        ): ("must require an exact-SHA dispatched render-hardening run"),
        ".github/workflows/render-browser.yml": (
            "must require the exact-SHA push render-browser path"
        ),
        (
            'browser_artifact_name="render-browser-${GITHUB_SHA}-'
            '${browser_run_id}-${browser_run_attempt}"'
        ): ("must select the SHA-, run-, and attempt-bound browser summary artifact"),
        "actions/runs/$browser_run_id/artifacts": (
            "must inspect artifacts from the selected exact browser run"
        ),
        'test "${#matching_browser_artifacts[@]}" = "1"': (
            "must require exactly one browser summary artifact"
        ),
        '"$browser_artifact_id" =~ ^[1-9][0-9]*$': (
            "must require a positive immutable browser artifact ID"
        ),
        '&& "$expired" == "false"': (
            "must reject expired hosted prerequisite artifacts"
        ),
        '&& "$size_bytes" -le 1048576': ("must bound the browser summary archive size"),
        "python3 scripts/check_render_browser_release_evidence.py download": (
            "must authenticate, download, extract, and verify browser evidence"
        ),
        '--artifact-id "$browser_artifact_id"': (
            "browser verifier must bind the exact artifact ID"
        ),
        '--artifact-name "$browser_artifact_name"': (
            "browser verifier must bind the exact artifact name"
        ),
        '--artifact-size-bytes "$size_bytes"': (
            "browser verifier must bind the hosted artifact size"
        ),
        '--artifact-digest "$digest"': (
            "browser verifier must bind the immutable hosted artifact digest"
        ),
        '--head-sha "$GITHUB_SHA"': (
            "browser verifier must bind the release candidate SHA"
        ),
        "--platform linux": (
            "browser verifier must bind the hosted Linux evidence platform"
        ),
        '--workflow-run-id "$browser_run_id"': (
            "browser verifier must bind the selected run ID"
        ),
        '--workflow-run-attempt "$browser_run_attempt"': (
            "browser verifier must bind the selected run attempt"
        ),
        "--output target/render-package/browser-prerequisite.json": (
            "must preserve the authenticated browser release receipt"
        ),
        "'[.head_sha, .event, .conclusion, .status, .path, .run_attempt] | @tsv'": (
            "must revalidate hosted run SHA, event, conclusion, status, path, "
            "and attempt"
        ),
        "for oracle_workflow in fuzz.yml render-oracle.yml; do": (
            "must inspect only the registered branch bridge and direct oracle workflows"
        ),
        '--workflow "$oracle_workflow"': (
            "must require a successful exact-SHA Render Oracle bridge run"
        ),
        '&& "$event" == "workflow_dispatch"': (
            "must accept full-oracle evidence only from deliberate dispatch"
        ),
        '( "$run_path" == ".github/workflows/fuzz.yml" \\': (
            "must validate the registered Render Oracle bridge path"
        ),
        '|| "$run_path" == ".github/workflows/render-oracle.yml" )': (
            "must validate the direct Render Oracle workflow path"
        ),
        '"$run_attempt" =~ ^[1-9][0-9]*$': (
            "must require a positive immutable hosted run attempt"
        ),
        'artifact_name="render-oracle-${GITHUB_SHA}-${run_id}-${run_attempt}-full-verify"': (
            "must select only the exact-SHA, run-bound full/verify artifact"
        ),
        "actions/runs/$run_id/artifacts": (
            "must inspect the selected run's artifact metadata"
        ),
        '"$digest" =~ ^sha256:[0-9a-f]{64}$': (
            "must require an immutable artifact digest"
        ),
        "scripts/check_render_oracle_release_evidence.py": (
            "must inspect full campaign and reviewed baseline-ratchet evidence"
        ),
        "--reviewed-baseline scripts/render-parity-baseline-full.json": (
            "must bind oracle ratchets to the checked reviewed baseline"
        ),
        "--baseline-mode verify": (
            "release selection must reject candidate-mode oracle evidence"
        ),
        "--campaign full": ("release selection must reject pilot oracle evidence"),
        'prerequisites.get("baseline_mode") != "verify"': (
            "release receipt must reverify baseline mode"
        ),
        'prerequisites.get("campaign") != "full"': (
            "release receipt must reverify campaign scope"
        ),
        "oracle-prerequisite.json": (
            "must preserve and reverify aggregate oracle prerequisite evidence"
        ),
        "python3 scripts/check_render_package.py": (
            "must enforce the bounded package/archive contract"
        ),
        "from scripts.check_render_package import REPORT_SCHEMA": (
            "must share the package report schema with the artifact checker"
        ),
        'report.get("schema") != REPORT_SCHEMA': (
            "must reverify the transported report against the shared schema"
        ),
        '--npm-pack "$output/npm-pack.json"': (
            "candidate validation must bind npm's exact pack receipt"
        ),
        "--npm-pack target/render-package/npm-pack.json": (
            "transported candidate validation must rebind the pack receipt"
        ),
        "Install checksum-verified cargo-deny": (
            "must install the reviewed dependency-policy binary"
        ),
        'echo "$CARGO_DENY_SHA256  $archive" | sha256sum --check --strict': (
            "must verify cargo-deny before execution"
        ),
        'test ! -L "$tool_root/cargo-deny"': (
            "must reject a symbolic-link cargo-deny executable"
        ),
        "cargo-deny --manifest-path bindings/render-wasm/Cargo.toml \\": (
            "cargo-deny must audit the locked nested render-WASM graph"
        ),
        "scripts/render_supply_chain.py notice": (
            "must verify the checked third-party notice against the locked closure"
        ),
        "--check bindings/render-wasm/THIRD_PARTY_NOTICES.txt": (
            "must validate the exact checked legal notice"
        ),
        '"$output/render-worker-sbom.cdx.json"': (
            "must generate nested CycloneDX evidence beside the package candidate"
        ),
        "cmp --silent \\": "must prove deterministic nested CycloneDX generation",
        "render-worker-sbom.cdx.json.sha256": (
            "must checksum and reverify nested CycloneDX evidence"
        ),
        "path: target/render-package/*": (
            "must upload the nested supply-chain evidence with the candidate"
        ),
        "python3 scripts/test_check_render_package.py": (
            "must run the focused immutable package tests"
        ),
        "python3 scripts/test_check_npm_registry_evidence.py": (
            "must run the focused npm provenance evidence tests"
        ),
        "python3 scripts/test_render_supply_chain.py": (
            "must run the focused nested supply-chain tests"
        ),
        "python3 scripts/test_check_render_oracle_release_evidence.py": (
            "must run the focused oracle-evidence tests"
        ),
        "python3 scripts/test_check_render_browser_release_evidence.py": (
            "must run the focused browser-evidence tests"
        ),
        "browser-proven package differs from release candidate": (
            "dry-run package must equal the browser-proven archive identity"
        ),
        "Render Browser prerequisite evidence differs": (
            "publication must reverify the authenticated browser receipt"
        ),
        "npm publish --dry-run --ignore-scripts --access public": (
            "must execute the registry publication dry run"
        ),
        "sha256sum --check": "must reverify the immutable candidate checksum",
        "actions/download-artifact@": (
            "must transfer the verified candidate rather than rebuild it for publication"
        ),
        "scripts/select_run_artifact.py": (
            "must select an attempt-bound candidate for failed-job retries"
        ),
        '--current-attempt "$GITHUB_RUN_ATTEMPT"': (
            "artifact selection must bind the current workflow attempt"
        ),
        'artifact-ids: ${{ steps.candidate.outputs.artifact_id }}': (
            "must download the selected immutable artifact ID"
        ),
        "digest-mismatch: error": "must fail closed on artifact transport drift",
        "if: github.event_name == 'push'": (
            "the publication job must not run for workflow_dispatch"
        ),
        "environment: npm-render-worker": (
            "registry mutation must pass through the protected deployment environment"
        ),
        "id-token: write": "npm publication must mint short-lived provenance identity",
        'registry-url: "https://registry.npmjs.org"': (
            "publication must target the public npm registry explicitly"
        ),
        "package-manager-cache: false": (
            "release jobs must not restore mutable package-manager caches"
        ),
        "npm publish \\": "must contain a real tag-only publication command",
        "id: registry": (
            "must expose the immutable registry preflight result to the publish step"
        ),
        "existing immutable registry version differs from the verified candidate": (
            "must fail closed when an immutable version already has different evidence"
        ),
        "if ! grep -Eq '(^|[[:space:]])E404([[:space:]]|$)' \"$error_log\"": (
            "registry absence must be distinguished from transient lookup failures"
        ),
        "if: steps.registry.outputs.already_published != 'true'": (
            "an identical verified registry version must make publication idempotent"
        ),
        "https://slsa.dev/provenance/v1": (
            "must require the npm SLSA provenance predicate"
        ),
        "version dist.integrity repository.url dist.attestations --json": (
            "must verify registry identity, integrity, and provenance"
        ),
        "npm audit signatures --json --include-attestations": (
            "must cryptographically verify registry signatures and provenance"
        ),
        'python3 "$GITHUB_WORKSPACE/scripts/check_npm_registry_evidence.py"': (
            "must bind the verified DSSE payload to the exact release identity"
        ),
        '--archive "$GITHUB_WORKSPACE/target/render-package/rxls-render-worker-$version.tgz"': (
            "npm evidence must hash the transferred candidate archive directly"
        ),
        "--workflow .github/workflows/render-package-release.yml": (
            "npm provenance must name the exact publishing workflow"
        ),
        '--git-sha "$GITHUB_SHA"': (
            "npm provenance must name the exact release commit"
        ),
        '--git-ref "$GITHUB_REF"': ("npm provenance must name the exact release tag"),
        '--run-id "$GITHUB_RUN_ID"': (
            "npm provenance must receive the current publishing run"
        ),
        '--run-attempt "$GITHUB_RUN_ATTEMPT"': (
            "npm provenance must receive the current publishing attempt"
        ),
        "ALREADY_PUBLISHED: ${{ steps.registry.outputs.already_published }}": (
            "npm provenance policy must consume the immutable preflight state"
        ),
        '--invocation-policy "$invocation_policy"': (
            "npm provenance must select an explicit invocation policy"
        ),
        'npm install --ignore-scripts "$spec"': (
            "must execute an exact registry-installed consumer"
        ),
    }
    for snippet, message in required.items():
        if snippet not in text:
            errors.append(f"{path}: {message}")

    exact_main = 'test "$(git rev-parse origin/main)" = "$GITHUB_SHA"'
    if text.count(exact_main) != 1:
        errors.append(
            f"{path}: exact origin/main must be checked once during candidate verification"
        )
    if text.count('git merge-base --is-ancestor "$GITHUB_SHA" origin/main') != 1:
        errors.append(
            f"{path}: publication retry must require the tag commit to remain in main"
        )
    if text.count('git fetch origin "refs/tags/$GITHUB_REF_NAME" --no-tags') != 1:
        errors.append(
            f"{path}: npm publication must refetch the exact hosted release tag"
        )
    if (
        text.count('test "$(git rev-parse \'FETCH_HEAD^{commit}\')" = "$GITHUB_SHA"')
        != 1
    ):
        errors.append(f"{path}: npm publication must revalidate the hosted tag commit")

    exact_assignments = {
        "CARGO_DENY_VERSION": "0.19.4",
        "CARGO_DENY_SHA256": (
            "3bd58b784e83715b86ddbc9deac591890372ec77fda5741bb0826970b958506f"
        ),
        "NODE_VERSION": RENDER_PACKAGE_NODE_VERSION,
        "NPM_VERSION": RENDER_PACKAGE_NPM_VERSION,
        "WASM_BINDGEN_BUILD_RUST": RENDER_PACKAGE_WASM_BINDGEN_BUILD_RUST,
        "WASM_BINDGEN_VERSION": RENDER_PACKAGE_WASM_BINDGEN_VERSION,
    }
    for name, value in exact_assignments.items():
        assignment = re.compile(
            rf"^\s*{re.escape(name)}:\s*[\"']?{re.escape(value)}[\"']?\s*$",
            re.MULTILINE,
        )
        if len(assignment.findall(text)) != 1:
            errors.append(f"{path}: expected exact {name}={value}")

    for forbidden in ("NODE_AUTH_TOKEN", "secrets.NPM_TOKEN", "_authToken"):
        if forbidden in text:
            errors.append(f"{path}: trusted npm publication must not expose {forbidden}")
    if text.count("if: github.event_name == 'push'") != 2:
        errors.append(
            f"{path}: only the hosted prerequisites and publish job may be tag-only"
        )
    if text.count("package-manager-cache: false") != 2 or re.search(
        r"package-manager-cache:\s*true", text
    ):
        errors.append(f"{path}: both release jobs must disable npm caching")
    if re.search(r"^\s*pull_request:\s*$", text, re.MULTILINE):
        errors.append(
            f"{path}: pull requests must never enter the registry release workflow"
        )
    if re.search(r"\bnpm\s+publish\b[^\n]*--force\b", text):
        errors.append(f"{path}: forced npm publication is forbidden")
    if len(re.findall(r"^\s*npm publish\b", text, re.MULTILINE)) != 2:
        errors.append(f"{path}: expected exactly one dry-run and one real npm publish")
    if text.count('npm view "$spec" \\') != 2:
        errors.append(
            f"{path}: registry preflight and postpublication verification must both run"
        )
    if (
        text.count("version dist.integrity repository.url dist.attestations --json")
        != 2
    ):
        errors.append(
            f"{path}: both registry lookups must request identity, integrity, and attestations"
        )
    if text.count("https://slsa.dev/provenance/v1") != 2:
        errors.append(
            f"{path}: both registry checks must require exact SLSA provenance"
        )
    if text.count("npm audit signatures --json --include-attestations") != 1:
        errors.append(
            f"{path}: registry signature and attestation audit must run exactly once"
        )
    if text.count("check_npm_registry_evidence.py") != 2:
        errors.append(
            f"{path}: npm provenance validator and its focused tests must each run once"
        )
    if text.count("python3 scripts/check_render_package.py") != 2:
        errors.append(
            f"{path}: candidate and transported render packages must both be validated"
        )
    if text.count('--current-attempt "$GITHUB_RUN_ATTEMPT"') != 1:
        errors.append(
            f"{path}: render candidate selection must bind exactly one current attempt"
        )
    if (
        text.count('--npm-pack "$output/npm-pack.json"') != 1
        or text.count("--npm-pack target/render-package/npm-pack.json") != 1
    ):
        errors.append(
            f"{path}: both render package checks must bind the exact npm pack receipt"
        )
    registry_verification_step = _single_yaml_block(
        path,
        text,
        "- name: Verify the registry distribution and installed consumer",
        6,
        "npm registry verification step",
        errors,
    )
    if (
        registry_verification_step.count(
            "ALREADY_PUBLISHED: ${{ steps.registry.outputs.already_published }}"
        )
        != 1
    ):
        errors.append(
            f"{path}: npm registry verification must bind exactly one invocation "
            "policy input to the immutable preflight state"
        )
    expected_invocation_policy = (
        '          invocation_policy="current-run"\n'
        '          if [[ "$ALREADY_PUBLISHED" == "true" ]]; then\n'
        '            invocation_policy="existing-release"\n'
        '          elif [[ "$ALREADY_PUBLISHED" != "false" ]]; then\n'
        '            echo "registry preflight did not produce a valid publication state" >&2\n'
        "            exit 1\n"
        "          fi"
    )
    if registry_verification_step.count(expected_invocation_policy) != 1:
        errors.append(
            f"{path}: newly published npm evidence must require the current run, "
            "while only a verified pre-existing release may use its original "
            "same-repository run"
        )
    if (
        registry_verification_step.count('--invocation-policy "$invocation_policy"')
        != 1
        or text.count('--invocation-policy "$invocation_policy"') != 1
    ):
        errors.append(
            f"{path}: npm provenance validator must receive exactly one selected "
            "invocation policy"
        )
    registry_preflight_index = text.find(
        "- name: Detect an identical immutable registry release"
    )
    real_publish_index = text.find("- name: Publish exact package with provenance")
    postpublish_index = text.find(
        "- name: Verify the registry distribution and installed consumer"
    )
    if not (0 <= registry_preflight_index < real_publish_index < postpublish_index):
        errors.append(
            f"{path}: immutable registry preflight must precede publication and verification"
        )
    if text.count("scripts/render_supply_chain.py sbom") != 3:
        errors.append(
            f"{path}: expected two deterministic SBOM generations and one exact validation"
        )
    if (
        text.count("browser-prerequisite.json") != 3
        or text.count("scripts/check_render_browser_release_evidence.py download") != 1
    ):
        errors.append(
            f"{path}: browser evidence must produce one receipt and reverify it twice"
        )
    hosted_gate_calls = re.findall(
        r"^\s*require_successful_run(?:\s|\\)", text, re.MULTILINE
    )
    if len(hosted_gate_calls) != 4:
        errors.append(
            f"{path}: expected exact-SHA CI, CodeQL, hardening, and browser gates"
        )
    deny_index = text.find("Install checksum-verified cargo-deny")
    build_index = text.find("npm --prefix bindings/render-wasm run build:wasm")
    if deny_index < 0 or build_index < 0 or deny_index > build_index:
        errors.append(f"{path}: nested dependency policy must run before building WASM")
    active = _without_commented_lines(text)
    verify_job = _single_yaml_block(
        path, active, "verify:", 2, "render package verify job", errors
    )
    deny_install_step = _single_yaml_block(
        path,
        verify_job,
        "- name: Install checksum-verified cargo-deny",
        6,
        "checksum-verified cargo-deny install step",
        errors,
    )
    deny_step = _single_yaml_block(
        path,
        verify_job,
        "- name: Audit nested Rust advisories, licenses, and sources",
        6,
        "nested Rust advisory and license audit step",
        errors,
    )
    if "sha256sum --check --strict" not in deny_install_step or (
        "cargo-deny-$CARGO_DENY_VERSION-x86_64-unknown-linux-musl.tar.gz"
        not in deny_install_step
    ):
        errors.append(f"{path}: cargo-deny install must use the reviewed binary and digest")
    if _normalized_active_commands(deny_step)[3:] != [
        "set -euo pipefail",
        'test "$(command -v cargo-deny)" = '
        '"$RUNNER_TEMP/cargo-deny-$CARGO_DENY_VERSION/cargo-deny"',
        'test "$(cargo-deny --version)" = "cargo-deny $CARGO_DENY_VERSION"',
        "cargo-deny --manifest-path bindings/render-wasm/Cargo.toml "
        "--locked --all-features check --config deny.toml",
    ]:
        errors.append(f"{path}: nested cargo-deny invocation drifted")
    if any(
        name == "if" for name, _ in _yaml_mapping_entries_at_indent(verify_job, 4)
    ):
        errors.append(
            f"{path}: verification must run for both tag pushes and manual dispatches"
        )
    verify_step_conditions = [
        value
        for name, value in _yaml_mapping_entries_at_indent(verify_job, 8)
        if name == "if"
    ]
    if verify_step_conditions != ["github.event_name == 'push'"]:
        errors.append(
            f"{path}: only the hosted prerequisite step may be conditional "
            "during verification"
        )
    publish_job = _single_yaml_block(
        path, active, "publish:", 2, "render package publish job", errors
    )
    publish_conditions = [
        value
        for name, value in _yaml_mapping_entries_at_indent(publish_job, 4)
        if name == "if"
    ]
    if publish_conditions != ["github.event_name == 'push'"]:
        errors.append(
            f"{path}: npm publication must have exactly one exact tag-push job guard"
        )
    reverify_step = _single_yaml_block(
        path,
        publish_job,
        "- name: Reverify the immutable candidate",
        6,
        "render package transported-candidate verification step",
        errors,
    )
    if (
        reverify_step.count("python3 scripts/check_render_package.py") != 1
        or reverify_step.count("--npm-pack target/render-package/npm-pack.json") != 1
        or reverify_step.count("--archive-only") != 1
    ):
        errors.append(
            f"{path}: transported render candidate must be checked once against "
            "its npm receipt without rebuilding"
        )
    prerequisite_step = _single_yaml_block(
        path,
        verify_job,
        "- name: Require exact-SHA hosted gates and reviewed full oracle evidence",
        6,
        "hosted prerequisite evidence step",
        errors,
    )
    prerequisite_top_keys = [
        name for name, _ in _yaml_mapping_entries_at_indent(prerequisite_step, 8)
    ]
    if prerequisite_top_keys != ["if", "env", "shell", "run"]:
        errors.append(
            f"{path}: hosted prerequisite step must use only exact if, env, shell, and run fields"
        )
    for snippet, message in {
        "        if: github.event_name == 'push'": (
            "hosted browser evidence must be required on tag publication"
        ),
        "          GH_TOKEN: ${{ github.token }}": (
            "artifact authentication must use the scoped workflow token"
        ),
        "        shell: bash": (
            "hosted prerequisite verification must use explicit Bash"
        ),
        "          set -euo pipefail": (
            "hosted prerequisite verification must use strict Bash"
        ),
        (
            'browser_artifact_name="render-browser-${GITHUB_SHA}-'
            '${browser_run_id}-${browser_run_attempt}"'
        ): "browser artifact lookup must bind exact SHA, run, and attempt",
        (
            '"repos/$GITHUB_REPOSITORY/actions/runs/$browser_run_id/artifacts"'
        ): "browser artifact lookup must remain scoped to the selected run",
        (
            "--jq '.artifacts[] | [.id, .name, .expired, "
            ".size_in_bytes, .digest] | @tsv'"
        ): "browser artifact lookup must retain all authenticated metadata",
        'test "${#matching_browser_artifacts[@]}" = "1"': (
            "browser artifact lookup must resolve exactly one summary"
        ),
        "python3 scripts/check_render_browser_release_evidence.py download": (
            "browser summary must be downloaded through the checked verifier"
        ),
        '--repository "$GITHUB_REPOSITORY"': (
            "browser download must bind the authenticated repository"
        ),
        '--artifact-id "$browser_artifact_id"': (
            "browser download must bind the artifact ID"
        ),
        '--artifact-name "$browser_artifact_name"': (
            "browser download must bind the exact artifact name"
        ),
        '--artifact-size-bytes "$size_bytes"': (
            "browser download must bind the hosted byte count"
        ),
        '--artifact-digest "$digest"': (
            "browser download must bind the immutable artifact digest"
        ),
        '--head-sha "$GITHUB_SHA"': ("browser download must bind the release SHA"),
        '--workflow-run-id "$browser_run_id"': (
            "browser download must bind the selected run"
        ),
        '--workflow-run-attempt "$browser_run_attempt"': (
            "browser download must bind the selected attempt"
        ),
        "--output target/render-package/browser-prerequisite.json": (
            "browser verifier receipt must be preserved with the candidate"
        ),
    }.items():
        if snippet not in prerequisite_step:
            errors.append(f"{path}: {message}")
    if (
        prerequisite_step.count(
            "python3 scripts/check_render_browser_release_evidence.py download"
        )
        != 1
    ):
        errors.append(
            f"{path}: browser prerequisite artifact must be downloaded and verified exactly once"
        )
    prerequisite_commands = _normalized_active_commands(prerequisite_step)
    exact_browser_artifact_guard = (
        '[[ "$browser_artifact_id" =~ ^[1-9][0-9]*$ '
        '&& "$expired" == "false" '
        '&& "$size_bytes" =~ ^[1-9][0-9]*$ '
        '&& "$size_bytes" -le 1048576 '
        '&& "$digest" =~ ^sha256:[0-9a-f]{64}$ ]]'
    )
    if prerequisite_commands.count(exact_browser_artifact_guard) != 1:
        errors.append(
            f"{path}: browser artifact metadata must pass the exact immutable bounded guard"
        )
    exact_browser_download = (
        "python3 scripts/check_render_browser_release_evidence.py download "
        '--repository "$GITHUB_REPOSITORY" '
        '--artifact-id "$browser_artifact_id" '
        '--artifact-name "$browser_artifact_name" '
        '--artifact-size-bytes "$size_bytes" '
        '--artifact-digest "$digest" '
        '--head-sha "$GITHUB_SHA" '
        "--platform linux "
        '--workflow-run-id "$browser_run_id" '
        '--workflow-run-attempt "$browser_run_attempt" '
        "--output target/render-package/browser-prerequisite.json"
    )
    if prerequisite_commands.count(exact_browser_download) != 1:
        errors.append(
            f"{path}: browser evidence download must retain every exact hosted binding"
        )
    prerequisite_order = (
        "render-browser.yml \\",
        'browser_run_id="$SELECTED_RUN_ID"',
        'browser_artifact_name="render-browser-',
        "actions/runs/$browser_run_id/artifacts",
        'test "${#matching_browser_artifacts[@]}" = "1"',
        "python3 scripts/check_render_browser_release_evidence.py download",
        "for oracle_workflow in fuzz.yml render-oracle.yml; do",
    )
    prerequisite_positions = [
        prerequisite_step.find(value) for value in prerequisite_order
    ]
    if any(
        index < 0 for index in prerequisite_positions
    ) or prerequisite_positions != sorted(prerequisite_positions):
        errors.append(
            f"{path}: browser run, artifact, verifier, and oracle gates must retain exact order"
        )
    policy_step = _single_yaml_block(
        path,
        verify_job,
        "- name: Enforce workflow and package policy",
        6,
        "render package policy and test step",
        errors,
    )
    policy_top_keys = [
        name for name, _ in _yaml_mapping_entries_at_indent(policy_step, 8)
    ]
    if policy_top_keys != ["run"]:
        errors.append(
            f"{path}: local workflow policy and tests must be an unconditional step"
        )
    expected_policy_commands = (
        'test "$(node --version)" = "v$NODE_VERSION"',
        'test "$(npm --version)" = "$NPM_VERSION"',
        "python3 scripts/check_workflow_policy.py",
        "python3 scripts/test_workflow_policy.py",
        "python3 scripts/test_check_render_browser_release_evidence.py",
        "python3 scripts/test_check_render_package.py",
        "python3 scripts/test_check_npm_registry_evidence.py",
        "python3 scripts/test_render_supply_chain.py",
        "python3 scripts/test_check_render_oracle_release_evidence.py",
        "python3 scripts/render_supply_chain.py notice --manifest-path "
        "bindings/render-wasm/Cargo.toml --check "
        "bindings/render-wasm/THIRD_PARTY_NOTICES.txt",
        "cargo fmt --manifest-path bindings/render-wasm/Cargo.toml -- --check",
        "cargo clippy --manifest-path bindings/render-wasm/Cargo.toml "
        "--all-targets --locked -- -D warnings",
        "cargo test --manifest-path bindings/render-wasm/Cargo.toml --locked",
        "npm --prefix bindings/render-wasm test",
    )
    policy_commands = _normalized_active_commands(policy_step)
    if policy_commands[2:] != list(expected_policy_commands):
        errors.append(
            f"{path}: local policy, focused tests, and package tests must be the "
            "exact reviewed command sequence"
        )
    pack_step = _single_yaml_block(
        path,
        verify_job,
        "- name: Pack, inspect, dry-run, and consume",
        6,
        "render package pack and dry-run step",
        errors,
    )
    pack_top_keys = [
        name for name, _ in _yaml_mapping_entries_at_indent(pack_step, 8)
    ]
    if pack_top_keys != ["shell", "run"]:
        errors.append(
            f"{path}: local pack, dry-run, and consumer verification must be one "
            "unconditional step"
        )
    push_guard = 'if [[ "$GITHUB_EVENT_NAME" == "push" ]]; then'
    dispatch_guard = 'test "$GITHUB_EVENT_NAME" = "workflow_dispatch"'
    if pack_step.count(push_guard) != 1 or pack_step.count(dispatch_guard) != 1:
        errors.append(
            f"{path}: package verification must contain exactly one tag-push "
            "browser guard and one fail-closed dispatch guard"
        )
    tag_branch_open = (
        '          if [[ "$GITHUB_EVENT_NAME" == "push" ]]; then\n'
        '            ARCHIVE="$archive" python3 - <<\'PY\''
    )
    tag_branch_close = (
        "          PY\n"
        "          else\n"
        '            test "$GITHUB_EVENT_NAME" = "workflow_dispatch"\n'
        "            echo \"workflow_dispatch verified the locally rebuilt package "
        "without publication prerequisites\"\n"
        "          fi\n"
    )
    if pack_step.count(tag_branch_open) != 1 or pack_step.count(tag_branch_close) != 1:
        errors.append(
            f"{path}: browser receipt must use one exact push branch with a "
            "fail-closed dispatch alternative"
        )
    tag_start = pack_step.find(tag_branch_open)
    tag_close_start = pack_step.find(tag_branch_close, max(tag_start, 0))
    tag_end = (
        tag_close_start + len(tag_branch_close) if tag_close_start >= 0 else -1
    )
    if tag_start < 0 or tag_close_start < tag_start:
        pack_prefix = ""
        tag_branch = ""
        pack_suffix = ""
    else:
        pack_prefix = pack_step[:tag_start]
        tag_branch = pack_step[tag_start:tag_end]
        pack_suffix = pack_step[tag_end:]
    if any(
        marker in section
        for section in (pack_prefix, pack_suffix)
        for marker in ("GITHUB_EVENT_NAME", "github.event_name")
    ):
        errors.append(
            f"{path}: event-selective logic in package verification must be "
            "confined to the authenticated browser-receipt branch"
        )

    prefix_order = (
        "python3 scripts/render_supply_chain.py sbom",
        "cmp --silent \\",
        '--check "$output/render-worker-sbom.cdx.json"',
        'sha256sum "$output/render-worker-sbom.cdx.json"',
        "npm pack --json --pack-destination",
        "python3 scripts/check_render_package.py",
    )
    prefix_positions = [pack_prefix.find(value) for value in prefix_order]
    if (
        any(index < 0 for index in prefix_positions)
        or prefix_positions != sorted(prefix_positions)
        or pack_step.count("python3 scripts/render_supply_chain.py sbom") != 3
        or pack_prefix.count("python3 scripts/render_supply_chain.py sbom") != 3
        or pack_prefix.count("python3 scripts/check_render_package.py") != 1
        or pack_prefix.count('--npm-pack "$output/npm-pack.json"') != 1
        or pack_prefix.count("npm pack --json --pack-destination") != 1
    ):
        errors.append(
            f"{path}: dispatch must run deterministic SBOM, package, and archive "
            "validation before the tag-only browser binding"
        )
    browser_receipt = 'Path("target/render-package/browser-prerequisite.json")'
    browser_mismatch = "browser-proven package differs from release candidate"
    if (
        pack_step.count(browser_receipt) != 1
        or pack_step.count(browser_mismatch) != 1
        or browser_receipt not in tag_branch
        or browser_mismatch not in tag_branch
        or "oracle-prerequisite.json" in pack_step
    ):
        errors.append(
            f"{path}: only tag pushes may read and bind hosted browser prerequisites "
            "during local package verification"
        )
    suffix_order = (
        "npm publish --dry-run --ignore-scripts --access public",
        'sha256sum "$archive" > "$archive.sha256"',
        'consumer="$RUNNER_TEMP/render-worker-consumer"',
        'rm -rf "$consumer"',
        'mkdir -p "$consumer"',
        'cd "$consumer"',
        "npm init --yes >/dev/null",
        'npm install --ignore-scripts "$GITHUB_WORKSPACE/$archive"',
        "node --input-type=module - <<'NODE'",
    )
    suffix_positions = [pack_suffix.find(value) for value in suffix_order]
    if (
        any(index < 0 for index in suffix_positions)
        or suffix_positions != sorted(suffix_positions)
        or any(pack_step.count(value) != 1 for value in suffix_order)
    ):
        errors.append(
            f"{path}: dry-run, checksum, and clean installed consumer must run "
            "after the event-specific browser branch"
        )
    build_step = _single_yaml_block(
        path,
        verify_job,
        "- name: Build the exact worker/WASM package",
        6,
        "render package wasm-bindgen build step",
        errors,
    )
    build_top_keys = [
        name for name, _ in _yaml_mapping_entries_at_indent(build_step, 8)
    ]
    if build_top_keys != ["shell", "run"]:
        errors.append(f"{path}: exact worker/WASM build must be unconditional")
    if "GITHUB_EVENT_NAME" in build_step or "github.event_name" in build_step:
        errors.append(
            f"{path}: exact worker/WASM build must not use event-selective shell guards"
        )
    verify_order = (
        "- name: Install checksum-verified cargo-deny",
        "- name: Audit nested Rust advisories, licenses, and sources",
        "- name: Validate event and package identity",
        "- name: Require exact-SHA hosted gates and reviewed full oracle evidence",
        "- name: Enforce workflow and package policy",
        "- name: Build the exact worker/WASM package",
        "- name: Pack, inspect, dry-run, and consume",
        "- name: Upload verified package candidate",
    )
    verify_positions = [verify_job.find(value) for value in verify_order]
    if any(index < 0 for index in verify_positions) or verify_positions != sorted(
        verify_positions
    ):
        errors.append(
            f"{path}: identity, tag-only prerequisites, local gates, build, pack, "
            "and upload must retain exact order"
        )
    errors.extend(
        _audit_exact_wasm_bindgen_install(
            path,
            active,
            build_step,
            build_step,
            "npm --prefix bindings/render-wasm run build:wasm",
            "render package wasm-bindgen build step",
        )
    )
    return errors


def audit_wasm_package_release_workflow(path: Path, text: str) -> list[str]:
    """Require a reproducible dispatch and protected exact-tag rxls-wasm publish."""

    errors: list[str] = []
    _audit_exact_workflow_sha256(
        path,
        text,
        WASM_PACKAGE_RELEASE_WORKFLOW_SHA256,
        errors,
    )
    active = _without_commented_lines(text)
    if re.search(r"^\s+continue-on-error\s*:", active, re.MULTILINE):
        errors.append(f"{path}: WASM package verification and publication must fail closed")
    if re.search(
        r"^\s*set\s+\+e\s*$|\|\|\s*(?:true|:)(?:\s|$)",
        active,
        re.MULTILINE,
    ):
        errors.append(f"{path}: release shell commands must not disable fail-closed mode")
    trigger_names, trigger_errors = _workflow_trigger_names(active)
    errors.extend(f"{path}: {error}" for error in trigger_errors)
    if not trigger_errors and trigger_names != {"push", "workflow_dispatch"}:
        errors.append(
            f"{path}: WASM package release must have only push and "
            "workflow_dispatch triggers"
        )
    if re.search(r"^\s*pull_request:\s*$", active, re.MULTILINE):
        errors.append(f"{path}: pull requests must never enter npm publication")

    required = {
        'tags:\n      - "wasm-v*"': "must use the core-WASM-only tag namespace",
        'test "$GITHUB_EVENT_NAME" = "workflow_dispatch"': (
            "manual verification must reject every unrecognized event"
        ),
        'test "$GITHUB_REPOSITORY" = "HyunjoJung/rxls"': (
            "must reject publication from repository forks"
        ),
        'test "$GITHUB_REF_NAME" = "wasm-v$version"': (
            "must bind publication to the exact package version tag"
        ),
        'test "$(node -p \'require("./bindings/wasm/npm/package.json").name\')" = "rxls-wasm"': (
            "must bind both release jobs to the exact npm package name"
        ),
        'test "$(git rev-parse origin/main)" = "$GITHUB_SHA"': (
            "must require the candidate to equal the public main head"
        ),
        "require_successful_run ci.yml .github/workflows/ci.yml CI": (
            "must require exact-SHA push CI"
        ),
        "require_successful_run codeql.yml .github/workflows/codeql.yml CodeQL": (
            "must require exact-SHA push CodeQL"
        ),
        '&& "$event" == "push" \\': (
            "hosted gates must accept only successful push runs"
        ),
        "Install checksum-verified cargo-deny": (
            "must install the reviewed dependency-policy binary"
        ),
        'echo "$CARGO_DENY_SHA256  $archive" | sha256sum --check --strict': (
            "must verify cargo-deny before execution"
        ),
        'test ! -L "$tool_root/cargo-deny"': (
            "must reject a symbolic-link cargo-deny executable"
        ),
        "cargo-deny --manifest-path bindings/wasm/Cargo.toml \\": (
            "nested dependency policy must audit the locked core WASM graph"
        ),
        "python3 scripts/test_workflow_policy.py": (
            "must execute workflow policy mutation tests"
        ),
        "python3 scripts/render_supply_chain.py notice": (
            "must regenerate and verify the complete legal notice"
        ),
        "--check bindings/wasm/THIRD_PARTY_NOTICES.txt": (
            "must bind the packaged legal notice to the locked closure"
        ),
        'bash scripts/build-wasm-package.sh "$package"': (
            "must build the exact staged npm package"
        ),
        "node bindings/wasm/tests/node-smoke.cjs": (
            "must exercise clean Node and TypeScript consumers"
        ),
        "node bindings/wasm/tests/browser-smoke.mjs": (
            "must exercise the real Chromium consumer"
        ),
        "npm pack --json --pack-destination": (
            "must preserve npm's exact archive metadata"
        ),
        "python3 scripts/check_wasm_package.py": (
            "must validate package contents, identity, and byte budgets"
        ),
        '--npm-pack "$output/npm-pack.json"': (
            "candidate and transported archive checks must bind npm pack metadata"
        ),
        "python3 scripts/render_supply_chain.py sbom": (
            "must produce the locked core-WASM CycloneDX graph"
        ),
        "cmp --silent \\": "must prove deterministic CycloneDX generation",
        "npm publish --dry-run --ignore-scripts --access public": (
            "must execute a registry publication dry run"
        ),
        "Verify evidence source remained exact and clean": (
            "must recheck the source after candidate construction"
        ),
        "actions/download-artifact@": (
            "must transfer the verified candidate instead of rebuilding it"
        ),
        "scripts/select_run_artifact.py": (
            "must select an attempt-bound candidate for failed-job retries"
        ),
        '--current-attempt "$GITHUB_RUN_ATTEMPT"': (
            "artifact selection must bind the current workflow attempt"
        ),
        'artifact-ids: ${{ steps.candidate.outputs.artifact_id }}': (
            "must download the selected immutable artifact ID"
        ),
        "digest-mismatch: error": "must fail closed on artifact transport drift",
        "environment: npm-rxls-wasm": (
            "registry mutation must use the protected deployment environment"
        ),
        "id-token: write": "npm publication must mint short-lived OIDC provenance",
        'registry-url: "https://registry.npmjs.org"': (
            "publication must explicitly target the public npm registry"
        ),
        'git fetch origin "refs/tags/$GITHUB_REF_NAME" --no-tags': (
            "publication must refetch the exact hosted release tag"
        ),
        'test "$(git rev-parse \'FETCH_HEAD^{commit}\')" = "$GITHUB_SHA"': (
            "publication must bind the hosted tag to the candidate commit"
        ),
        "existing immutable registry version differs from the verified candidate": (
            "must reject a registry version with different bytes or provenance"
        ),
        "if ! grep -Eq '(^|[[:space:]])E404([[:space:]]|$)' \"$error_log\"": (
            "registry absence must be distinguished from lookup failures"
        ),
        "if: steps.registry.outputs.already_published != 'true'": (
            "identical immutable releases must make retries idempotent"
        ),
        "https://slsa.dev/provenance/v1": (
            "registry checks must require the npm SLSA provenance predicate"
        ),
        "npm audit signatures --json --include-attestations": (
            "must verify registry signatures and attestations"
        ),
        'python3 "$GITHUB_WORKSPACE/scripts/check_npm_registry_evidence.py"': (
            "must bind DSSE evidence to the exact release identity"
        ),
        '--archive "$GITHUB_WORKSPACE/$output/rxls-wasm-$version.tgz"': (
            "npm evidence must hash the transferred candidate archive directly"
        ),
        "--workflow .github/workflows/wasm-package-release.yml": (
            "npm provenance must identify this publishing workflow"
        ),
        '--git-sha "$GITHUB_SHA"': "npm provenance must identify the release commit",
        '--git-ref "$GITHUB_REF"': "npm provenance must identify the release tag",
        '--run-id "$GITHUB_RUN_ID"': "npm provenance must identify the publishing run",
        '--run-attempt "$GITHUB_RUN_ATTEMPT"': (
            "npm provenance must identify the publishing attempt"
        ),
        '--invocation-policy "$invocation_policy"': (
            "npm provenance must use an explicit retry policy"
        ),
        'npm install --ignore-scripts "$spec"': (
            "must execute a clean registry-installed consumer"
        ),
    }
    for snippet, message in required.items():
        if snippet not in active:
            errors.append(f"{path}: {message}")

    exact_assignments = {
        "CARGO_DENY_VERSION": "0.19.4",
        "CARGO_DENY_SHA256": (
            "3bd58b784e83715b86ddbc9deac591890372ec77fda5741bb0826970b958506f"
        ),
        "WASM_MSRV": "1.85.0",
        "WASM_BINDGEN_BUILD_RUST": RENDER_PACKAGE_WASM_BINDGEN_BUILD_RUST,
        "WASM_BINDGEN_VERSION": RENDER_PACKAGE_WASM_BINDGEN_VERSION,
        "NODE_VERSION": RENDER_PACKAGE_NODE_VERSION,
        "NPM_VERSION": RENDER_PACKAGE_NPM_VERSION,
        "PLAYWRIGHT_VERSION": "1.54.1",
    }
    for name, value in exact_assignments.items():
        assignment = re.compile(
            rf"^\s*{re.escape(name)}:\s*[\"']?{re.escape(value)}[\"']?\s*$",
            re.MULTILINE,
        )
        if len(assignment.findall(active)) != 1:
            errors.append(f"{path}: expected exact {name}={value}")

    if active.count('test "$(git rev-parse --is-shallow-repository)" = "false"') != 2:
        errors.append(f"{path}: both release jobs must reject shallow source history")
    if active.count('test "$GITHUB_REPOSITORY" = "HyunjoJung/rxls"') != 2:
        errors.append(f"{path}: both release jobs must reject repository forks")
    package_name_check = (
        'test "$(node -p \'require("./bindings/wasm/npm/package.json").name\')" '
        '= "rxls-wasm"'
    )
    if active.count(package_name_check) != 2:
        errors.append(f"{path}: both release jobs must bind the rxls-wasm package name")
    if active.count('&& "$event" == "push" \\') != 1:
        errors.append(f"{path}: hosted gates must require exactly one push-event check")
    exact_main = 'test "$(git rev-parse origin/main)" = "$GITHUB_SHA"'
    if active.count(exact_main) != 1:
        errors.append(
            f"{path}: exact origin/main must be checked once during verification"
        )
    if active.count('git merge-base --is-ancestor "$GITHUB_SHA" origin/main') != 1:
        errors.append(
            f"{path}: publication retry must require the tag commit to remain in main"
        )
    if active.count('test "$(git rev-parse HEAD)" = "$GITHUB_SHA"') != 3:
        errors.append(
            f"{path}: candidate creation, late source audit, and publication must "
            "bind checkout HEAD to GITHUB_SHA"
        )
    if active.count('git fetch origin "refs/tags/$GITHUB_REF_NAME" --no-tags') != 1:
        errors.append(f"{path}: publication must refetch exactly one hosted tag")
    if active.count("package-manager-cache: false") != 2:
        errors.append(f"{path}: both release jobs must disable mutable npm caching")
    for forbidden in ("NODE_AUTH_TOKEN", "secrets.NPM_TOKEN", "_authToken"):
        if forbidden in active:
            errors.append(f"{path}: trusted npm publication must not expose {forbidden}")
    if active.count("if: github.event_name == 'push'") != 2:
        errors.append(f"{path}: only hosted gates and the publish job may be tag-only")
    if any(
        command.startswith("npm publish") and "--force" in command
        for command in _normalized_active_commands(active)
    ):
        errors.append(f"{path}: forced npm publication is forbidden")
    if len(re.findall(r"^\s*npm publish\b", active, re.MULTILINE)) != 2:
        errors.append(f"{path}: expected exactly one dry-run and one real npm publish")
    if active.count('npm view "$spec" \\') != 2:
        errors.append(f"{path}: registry preflight and postpublication checks must both run")
    if active.count("version dist.integrity repository.url dist.attestations --json") != 2:
        errors.append(f"{path}: registry checks must bind identity, integrity, and attestations")
    if active.count("https://slsa.dev/provenance/v1") != 2:
        errors.append(f"{path}: both registry checks must require exact SLSA provenance")
    if active.count("python3 scripts/render_supply_chain.py sbom") != 3:
        errors.append(f"{path}: SBOM must be generated twice and checked once")
    if active.count("python3 scripts/check_wasm_package.py") != 2:
        errors.append(f"{path}: candidate and transported package must both be validated")
    if active.count('--npm-pack "$output/npm-pack.json"') != 2:
        errors.append(f"{path}: both package checks must bind the npm pack receipt")
    if active.count('--current-attempt "$GITHUB_RUN_ATTEMPT"') != 1:
        errors.append(
            f"{path}: WASM candidate selection must bind exactly one current attempt"
        )

    verify_job = _single_yaml_block(
        path, active, "verify:", 2, "WASM package verify job", errors
    )
    if any(
        name == "if" for name, _ in _yaml_mapping_entries_at_indent(verify_job, 4)
    ):
        errors.append(f"{path}: verification must run for dispatches and tag pushes")
    verify_step_conditions = [
        value
        for name, value in _yaml_mapping_entries_at_indent(verify_job, 8)
        if name == "if"
    ]
    if verify_step_conditions != ["github.event_name == 'push'"]:
        errors.append(f"{path}: only exact-SHA hosted gates may be conditional in verify")

    publish_job = _single_yaml_block(
        path, active, "publish:", 2, "WASM package publish job", errors
    )
    publish_conditions = [
        value
        for name, value in _yaml_mapping_entries_at_indent(publish_job, 4)
        if name == "if"
    ]
    if publish_conditions != ["github.event_name == 'push'"]:
        errors.append(f"{path}: npm publication must have one exact tag-push job guard")
    for forbidden in (
        "build-wasm-package.sh",
        "cargo build",
        "install wasm-bindgen-cli",
        "npm pack ",
    ):
        if forbidden in publish_job:
            errors.append(f"{path}: publish job must not rebuild the verified candidate")

    deny_install_step = _single_yaml_block(
        path,
        verify_job,
        "- name: Install checksum-verified cargo-deny",
        6,
        "core WASM cargo-deny install step",
        errors,
    )
    deny_step = _single_yaml_block(
        path,
        verify_job,
        "- name: Audit nested Rust advisories, licenses, and sources",
        6,
        "core WASM dependency policy step",
        errors,
    )
    if "sha256sum --check --strict" not in deny_install_step or (
        "cargo-deny-$CARGO_DENY_VERSION-x86_64-unknown-linux-musl.tar.gz"
        not in deny_install_step
    ):
        errors.append(f"{path}: cargo-deny install must use the reviewed binary and digest")
    if _normalized_active_commands(deny_step)[3:] != [
        "set -euo pipefail",
        'test "$(command -v cargo-deny)" = '
        '"$RUNNER_TEMP/cargo-deny-$CARGO_DENY_VERSION/cargo-deny"',
        'test "$(cargo-deny --version)" = "cargo-deny $CARGO_DENY_VERSION"',
        "cargo-deny --manifest-path bindings/wasm/Cargo.toml "
        "--locked --all-features check --config deny.toml",
    ]:
        errors.append(f"{path}: nested cargo-deny invocation drifted")

    policy_step = _single_yaml_block(
        path,
        verify_job,
        "- name: Enforce source and package policy",
        6,
        "WASM package policy step",
        errors,
    )
    expected_policy_commands = [
        "set -euo pipefail",
        'test "$(node --version)" = "v$NODE_VERSION"',
        'test "$(npm --version)" = "$NPM_VERSION"',
        "python3 scripts/check_release_identity.py",
        "python3 scripts/check_workflow_policy.py",
        "python3 scripts/test_workflow_policy.py",
        "python3 -m unittest scripts.test_release_tools "
        "scripts.test_check_npm_registry_evidence",
        "python3 scripts/render_supply_chain.py notice --profile core-wasm "
        "--manifest-path bindings/wasm/Cargo.toml "
        "--check bindings/wasm/THIRD_PARTY_NOTICES.txt",
        "cargo fmt --manifest-path bindings/wasm/Cargo.toml -- --check",
        "cargo clippy --manifest-path bindings/wasm/Cargo.toml --all-targets "
        "--target wasm32-unknown-unknown --locked -- -D warnings",
        "cargo test --manifest-path bindings/wasm/Cargo.toml --locked",
    ]
    policy_commands = _normalized_active_commands(policy_step)
    if policy_commands[:3] != [
        "- name: Enforce source and package policy",
        "shell: bash",
        "run: |",
    ] or policy_commands[3:] != expected_policy_commands:
        errors.append(f"{path}: local policy gates must retain the reviewed exact sequence")

    install_step = _single_yaml_block(
        path,
        verify_job,
        "- name: Install exact wasm-bindgen CLI",
        6,
        "WASM package wasm-bindgen install step",
        errors,
    )
    build_step = _single_yaml_block(
        path,
        verify_job,
        "- name: Build and exercise exact package",
        6,
        "WASM package build and consumer step",
        errors,
    )
    errors.extend(
        _audit_exact_wasm_bindgen_install(
            path,
            active,
            install_step,
            build_step,
            'bash scripts/build-wasm-package.sh "$package"',
            "WASM package wasm-bindgen install",
        )
    )

    pack_step = _single_yaml_block(
        path,
        verify_job,
        "- name: Pack, inspect, dry-run, and attest candidate",
        6,
        "WASM package archive step",
        errors,
    )
    pack_order = (
        "npm pack --json --pack-destination",
        "python3 scripts/check_wasm_package.py",
        "python3 scripts/render_supply_chain.py sbom",
        "cmp --silent \\",
        '--check "$output/rxls-wasm-sbom.cdx.json"',
        'sha256sum "$archive"',
        "npm publish --dry-run --ignore-scripts --access public",
    )
    pack_positions = [pack_step.find(value) for value in pack_order]
    if any(index < 0 for index in pack_positions) or pack_positions != sorted(
        pack_positions
    ):
        errors.append(f"{path}: pack, byte audit, SBOM, checksum, and dry-run order drifted")
    if "GITHUB_EVENT_NAME" in pack_step or "github.event_name" in pack_step:
        errors.append(f"{path}: package validation must be identical for dispatch and tag")

    verify_order = (
        "- name: Validate event and package identity",
        "- name: Require successful exact-SHA CI and CodeQL",
        "- name: Install checksum-verified cargo-deny",
        "- name: Audit nested Rust advisories, licenses, and sources",
        "- name: Enforce source and package policy",
        "- name: Install exact wasm-bindgen CLI",
        "- name: Build and exercise exact package",
        "- name: Pack, inspect, dry-run, and attest candidate",
        "- name: Verify evidence source remained exact and clean",
        "- name: Upload verified package candidate",
    )
    verify_positions = [verify_job.find(value) for value in verify_order]
    if any(index < 0 for index in verify_positions) or verify_positions != sorted(
        verify_positions
    ):
        errors.append(f"{path}: verification stages must retain the reviewed exact order")

    publish_order = (
        "- name: Select immutable verified candidate",
        "- name: Reverify immutable candidate and hosted tag",
        "- name: Detect an identical immutable registry release",
        "- name: Publish exact package with provenance",
        "- name: Verify registry provenance and installed consumer",
        "- name: Upload registry evidence",
    )
    publish_positions = [publish_job.find(value) for value in publish_order]
    if any(index < 0 for index in publish_positions) or publish_positions != sorted(
        publish_positions
    ):
        errors.append(f"{path}: revalidation, preflight, publish, and audit order drifted")
    return errors


def audit_repository(root: Path) -> list[str]:
    workflow_root = root / ".github" / "workflows"
    workflows = sorted((*workflow_root.glob("*.yml"), *workflow_root.glob("*.yaml")))
    if not workflows:
        return [f"{workflow_root}: no workflows found"]

    errors: list[str] = []
    for path in workflows:
        text = path.read_text(encoding="utf-8")
        relative = path.relative_to(root)
        errors.extend(audit_action_pins(relative, text))
        errors.extend(audit_pr_head_checkouts(relative, text))
        if path.name == "fuzz.yml":
            errors.extend(audit_fuzz_workflow(relative, text))
        elif path.name != "release.yml":
            errors.extend(audit_tool_commands(relative, text))
        if path.name in {"ci.yml", "release.yml"}:
            errors.extend(audit_semver_gate(relative, text))
        if path.name == "ci.yml":
            errors.extend(audit_ci_feature_matrix(relative, text))
        if path.name == "render-oracle.yml":
            errors.extend(audit_render_oracle_workflow(relative, text))
        elif path.name == "render-hardening.yml":
            errors.extend(audit_render_hardening_workflow(relative, text))
        elif path.name == "render-browser.yml":
            errors.extend(audit_render_browser_workflow(relative, text))
        elif path.name == "render-package-release.yml":
            errors.extend(audit_render_package_release_workflow(relative, text))
        elif path.name == "wasm-package-release.yml":
            errors.extend(audit_wasm_package_release_workflow(relative, text))
        elif path.name == "codeql.yml":
            errors.extend(audit_codeql_workflow(relative, text))

    release = workflow_root / "release.yml"
    if not release.is_file():
        errors.append(f"{release.relative_to(root)}: missing release workflow")
    else:
        release_text = release.read_text(encoding="utf-8")
        errors.extend(audit_release_versions(release.relative_to(root), release_text))
        errors.extend(
            audit_core_release_evidence(release.relative_to(root), release_text)
        )
    github_release_reconciler = root / "scripts" / "reconcile_github_release.py"
    if not github_release_reconciler.is_file():
        errors.append(
            f"{github_release_reconciler.relative_to(root)}: missing GitHub Release reconciler"
        )
    else:
        errors.extend(
            audit_github_release_reconciler(
                github_release_reconciler.relative_to(root),
                github_release_reconciler.read_text(encoding="utf-8"),
            )
        )
    render_package_release = workflow_root / "render-package-release.yml"
    if not render_package_release.is_file():
        errors.append(
            f"{render_package_release.relative_to(root)}: missing render package release workflow"
        )
    wasm_package_release = workflow_root / "wasm-package-release.yml"
    if not wasm_package_release.is_file():
        errors.append(
            f"{wasm_package_release.relative_to(root)}: missing WASM package release workflow"
        )
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root", type=Path, default=Path(__file__).resolve().parents[1]
    )
    args = parser.parse_args()

    errors = audit_repository(args.root.resolve())
    if errors:
        for error in errors:
            print(error, file=sys.stderr)
        return 1
    print("workflow policy passed: immutable action SHAs and exact release tools")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
