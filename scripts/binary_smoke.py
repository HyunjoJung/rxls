#!/usr/bin/env python3
"""Bounded stdlib-only smoke of already validated, unpacked CLI/MCP binaries."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
import queue
import subprocess
import threading
import time
import zipfile
import xml.etree.ElementTree as ET

MAX_LINE = 1024 * 1024
MAX_STDERR = 64 * 1024
MAX_NOTIFICATIONS = 8
MAX_TRANSCRIPT = 1024 * 1024
STEP_SECONDS = 10
NS = {"s": "http://schemas.openxmlformats.org/spreadsheetml/2006/main"}
TOOLS = {"workbook_open", "workbook_list_sessions", "workbook_inspect", "workbook_read_range", "workbook_compare", "workbook_export_sheet", "workbook_set_cells", "workbook_save_copy", "workbook_close"}


def decode_response(line: bytes, identity: int) -> dict:
    if len(line) > MAX_LINE or not line.endswith(b"\n"):
        raise ValueError("MCP response line absent/oversized/unterminated")
    value = json.loads(line)
    if not isinstance(value, dict) or value.get("jsonrpc") != "2.0" or type(value.get("id")) is not int or value["id"] != identity or "error" in value or not isinstance(value.get("result"), dict):
        raise ValueError("MCP response identity/protocol/result mismatch")
    return value["result"]


def tool_content(value: dict) -> dict:
    if value.get("isError", False) or not isinstance(value.get("structuredContent"), dict):
        raise ValueError("MCP tool did not return successful structured content")
    return value["structuredContent"]


def expect_read(value: dict, number: int) -> None:
    try:
        rows = value["rows"]
        if len(rows) != 1 or len(rows[0]) != 2 or rows[0][0]["value"] != {"type": "number", "value": number} or rows[0][1]["value"] != {"type": "formula", "formula": "A1*2", "cached": {"type": "number", "value": number * 2}}:
            raise ValueError("MCP read value/formula/cache mismatch")
    except (KeyError, TypeError, IndexError) as error:
        raise ValueError("MCP read result shape mismatch") from error


def expect_summary(value: dict) -> None:
    summary = value.get("recalculation")
    if not isinstance(summary, dict) or any(type(summary.get(name)) is not int for name in ("computed_cells", "unchanged_cells", "unsupported_cells")) or type(value.get("applied_edits")) is not int or summary != {"computed_cells": 1, "unchanged_cells": 0, "unsupported_cells": 0, "reasons": []} or value["applied_edits"] != 1:
        raise ValueError("MCP recalculation result mismatch")


def make_fixture(path: Path) -> bytes:
    prefix = '<?xml version="1.0" encoding="UTF-8"?>'
    payloads = {
        "[Content_Types].xml": '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/></Types>',
        "_rels/.rels": '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>',
        "xl/workbook.xml": '<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Data" sheetId="1" r:id="rId1"/></sheets><calcPr calcId="124519" calcMode="manual"/></workbook>',
        "xl/_rels/workbook.xml.rels": '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>',
        "xl/styles.xml": '<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><fonts count="1"><font><sz val="11"/><name val="Calibri"/></font></fonts><fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill></fills><borders count="1"><border/></borders><cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs><cellXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>',
        "xl/worksheets/sheet1.xml": '<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:B1"/><sheetData><row r="1"><c r="A1"><v>7</v></c><c r="B1"><f>A1*2</f><v>14</v></c></row></sheetData></worksheet>',
    }
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as archive:
        for name, text in sorted(payloads.items()):
            archive.writestr(zipfile.ZipInfo(name, (1980, 1, 1, 0, 0, 0)), (prefix + text).encode("utf-8"))
    return path.read_bytes()


class Client:
    def __init__(self, executable: Path, runtime: Path, deadline: float):
        self.process = subprocess.Popen([str(executable), "--root", str(runtime)], cwd=runtime, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.identity = 0
        self.responses = queue.Queue(maxsize=16)
        self.stderr = bytearray()
        self.errors = []
        self.transcript = []
        self.transcript_bytes = 0
        self.deadline = deadline
        self.stop_watchdog = threading.Event()

        def reader():
            try:
                while line := self.process.stdout.readline(MAX_LINE + 1):
                    if len(line) > MAX_LINE or not line.endswith(b"\n"):
                        raise ValueError("MCP output line limit")
                    self.responses.put_nowait(line)
            except Exception as error:
                self.errors.append(str(error))
                self.process.kill()

        def read_stderr():
            try:
                while block := self.process.stderr.read(4096):
                    if len(self.stderr) + len(block) > MAX_STDERR:
                        raise ValueError("MCP stderr limit")
                    self.stderr.extend(block)
            except Exception as error:
                self.errors.append(str(error))
                self.process.kill()

        def watchdog():
            if not self.stop_watchdog.wait(max(0, self.deadline - time.monotonic())) and self.process.poll() is None:
                self.errors.append("owned MCP overall deadline")
                self.process.kill()

        self.threads = [threading.Thread(target=reader, daemon=True), threading.Thread(target=read_stderr, daemon=True), threading.Thread(target=watchdog, daemon=True)]
        for thread in self.threads:
            thread.start()

    def record(self, direction: str, message: object) -> None:
        size = len(json.dumps(message).encode("utf-8"))
        if len(self.transcript) >= 64 or self.transcript_bytes + size > MAX_TRANSCRIPT:
            raise ValueError("MCP transcript limit")
        self.transcript_bytes += size
        self.transcript.append({direction: message})

    def send(self, message: dict) -> None:
        encoded = (json.dumps(message) + "\n").encode("utf-8")
        if len(encoded) > MAX_LINE:
            raise ValueError("MCP request/transcript limit")
        self.record("request", message)
        try:
            self.process.stdin.write(encoded)
            self.process.stdin.flush()
        except OSError as error:
            raise ValueError("owned MCP input write/flush failed") from error

    def call(self, method: str, params: dict) -> dict:
        self.identity += 1
        self.send({"jsonrpc": "2.0", "id": self.identity, "method": method, "params": params})
        for _ in range(MAX_NOTIFICATIONS + 1):
            remaining = min(STEP_SECONDS, self.deadline - time.monotonic())
            if remaining <= 0 or self.errors:
                raise ValueError("MCP bounded step/deadline failed")
            try:
                line = self.responses.get(timeout=remaining)
            except queue.Empty as error:
                raise ValueError("MCP response timeout") from error
            parsed = json.loads(line)
            self.record("response", parsed)
            if isinstance(parsed, dict) and "id" not in parsed and parsed.get("jsonrpc") == "2.0" and isinstance(parsed.get("method"), str):
                continue
            return decode_response(line, self.identity)
        raise ValueError("MCP notification limit")

    def tool(self, name: str, arguments: dict) -> dict:
        return tool_content(self.call("tools/call", {"name": name, "arguments": arguments}))

    def close(self) -> int:
        try:
            self.process.stdin.close()
        except (OSError, ValueError) as error:
            self.errors.append(f"owned MCP input close: {error}")
        finally:
            self.stop_watchdog.set()
            try:
                code = self.process.wait(timeout=STEP_SECONDS)
            except subprocess.TimeoutExpired:
                self.process.kill()
                code = self.process.wait(timeout=5)
                self.errors.append("owned MCP failed to exit normally")
            finally:
                for thread in self.threads:
                    thread.join(timeout=5)
        if code or self.stderr or self.errors or not self.responses.empty() or any(thread.is_alive() for thread in self.threads):
            raise ValueError("MCP exit/stderr/reader failure")
        return code


def command(executable: Path, arguments: list[str], runtime: Path) -> bytes:
    # Version/help/export are individually capped and never use PATH fallback.
    process = subprocess.Popen([str(executable), *arguments], cwd=runtime, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    outputs = [bytearray(), bytearray()]
    errors = []

    def collect(stream, output):
        try:
            while block := stream.read(4096):
                if len(output) + len(block) > MAX_STDERR:
                    errors.append("smoke output limit")
                    process.kill()
                    return
                output.extend(block)
        except OSError as error:
            errors.append(str(error))

    threads = [threading.Thread(target=collect, args=(stream, output), daemon=True) for stream, output in zip((process.stdout, process.stderr), outputs)]
    for thread in threads:
        thread.start()
    try:
        code = process.wait(timeout=STEP_SECONDS)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)
        errors.append("owned CLI/MCP smoke step timeout")
        code = -1
    for thread in threads:
        thread.join(timeout=5)
    record = {"binary": executable.name, "arguments": arguments, "exit_code": code, "errors": errors, "stdout_bytes": len(outputs[0]), "stdout_sha256": hashlib.sha256(outputs[0]).hexdigest(), "stderr_bytes": len(outputs[1]), "stderr": outputs[1].decode("utf-8", errors="replace")[:4096]}
    with (runtime / "commands.jsonl").open("a", encoding="utf-8", newline="\n") as stream:
        stream.write(json.dumps(record) + "\n")
    if code or outputs[1] or errors or any(thread.is_alive() for thread in threads):
        raise ValueError("version/help/CLI export output/exit mismatch")
    return bytes(outputs[0])


def smoke(unpacked: Path, manifest: dict, runtime: Path) -> dict:
    if runtime.exists():
        raise ValueError("runtime directory must be fresh")
    runtime.mkdir(parents=True)
    suffix = ".exe" if manifest["platform"] == "windows-x64" else ""
    cli, mcp = unpacked / f"bin/rxls{suffix}", unpacked / f"bin/rxls-mcp{suffix}"
    if command(cli, ["--version"], runtime) != f"rxls {manifest['versions']['cli']}\n".encode() or command(mcp, ["--version"], runtime) != f"rxls-mcp {manifest['versions']['mcp']}\n".encode():
        raise ValueError("unpacked binary version identity mismatch")
    if not command(cli, ["--help"], runtime).startswith(b"usage: ") or not command(mcp, ["--help"], runtime).startswith(f"rxls-mcp {manifest['versions']['mcp']}\n\nUSAGE:\n".encode()):
        raise ValueError("unpacked binary help contract mismatch")
    fixture = runtime / "input.xlsx"
    original = make_fixture(fixture)
    if command(cli, ["csv", str(fixture)], runtime) != b"7,14\n":
        raise ValueError("CLI exported content mismatch")
    client = Client(mcp, runtime, time.monotonic() + 90)
    stage = "initialize"
    primary_failure = cleanup_failure = None
    exit_code = None
    try:
        initialization = client.call("initialize", {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "rxls-binary-smoke", "version": "1"}})
        if initialization.get("protocolVersion") != "2025-03-26" or not isinstance(initialization.get("capabilities"), dict) or "tools" not in initialization["capabilities"]:
            raise ValueError("MCP initialization/capabilities mismatch")
        client.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        stage = "tools/list"
        listed = client.call("tools/list", {})
        tools = listed.get("tools")
        if not isinstance(tools, list) or len(tools) != len(TOOLS) or any(not isinstance(item, dict) or not isinstance(item.get("name"), str) for item in tools) or {item["name"] for item in tools} != TOOLS:
            raise ValueError("MCP tool identity mismatch")
        stage = "workbook_open:input"
        opened = client.tool("workbook_open", {"path": str(fixture)})
        identity = opened.get("session_id")
        if not isinstance(identity, str) or not identity:
            raise ValueError("MCP opened-session identity absent")
        stage = "workbook_read_range:before"
        expect_read(client.tool("workbook_read_range", {"session_id": identity, "sheet": "Data", "range": "A1:B1"}), 7)
        stage = "workbook_set_cells:recalculate"
        edited = client.tool("workbook_set_cells", {"session_id": identity, "sheet": "Data", "recalculate": True, "edits": [{"kind": "set", "cell": "A1", "value": {"type": "number", "value": 9}}]})
        expect_summary(edited)
        saved = runtime / "saved.xlsx"
        stage = "workbook_save_copy"
        client.tool("workbook_save_copy", {"session_id": identity, "path": str(saved)})
        stage = "workbook_open:saved"
        reopened = client.tool("workbook_open", {"path": str(saved)}).get("session_id")
        if not isinstance(reopened, str) or not reopened or reopened == identity:
            raise ValueError("MCP reopened-session identity mismatch")
        stage = "workbook_read_range:after"
        expect_read(client.tool("workbook_read_range", {"session_id": reopened, "sheet": "Data", "range": "A1:B1"}), 9)
        stage = "workbook_close:accounting"
        for session in (reopened, identity):
            client.tool("workbook_close", {"session_id": session})
        if client.tool("workbook_list_sessions", {}).get("retained_bytes") != 0:
            raise ValueError("MCP close/accounting mismatch")
    except Exception as error:
        primary_failure = error
    finally:
        try:
            (runtime / "stdio-transcript.json").write_text(json.dumps(client.transcript, indent=2) + "\n", encoding="utf-8", newline="\n")
        except OSError as error:
            if primary_failure is None:
                primary_failure = error
        finally:
            try:
                exit_code = client.close()
            except Exception as error:
                cleanup_failure = error
                exit_code = client.process.returncode
        (runtime / "stdio-result.json").write_text(json.dumps({"status": "failed" if primary_failure or cleanup_failure else "passed", "last_stage": stage, "primary_failure": str(primary_failure)[:4096] if primary_failure else None, "cleanup_failure": str(cleanup_failure)[:4096] if cleanup_failure else None, "exit_code": exit_code}, indent=2) + "\n", encoding="utf-8", newline="\n")
        (runtime / "stdio-stderr.log").write_bytes(client.stderr)
    if primary_failure:
        raise ValueError(f"MCP smoke stage {stage}: {primary_failure}; cleanup: {cleanup_failure}; exit: {exit_code}") from primary_failure
    if cleanup_failure:
        raise ValueError(f"MCP smoke cleanup: {cleanup_failure}; exit: {exit_code}") from cleanup_failure
    if any(path.is_symlink() or not path.is_file() or path.stat().st_size > MAX_STDERR for path in (fixture, saved)) or fixture.read_bytes() != original or command(cli, ["csv", str(saved)], runtime) != b"9,18\n":
        raise ValueError("original preservation or saved CLI export mismatch")
    with zipfile.ZipFile(fixture) as before, zipfile.ZipFile(saved) as after:
        if len(after.infolist()) != 6 or set(before.namelist()) != set(after.namelist()) or any(info.file_size > 32 * 1024 for info in after.infolist()):
            raise ValueError("MCP saved package inventory/size mismatch")
        for name in before.namelist():
            if name != "xl/worksheets/sheet1.xml" and before.read(name) != after.read(name):
                raise ValueError("MCP save rewrote an unrelated package part")
        xml = ET.fromstring(after.read("xl/worksheets/sheet1.xml"))
        result = xml.find(".//s:c[@r='B1']", NS)
        if result is None or result.findtext("s:f", "", NS) != "A1*2" or result.findtext("s:v", "", NS) != "18":
            raise ValueError("saved formula/cache XML mismatch")
    return {"status": "passed", "unpacked_binaries": True, "cli_version_help_export": True, "mcp_initialize_open_read_edit_recalculate_save_reopen_close": True, "mcp_exit_code": exit_code, "mcp_requests": client.identity, "mcp_server_info": initialization.get("serverInfo"), "transcript": client.transcript, "original_sha256": hashlib.sha256(original).hexdigest(), "saved_sha256": hashlib.sha256(saved.read_bytes()).hexdigest()}
