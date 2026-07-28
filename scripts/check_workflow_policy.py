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
RENDER_ORACLE_PYTHON_VERSION = "3.13.14"
RENDER_ORACLE_FULL_CASES = "800"
RENDER_ORACLE_FULL_REPEATS = "2"
RENDER_ORACLE_FULL_SHARDS = "4"
RENDER_ORACLE_MAX_PARALLEL_SHARDS = "2"
RENDER_PACKAGE_NODE_VERSION = "24.18.0"
RENDER_PACKAGE_NPM_VERSION = "11.16.0"
RENDER_PACKAGE_WASM_BINDGEN_BUILD_RUST = "1.88.0"
RENDER_PACKAGE_WASM_BINDGEN_VERSION = "0.2.126"
ORACLE_BUILDX_VERSION = "v0.35.0"
ORACLE_BUILDX_ACTION = (
    "docker/setup-buildx-action@bb05f3f5519dd87d3ba754cc423b652a5edd6d2c"
)
ORACLE_CHECKOUT_ACTION = (
    "actions/checkout@9c091bb21b7c1c1d1991bb908d89e4e9dddfe3e0"
)
ORACLE_SETUP_PYTHON_ACTION = (
    "actions/setup-python@ece7cb06caefa5fff74198d8649806c4678c61a1"
)
ORACLE_UPLOAD_ARTIFACT_ACTION = (
    "actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a"
)
ORACLE_RENDER_STEP_SHA256 = (
    "2749d6af5803d3c39cf6f3ac102f624270a1f890dc7d5de857e68d8dc9f52ffa",
    "fd310c68a384cb3379a59a669357ba6d0106dc5c551d0194586f94ec6184a55a",
    "bb87d04b1e41f135497a80b94c55791c6f8fc109bc50d7941b704ebfa3a8a4eb",
    "63a6303f2a8a61524a3fa5e5f92fcb0fb4e013aebaec12b273a28bc4567b5559",
    "e68111b94d173e641656a08128f361c61c4521f9262bd5ab7644eff4e4c4ad52",
    "550ef35d2ed02fa5403ec22cf9526fe431a4908ba7bf6b8bcde23156fee79c66",
    "455b842e761235cf52cc695d818461372c5b1c99132d9c6df12224ca82af42bb",
    "0308865d11b5e8e1a6d43e19a0b5f0b942799aef63ba811d05fb0eaaec5687bc",
    "11c3256a804cc01812f61ef29b4704e5386a7a20ad06a7986802885ed54be6aa",
    "d57b5ea788b9602ed0e8dc38eda6a5ac2bc067b6167883ebdf55340681ac5547",
    "13e889c1b3e254d53879c4a593f72f0d0c45f2962b9b7b1590717ca8a5820b38",
    "1f3453b34f490f6ec86e9655d994b5af8fb7fb7c68c0da7b6016112e4b764202",
    "3a68e719dd211780d0c0778445d63fc19cdbcadf80fc7068e132c623f38030ac",
)
ORACLE_HARDENING_IMAGE_STEP_SHA256 = (
    "82dcaaf5e601cb509cf5312a5caf66a1f08e651165b53a3758e346938a32b7f4",
    "974a8f3bf55df0faabfb0d3bbbf0bd87a9692941a3c7f2d619bd9916694bcda5",
    "63a6303f2a8a61524a3fa5e5f92fcb0fb4e013aebaec12b273a28bc4567b5559",
    "5eb296aeb7a081fef5622668a2658e484191f93958a318518d4253a22f92d2bc",
    "1fbac1d8e41eb4bab96ebfdbdbd64a9964cc30e513bb3c804d3e80275d817e2b",
    "43d6bfd32a185411e10497a570623fec6e09413f8be78adcae671f8516b43b79",
)
ORACLE_RENDER_WORKFLOW_SHA256 = (
    "d4d2791735de8376e7adf8b282c707ff43bba903cd333dabd9b23b1e1f208d04"
)
ORACLE_HARDENING_WORKFLOW_SHA256 = (
    "b6ad857f1de193d8c00dfb3aded9ae14ad4b19d16b5aaf71d82e461b48c72c7f"
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
            return set(), ["workflow on trigger block is not a supported YAML collection"]
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


def _normalized_active_commands(text: str) -> list[str]:
    active = _without_commented_lines(text)
    normalized = re.sub(r"[ \t]*\\\r?\n[ \t]*", " ", active)
    return [line.strip() for line in normalized.splitlines() if line.strip()]


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
_PYTHON_INTERPRETER_RE = re.compile(
    r"(?:python|pypy)(?:\d+(?:\.\d+)*)?\Z"
)
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
                return (
                    index + 1 >= len(arguments)
                    or not _python_inline_is_policy_safe(arguments[index + 1])
                )
            if argument.startswith("-c") and len(argument) > 2:
                return not _python_inline_is_policy_safe(argument[2:])
            if "$" in argument and (
                index == 0 or argument.startswith("-")
            ):
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
        if wrapper == "command" and token.startswith("-") and any(
            flag in token[1:] for flag in ("v", "V")
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
                    tokens[: index - 1]
                    + split_tokens
                    + tokens[remaining_index:],
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
        and (
            _variable_name(tokens[index]) is not None
            or tokens[index].startswith("$")
        )
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
        token in {"-exec", "-execdir", "-ok", "-okdir"}
        for token in tokens[index + 1 :]
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
        and (
            _variable_name(arguments[1]) is not None
            or "$" in arguments[1]
        )
    ):
        return True
    return (
        arguments[0] == "build"
        or len(arguments) >= 2
        and arguments[:2] in (
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
        entry = _yaml_mapping_entry(
            _strip_yaml_inline_comment(line.lstrip(" "))
        )
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
                if (
                    _yaml_mapping_entry(_strip_yaml_inline_comment(content))
                    is None
                ):
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


def _audit_oracle_buildx_setup(
    path: Path, text: str, errors: list[str]
) -> None:
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
        "buildkitd-flags: --oci-worker-snapshotter=overlayfs",
    )
    if any(setup.count(snippet) != 1 for snippet in required):
        errors.append(
            f"{path}: oracle builds must pin Buildx, BuildKit, linux/amd64, "
            "overlayfs, and disabled GitHub provenance"
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
        command
        for command in step_commands
        if "install wasm-bindgen-cli" in command
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
    if len(install_positions) != len(required_install_commands) or install_positions != sorted(
        install_positions
    ):
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
    workflows.  Flow mappings, duplicate ``jobs`` keys, reusable-workflow jobs,
    and jobs whose steps cannot be scoped are rejected instead of being
    silently omitted from the PR-head policy.
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
        job_text = "\n".join(lines[job_start:job_end])
        sequences = _workflow_step_sequences(job_text)
        if len(sequences) != 1:
            errors.append(f"job {name!r} must contain exactly one inline steps sequence")
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


def _step_values(
    entries: list[tuple[str, str, int, int]], key: str
) -> list[str]:
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
        entry = _yaml_mapping_entry(
            _strip_yaml_inline_comment(line.lstrip(" "))
        )
        if entry is not None and entry[0] == "uses":
            errors.append(
                f"{path}: oracle-build job {job_name!r} cannot call a reusable workflow"
            )

    actual: list[str] = []
    actual_step_sha256: list[str] = []
    unsupported = False
    for step_indent, block in matches[0]:
        canonical_block = "\n".join(
            _strip_yaml_inline_comment(line) for line in block
        )
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
    if _step_values(entries, "run") != [
        'test "$(git rev-parse HEAD)" = "$EXPECTED_SHA"'
    ]:
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
    expected_expression = "${{ github.event.pull_request.head.sha || github.sha }}"
    jobs, job_errors = _workflow_job_step_sequences(active)
    errors.extend(f"{path}: {error}" for error in job_errors)
    for job_name, blocks in jobs:
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
                f'{RELEASE_VERSIONS["CARGO_FUZZ_VERSION"]}'
            )

    if re.search(r"rustup\s+toolchain\s+install\s+nightly(?:\s|$)", text):
        errors.append(f"{path}: workflow must not install mutable nightly")
    if re.search(r"cargo\s+\+nightly(?:\s|$)", text):
        errors.append(f"{path}: workflow must not invoke mutable nightly")
    return errors


def audit_release_versions(path: Path, text: str) -> list[str]:
    """Return violations for release toolchain and cargo-fuzz version pins."""

    return audit_fuzz_tools(path, text, tuple(RELEASE_VERSIONS))


def audit_fuzz_workflow(path: Path, text: str) -> list[str]:
    """Return violations for the standalone hosted fuzz workflow."""

    return audit_fuzz_tools(
        path, text, ("FUZZ_NIGHTLY_VERSION", "CARGO_FUZZ_VERSION")
    )


def audit_render_oracle_workflow(path: Path, text: str) -> list[str]:
    """Require exact identities and bounded pilot/full rendering campaigns."""

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
        ),
        ORACLE_RENDER_STEP_SHA256,
        errors,
    )
    _audit_oracle_buildx_setup(path, active, errors)
    required = {
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
        "--output target/render-oracle-hosted/host-tools.json": (
            "must emit path-neutral hosted identity evidence"
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
        "if: always()": "must upload bootstrap or mismatch identity evidence",
        'RXLS_ORACLE_CAMPAIGN: ${{ github.event_name == \'workflow_dispatch\' && inputs.campaign || \'pilot\' }}': (
            "push and schedule runs must stay on the bounded pilot"
        ),
        'test "$(git rev-parse HEAD)" = "$GITHUB_SHA"': (
            "must verify the exact checked-out commit"
        ),
        "timeout-minutes: ${{ github.event_name == 'workflow_dispatch' && inputs.campaign == 'full' && 330 || 120 }}": (
            "must keep the pilot at 120 minutes and bound explicit full campaigns at 330"
        ),
        '--profile "$RXLS_ORACLE_CAMPAIGN"': (
            "must generate and verify the selected deterministic profile"
        ),
        'row.get("name") == "pdffonts"': (
            "must select pdffonts from verified host identity evidence"
        ),
        '--pdffonts-binary-sha256 "$PDFFONTS_SHA256"': (
            "must bind PDF font inspection to the verified host binary"
        ),
        '--shard-count "$shard_count"': (
            "full campaigns must use the harness content-identity sharder"
        ),
        'if int(row["sha256"][:16], 16) % 4 == shard_index': (
            "must preflight the same deterministic content-identity shards"
        ),
        "assert all(180 <= len(rows) <= 220 for rows in shards)": (
            "full shards must remain balanced and bounded"
        ),
        '40 <= sum(row["format"] == format_name for row in rows) <= 60': (
            "every full shard must remain balanced by format"
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
        'assert authored_gate["schema"] == "rxls.authored-print-parity.v1"': (
            "must verify the aggregate authored-print gate schema"
        ),
        'assert authored_gate["passed"] is True': (
            "must reject failed authored-print aggregate evidence"
        ),
        'authored_gate["evidence"]["oracle_libreoffice_artifact_sha256"]': (
            "must bind authored-print evidence to the locked LibreOffice artifact"
        ),
        'authored_gate["evidence"]["pdffonts_sha256"] == pdffonts_sha256': (
            "must bind authored-print text evidence to the pinned PDF inspector"
        ),
        '"schema": "rxls.render-oracle-hosted-campaign.v5"': (
            "must emit the aggregate-only hosted campaign contract"
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
        '"reviewed_baseline_available": all(': (
            "must distinguish a reviewed ratchet from a bootstrap candidate"
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
        'gate["evidence"]["pdffonts_sha256"] == pdffonts_sha256': (
            "must bind absolute-gate font inspection to the pinned host tool"
        ),
        "compression-level: 9": "must bound aggregate artifact transfer size",
    }
    for snippet, message in required.items():
        if snippet not in active:
            errors.append(f"{path}: {message}")
    if re.search(r"python-version:\s*[\"']?3\.13[\"']?\s*$", text, re.MULTILINE):
        errors.append(f"{path}: mutable Python minor selectors are forbidden")
    if "runtime_verified_unpinned" in text or "runtime_verified" in text:
        errors.append(f"{path}: normal oracle gates must not accept unpinned identities")
    if re.search(r"check-render-parity-baseline\.py(?s:.*?)--create", text):
        errors.append(
            f"{path}: hosted gates must not auto-approve their own reviewed baseline"
        )

    exact_assignments = {
        "FULL_CASE_COUNT": RENDER_ORACLE_FULL_CASES,
        "FULL_REPEAT_COUNT": RENDER_ORACLE_FULL_REPEATS,
        "FULL_SHARD_COUNT": RENDER_ORACLE_FULL_SHARDS,
        "MAX_PARALLEL_SHARDS": RENDER_ORACLE_MAX_PARALLEL_SHARDS,
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
        r"(?P<body>.*?)(?=^\s{6}bootstrap_identities:\s*$)",
        text,
    )
    if campaign_input is None:
        errors.append(f"{path}: missing workflow_dispatch pilot/full campaign choice")
    else:
        body = campaign_input.group("body")
        if (
            "type: choice" not in body
            or "default: pilot" not in body
            or len(re.findall(r"^\s+- pilot\s*$", body, re.MULTILINE)) != 1
            or len(re.findall(r"^\s+- full\s*$", body, re.MULTILINE)) != 1
        ):
            errors.append(
                f"{path}: workflow_dispatch campaign must be an exact pilot/full choice"
            )

    if re.search(
        r"--max-(?:similarity|blur|mask)-drift-ppm(?:=|\s)", text
    ):
        errors.append(
            f"{path}: same-SHA drift thresholds must use the calibrated checked-in defaults"
        )
    if text.count('test "$FULL_REPEAT_COUNT" = "2"') != 1:
        errors.append(f"{path}: full mode must require exactly two same-SHA campaigns")
    if text.count('test "$FULL_SHARD_COUNT" = "4"') != 1:
        errors.append(f"{path}: full mode must require exactly four deterministic shards")
    if text.count('test "$MAX_PARALLEL_SHARDS" = "2"') != 1:
        errors.append(f"{path}: full mode must cap concurrent shard processes at two")
    if len(
        re.findall(
            r"^\s*python3 scripts/check-render-fidelity-targets\.py\s+\\$",
            text,
            re.MULTILINE,
        )
    ) != 2:
        errors.append(f"{path}: pilot/full evidence needs one absolute gate per campaign")

    upload = re.search(
        r"(?ms)^\s+- name: Upload path-neutral aggregate identities only\s*$"
        r".*?^\s+path:\s*\|\s*$\n(?P<paths>(?:\s+target/[^\n]+\n)+)"
        r"\s+compression-level:\s*9\s*$",
        text,
    )
    allowed_artifacts = {
        "target/render-oracle-hosted/authored-print-gate.json",
        "target/render-oracle-hosted/baseline-candidate-a.json",
        "target/render-oracle-hosted/baseline-candidate-b.json",
        "target/render-oracle-hosted/baseline-gate-a.json",
        "target/render-oracle-hosted/baseline-gate-b.json",
        "target/render-oracle-hosted/build.json",
        "target/render-oracle-hosted/fidelity-a.json",
        "target/render-oracle-hosted/fidelity-b.json",
        "target/render-oracle-hosted/hosted-summary.json",
        "target/render-oracle-hosted/host-tools.json",
        "target/render-oracle-hosted/repeatability.json",
        "target/render-oracle-hosted/renderer.json",
    }
    if upload is None:
        errors.append(f"{path}: aggregate-only artifact allowlist is missing")
    else:
        uploaded = {
            line.strip() for line in upload.group("paths").splitlines() if line.strip()
        }
        if uploaded != allowed_artifacts:
            errors.append(f"{path}: hosted artifacts must use the exact aggregate-only allowlist")

    apt_lines = [line for line in text.splitlines() if "apt-get " in line]
    bootstrap_matches = re.finditer(
        r'if \[\[ "\$RXLS_IDENTITY_BOOTSTRAP" == "1" \]\]; then\n'
        r'(?P<body>(?:\s+[^\n]*\n)+?)\s+fi',
        text,
    )
    bootstrap_bodies = [match.group("body") for match in bootstrap_matches]
    unpinned_installs = [
        line
        for line in apt_lines
        if "install" in line and '"${SYSTEM_PACKAGES[@]}"' not in line
    ]
    unpinned_is_bootstrap_only = any(
        all(line.strip() in body for line in unpinned_installs)
        for body in bootstrap_bodies
    )
    if (
        len(apt_lines) != 3
        or len(unpinned_installs) != 1
        or not unpinned_is_bootstrap_only
        or text.count('"${SYSTEM_PACKAGES[@]}"') != 1
    ):
        errors.append(
            f"{path}: apt must use bootstrap-only top-level packages or the exact pinned closure"
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
    ):
        if trigger_path not in pull_request.splitlines():
            errors.append(
                f"{path}: pull requests must trigger hardening for {trigger_path.strip()[2:]}"
            )

    pdf_job = _single_yaml_block(path, active, "pdf:", 2, "pdf job", errors)
    pdf_runners = re.findall(r"^\s{4}runs-on:\s*(\S+)\s*$", pdf_job, re.MULTILINE)
    if pdf_runners != ["ubuntu-24.04"]:
        errors.append(f"{path}: PDF hardening must use only ubuntu-24.04")
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
    if "run: python3 scripts/check_workflow_policy.py" not in pdf_policy_step:
        errors.append(f"{path}: PDF job must actively enforce hosted workflow policy")

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
        "sudo apt-get update": "host bootstrap must refresh its package source",
        "sudo apt-get install --yes --no-install-recommends libcairo2 poppler-utils": (
            "host bootstrap must install only the declared comparison tools"
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
        'sudo apt-get install --yes --no-install-recommends "${SYSTEM_PACKAGES[@]}"': (
            "strict PDF gate must install only exact locked package specs"
        ),
        (
            "python3 scripts/render-oracle-host-tools.py verify --scope poppler "
            "--output target/poppler-identity.json"
        ): "strict PDF gate must verify and record the complete Poppler closure",
    }.items():
        if command not in strict_commands:
            errors.append(f"{path}: {message}")
    bootstrap_index = pdf_job.find("Capture an unpinned host identity and fail closed")
    strict_index = pdf_job.find("Verify the pinned Poppler PDF gate")
    if bootstrap_index < 0 or strict_index < 0 or bootstrap_index >= strict_index:
        errors.append(f"{path}: host bootstrap must precede the strict PDF gate")

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
    image_runners = re.findall(
        r"^\s{4}runs-on:\s*(\S+)\s*$", image_job, re.MULTILINE
    )
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
        or sum("apt-get update" in line for line in apt_lines) != 2
        or sum(
            'apt-get install --yes --no-install-recommends "${SYSTEM_PACKAGES[@]}"'
            in line
            for line in apt_lines
        )
        != 1
        or sum(
            "apt-get install --yes --no-install-recommends libcairo2 poppler-utils"
            in line
            for line in apt_lines
        )
        != 1
    ):
        errors.append(
            f"{path}: PDF apt inputs must be the fail-closed bootstrap or exact lock"
        )
    if "poppler-version.txt" in active or "command -v pdfinfo |" in active:
        errors.append(f"{path}: path-bearing Poppler evidence is forbidden")
    return errors


def audit_codeql_workflow(path: Path, text: str) -> list[str]:
    """Require explicit CodeQL builds for every shipped Rust surface."""

    errors: list[str] = []
    normalized = re.sub(r"[ \t]*\\\r?\n[ \t]*", " ", text)
    commands = (
        "cargo build --all-targets --all-features --locked",
        "cargo build --manifest-path render/Cargo.toml --all-targets --locked",
        "cargo build --manifest-path bindings/render-wasm/Cargo.toml --all-targets --locked",
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
        errors.append(f"{path}: explicit Rust builds must run between CodeQL init and analysis")
    return errors


def audit_render_browser_workflow(path: Path, text: str) -> list[str]:
    """Require the browser lane to build wasm-bindgen with its exact Rust pin."""

    errors: list[str] = []
    active = _without_commented_lines(text)
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
    return errors


def audit_render_package_release_workflow(path: Path, text: str) -> list[str]:
    """Require a verification-only dispatch and protected, exact-tag npm publish."""

    errors: list[str] = []
    required = {
        'tags:\n      - "render-v*"': "must use the render-package-only tag namespace",
        "workflow_dispatch:": "must provide a verification-only manual dry run",
        'test "$GITHUB_REF_NAME" = "render-v$version"': (
            "must bind publication to the exact package version tag"
        ),
        'test "$GITHUB_REPOSITORY" = "HyunjoJung/rxls"': (
            "must reject publication from repository forks"
        ),
        'git merge-base --is-ancestor "$GITHUB_SHA" origin/main': (
            "must require the tagged commit to be on public main"
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
        ): (
            "must require an exact-SHA dispatched render-hardening run"
        ),
        ".github/workflows/render-browser.yml": (
            "must require the exact-SHA push render-browser path"
        ),
        "'[.head_sha, .event, .conclusion, .status, .path, .run_attempt] | @tsv'": (
            "must revalidate hosted run SHA, event, conclusion, status, path, "
            "and attempt"
        ),
        "--workflow render-oracle.yml": (
            "must require a successful exact-SHA Render Oracle run"
        ),
        '&& "$event" == "workflow_dispatch"': (
            "must accept full-oracle evidence only from deliberate dispatch"
        ),
        '&& "$run_path" == ".github/workflows/render-oracle.yml"': (
            "must validate the Render Oracle workflow path"
        ),
        '"$run_attempt" =~ ^[1-9][0-9]*$': (
            "must require a positive immutable hosted run attempt"
        ),
        'artifact_name="render-oracle-${GITHUB_SHA}-${run_id}-${run_attempt}-full"': (
            "must select only the exact-SHA, run-bound full-campaign artifact"
        ),
        'actions/runs/$run_id/artifacts': (
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
        "oracle-prerequisite.json": (
            "must preserve and reverify aggregate oracle prerequisite evidence"
        ),
        "python3 scripts/check_render_package.py": (
            "must enforce the bounded package/archive contract"
        ),
        "EmbarkStudios/cargo-deny-action@3c6349835b2b7b196a839186cb8b78e02f7b5f25": (
            "must use the pinned nested advisory and license gate"
        ),
        "manifest-path: bindings/render-wasm/Cargo.toml": (
            "cargo-deny must audit the nested render-WASM manifest"
        ),
        "arguments: --config deny.toml --locked --all-features": (
            "cargo-deny must use the root policy and locked complete feature graph"
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
        'cmp --silent \\': "must prove deterministic nested CycloneDX generation",
        "render-worker-sbom.cdx.json.sha256": (
            "must checksum and reverify nested CycloneDX evidence"
        ),
        "path: target/render-package/*": (
            "must upload the nested supply-chain evidence with the candidate"
        ),
        "python3 scripts/test_check_render_package.py": (
            "must run the focused immutable package tests"
        ),
        "python3 scripts/test_render_supply_chain.py": (
            "must run the focused nested supply-chain tests"
        ),
        "python3 scripts/test_check_render_oracle_release_evidence.py": (
            "must run the focused oracle-evidence tests"
        ),
        "npm publish --dry-run --ignore-scripts --access public": (
            "must execute the registry publication dry run"
        ),
        "sha256sum --check": "must reverify the immutable candidate checksum",
        "actions/download-artifact@": (
            "must transfer the verified candidate rather than rebuild it for publication"
        ),
        "digest-mismatch: error": "must fail closed on artifact transport drift",
        "if: github.event_name == 'push'": (
            "the publication job must not run for workflow_dispatch"
        ),
        "environment: npm-render-worker": (
            "registry mutation must pass through the protected deployment environment"
        ),
        "id-token: write": "npm publication must mint short-lived provenance identity",
        "registry-url: \"https://registry.npmjs.org\"": (
            "publication must target the public npm registry explicitly"
        ),
        "package-manager-cache: false": (
            "release jobs must not restore mutable package-manager caches"
        ),
        "NODE_AUTH_TOKEN: ${{ secrets.NPM_TOKEN }}": (
            "the first-package bootstrap token must be scoped to the publish step"
        ),
        "npm publish \\": "must contain a real tag-only publication command",
        "npm view \"$spec\" version dist.integrity repository.url --json": (
            "must verify the published registry identity and integrity"
        ),
        "npm install --ignore-scripts \"$spec\"": (
            "must execute an exact registry-installed consumer"
        ),
    }
    for snippet, message in required.items():
        if snippet not in text:
            errors.append(f"{path}: {message}")

    exact_assignments = {
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

    if text.count("NODE_AUTH_TOKEN:") != 1 or text.count("secrets.NPM_TOKEN") != 1:
        errors.append(f"{path}: npm bootstrap credentials must appear only on publish")
    if text.count("if: github.event_name == 'push'") != 2:
        errors.append(
            f"{path}: only the hosted prerequisites and publish job may be tag-only"
        )
    if text.count("package-manager-cache: false") != 2 or re.search(
        r"package-manager-cache:\s*true", text
    ):
        errors.append(f"{path}: both release jobs must disable npm caching")
    if re.search(r"^\s*pull_request:\s*$", text, re.MULTILINE):
        errors.append(f"{path}: pull requests must never enter the registry release workflow")
    if re.search(r"\bnpm\s+publish\b[^\n]*--force\b", text):
        errors.append(f"{path}: forced npm publication is forbidden")
    if len(re.findall(r"^\s*npm publish\b", text, re.MULTILINE)) != 2:
        errors.append(f"{path}: expected exactly one dry-run and one real npm publish")
    if text.count("scripts/render_supply_chain.py sbom") != 3:
        errors.append(
            f"{path}: expected two deterministic SBOM generations and one exact validation"
        )
    hosted_gate_calls = re.findall(
        r"^\s*require_successful_run(?:\s|\\)", text, re.MULTILINE
    )
    if len(hosted_gate_calls) != 4:
        errors.append(f"{path}: expected exact-SHA CI, CodeQL, hardening, and browser gates")
    deny_index = text.find("EmbarkStudios/cargo-deny-action@")
    build_index = text.find("npm --prefix bindings/render-wasm run build:wasm")
    if deny_index < 0 or build_index < 0 or deny_index > build_index:
        errors.append(f"{path}: nested dependency policy must run before building WASM")
    active = _without_commented_lines(text)
    verify_job = _single_yaml_block(
        path, active, "verify:", 2, "render package verify job", errors
    )
    build_step = _single_yaml_block(
        path,
        verify_job,
        "- name: Build the exact worker/WASM package",
        6,
        "render package wasm-bindgen build step",
        errors,
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
        if path.name == "render-oracle.yml":
            errors.extend(audit_render_oracle_workflow(relative, text))
        elif path.name == "render-hardening.yml":
            errors.extend(audit_render_hardening_workflow(relative, text))
        elif path.name == "render-browser.yml":
            errors.extend(audit_render_browser_workflow(relative, text))
        elif path.name == "render-package-release.yml":
            errors.extend(audit_render_package_release_workflow(relative, text))
        elif path.name == "codeql.yml":
            errors.extend(audit_codeql_workflow(relative, text))

    release = workflow_root / "release.yml"
    if not release.is_file():
        errors.append(f"{release.relative_to(root)}: missing release workflow")
    else:
        errors.extend(
            audit_release_versions(
                release.relative_to(root), release.read_text(encoding="utf-8")
            )
        )
    render_package_release = workflow_root / "render-package-release.yml"
    if not render_package_release.is_file():
        errors.append(
            f"{render_package_release.relative_to(root)}: missing render package release workflow"
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
