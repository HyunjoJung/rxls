#!/usr/bin/env python3
"""Resume bounded, verification-only hosted release stages for one exact main SHA.

This tool does not attest artifacts, bump versions, commit, push, tag, deploy, or
publish. Existing hosted publication gates remain authoritative. Dispatch uses
GitHub API 2026-03-10's returned run ID; a lost response is never guessed from
nearby runs and never retried automatically.
All accepted runs must still be attempt 1; reruns require explicit maintainer
review instead of silently replacing previously observed evidence.
"""

from __future__ import annotations

import argparse
from contextlib import contextmanager
import json
import os
from pathlib import Path
import re
import selectors
import signal
import subprocess
import sys
import tempfile
import time
import tomllib


ROOT = Path(__file__).resolve().parents[1]
REPOSITORY = "HyunjoJung/rxls"
API = f"repos/{REPOSITORY}"
MAX_RESPONSE_BYTES = 2 * 1024 * 1024
MAX_STATE_BYTES = 128 * 1024
MAX_CALLS = 64
PREREQUISITES = ("ci.yml", "codeql.yml", "render-browser.yml")
STAGES = (
    ("hardening", "render-hardening.yml", {}),
    ("oracle", "render-oracle.yml", {"campaign": "full", "baseline_mode": "verify", "bootstrap_identities": False}),
    ("vscode", "vscode-extension.yml", {}),
    ("viewer", "viewer-pages.yml", {"deploy": False}),
    ("core-baseline", "release.yml", {"baseline_run_id": ""}),
    ("core-compare", "release.yml", None),
    ("render-package", "render-package-release.yml", {}),
)
STAGE_KEYS = {stage[0] for stage in STAGES}
DISCLAIMER = "Run completion is not artifact attestation or publication authorization; existing hosted release gates remain authoritative."


class PipelineError(ValueError):
    """A missing identity, boundedness, or safety prerequisite."""


def validate_sha(value):
    if not isinstance(value, str) or re.fullmatch(r"[0-9a-f]{40}", value) is None:
        raise PipelineError("--sha must be a full lowercase 40-hex commit SHA")
    return value


def positive_int(value):
    return type(value) is int and 0 < value < 2**63


def decode_json(value):
    def unique(pairs):
        result = {}
        for key, item in pairs:
            if key in result:
                raise PipelineError("duplicate JSON key")
            result[key] = item
        return result
    try:
        return json.loads(value, object_pairs_hook=unique)
    except (ValueError, UnicodeError, RecursionError) as error:
        raise PipelineError("invalid bounded JSON response or state") from error


class Runner:
    """No-shell subprocess adapter with shared call/time and streaming byte caps."""

    def __init__(self, root):
        self.root = root
        self.calls = 0
        self.deadline = time.monotonic() + 180

    def run(self, command, input_bytes=b""):
        self.calls += 1
        deadline = min(self.deadline, time.monotonic() + 30)
        if self.calls > MAX_CALLS or deadline <= time.monotonic() or len(input_bytes) > 4096:
            raise PipelineError("subprocess call/time/input budget exceeded")
        with tempfile.TemporaryFile() as stdin:
            stdin.write(input_bytes)
            stdin.seek(0)
            process = subprocess.Popen(command, cwd=self.root, stdin=stdin, stdout=subprocess.PIPE,
                                       stderr=subprocess.PIPE, start_new_session=os.name == "posix")
            output = bytearray()
            seen = 0
            completed = False
            try:
                with selectors.DefaultSelector() as selector:
                    selector.register(process.stdout, selectors.EVENT_READ, True)
                    selector.register(process.stderr, selectors.EVENT_READ, False)
                    while selector.get_map():
                        remaining = deadline - time.monotonic()
                        if remaining <= 0:
                            raise PipelineError("subprocess timeout; dispatch outcome may be unknown")
                        for key, _ in selector.select(min(remaining, 0.2)):
                            chunk = os.read(key.fileobj.fileno(), 65536)
                            if not chunk:
                                selector.unregister(key.fileobj)
                                continue
                            seen += len(chunk)
                            if seen > MAX_RESPONSE_BYTES:
                                raise PipelineError("subprocess response byte budget exceeded")
                            if key.data:
                                output.extend(chunk)
                code = process.wait(timeout=max(0.01, deadline - time.monotonic()))
                if code != 0:
                    raise PipelineError(f"{command[0]} failed (exit {code}); no automatic retry")
                decoded = output.decode("utf-8", errors="strict")
                completed = True
                return decoded
            except (subprocess.TimeoutExpired, UnicodeError) as error:
                raise PipelineError("subprocess timeout or invalid UTF-8 output") from error
            finally:
                # A leader can exit while a descendant keeps its pipes open.
                # Abort the whole group on failure, even after the leader exits.
                if not completed and os.name == "posix":
                    try:
                        os.killpg(process.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                elif process.poll() is None:
                    process.kill()
                process.wait(timeout=5)
                process.stdout.close()
                process.stderr.close()


class GitHub:
    def __init__(self, runner):
        self.runner = runner

    def api(self, endpoint, *, payload=None):
        command = ["gh", "api", "--hostname", "github.com", "--method", "GET" if payload is None else "POST",
                   "-H", "Accept: application/vnd.github+json", "-H", "X-GitHub-Api-Version: 2026-03-10", endpoint]
        body = b""
        if payload is not None:
            command += ["--input", "-"]
            body = json.dumps(payload, separators=(",", ":")).encode()
        return decode_json(self.runner.run(command, body))


def read_bounded(path, limit):
    if path.is_symlink():
        raise PipelineError("symlinked pipeline input/state is not accepted")
    if not path.is_file():
        raise PipelineError("pipeline input/state must be a regular file")
    with path.open("rb") as stream:
        value = stream.read(limit + 1)
    if len(value) > limit:
        raise PipelineError("local input/state byte budget exceeded")
    return value


class StateStore:
    def __init__(self, root, sha):
        self.root = Path(root).resolve()
        self.sha = validate_sha(sha)
        self.path = self.root / "target" / "release-pipeline" / sha / "state.json"

    def _safe_directory(self):
        current = self.root
        for part in ("target", "release-pipeline", self.sha):
            current /= part
            if current.is_symlink():
                raise PipelineError("pipeline state directory must not traverse symlinks")

    def load(self):
        self._safe_directory()
        if self.path.is_symlink():
            raise PipelineError("symlinked pipeline state is not accepted")
        if not self.path.exists():
            return {"schema": "rxls.release-pipeline.v1", "repository": REPOSITORY, "sha": self.sha, "records": {}}
        value = decode_json(read_bounded(self.path, MAX_STATE_BYTES))
        if (not isinstance(value, dict) or set(value) != {"schema", "repository", "sha", "records"}
                or value["schema"] != "rxls.release-pipeline.v1" or value["repository"] != REPOSITORY
                or value["sha"] != self.sha or not isinstance(value["records"], dict)
                or not set(value["records"]).issubset(STAGE_KEYS)):
            raise PipelineError("foreign or invalid pipeline state")
        return value

    def save(self, value):
        self._safe_directory()
        encoded = (json.dumps(value, sort_keys=True, indent=2) + "\n").encode()
        if len(encoded) > MAX_STATE_BYTES:
            raise PipelineError("pipeline state byte budget exceeded")
        self.path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        temporary = None
        try:
            with tempfile.NamedTemporaryFile(dir=self.path.parent, delete=False) as stream:
                temporary = Path(stream.name)
                os.chmod(temporary, 0o600)
                stream.write(encoded)
                stream.flush()
                os.fsync(stream.fileno())
            os.replace(temporary, self.path)
            descriptor = os.open(self.path.parent, os.O_RDONLY)
            try:
                os.fsync(descriptor)
            finally:
                os.close(descriptor)
        finally:
            if temporary is not None:
                temporary.unlink(missing_ok=True)

    @contextmanager
    def locked(self):
        if os.name != "posix":
            raise PipelineError("--execute requires POSIX file locking; plan/status/dry-run remain available")
        import fcntl
        self._safe_directory()
        self.path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        descriptor = os.open(self.path.parent / ".lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
        try:
            try:
                fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError as error:
                raise PipelineError("another pipeline advance holds the local lock") from error
            yield
        finally:
            os.close(descriptor)


def validate_run(value, sha, workflow, event, expected_id=None):
    if not isinstance(value, dict) or not positive_int(value.get("id")):
        raise PipelineError("invalid workflow run ID")
    expected = {"head_sha": sha, "path": f".github/workflows/{workflow}", "event": event, "head_branch": "main"}
    if any(value.get(key) != item for key, item in expected.items()):
        raise PipelineError("workflow SHA/path/event/branch does not match the pinned stage")
    if expected_id is not None and value["id"] != expected_id:
        raise PipelineError("workflow returned a different run ID")
    if any(not isinstance(value.get(key), dict) or value[key].get("full_name") != REPOSITORY
           for key in ("repository", "head_repository")):
        raise PipelineError("workflow belongs to a fork or different repository")
    if type(value.get("run_attempt")) is not int or value["run_attempt"] != 1:
        raise PipelineError("rerun or unknown run attempt is not accepted")
    if value.get("html_url") != f"https://github.com/{REPOSITORY}/actions/runs/{value['id']}":
        raise PipelineError("workflow URL does not match its canonical run ID")
    if value.get("status") == "completed":
        if value.get("conclusion") != "success":
            raise PipelineError("workflow did not conclude successfully")
        return "passed"
    if value.get("status") not in {"queued", "in_progress", "waiting", "pending", "requested"} or value.get("conclusion") is not None:
        raise PipelineError("unrecognized or inconsistent workflow status")
    return "waiting"


def dispatch_run_id(value):
    if not isinstance(value, dict) or set(value) != {"workflow_run_id", "run_url", "html_url"}:
        raise PipelineError("dispatch did not return an authoritative run ID")
    run_id = value["workflow_run_id"]
    if (not positive_int(run_id) or value["run_url"] != f"https://api.github.com/repos/{REPOSITORY}/actions/runs/{run_id}"
            or value["html_url"] != f"https://github.com/{REPOSITORY}/actions/runs/{run_id}"):
        raise PipelineError("dispatch returned a foreign or malformed run identity")
    return run_id


def activate_checkout(root, sha, runner):
    if runner.run(["git", "rev-parse", "HEAD"]).strip() != sha:
        raise PipelineError("execution requires the pinned SHA checked out locally")
    if runner.run(["git", "status", "--porcelain", "--untracked-files=normal"]).strip():
        raise PipelineError("execution requires a clean tracked and untracked checkout")
    origin = runner.run(["git", "remote", "get-url", "origin"]).strip()
    if origin not in {f"git@github.com:{REPOSITORY}.git", f"https://github.com/{REPOSITORY}.git", f"https://github.com/{REPOSITORY}"}:
        raise PipelineError("origin is not the canonical GitHub repository")
    files = ["scripts/release_pipeline.py", "scripts/check_workflow_policy.py", "scripts/package-identities.json"] + [f".github/workflows/{stage[1]}" for stage in STAGES]
    runner.run(["git", "ls-files", "--error-unmatch", "--", *files])
    runner.run(["git", "check-ignore", "--", f"target/release-pipeline/{sha}/state.json"])
    # Reuse the canonical semantic guard audit, including verification/deploy
    # separation, rather than accepting a few matching workflow substrings.
    runner.run([sys.executable, "scripts/check_workflow_policy.py", "--root", str(root)])


class Pipeline:
    def __init__(self, root, sha, github, *, store=None, activate=None):
        self.root = Path(root)
        self.sha = validate_sha(sha)
        self.gh = github
        self.store = store or StateStore(root, sha)
        self.activate = activate or (lambda: activate_checkout(self.root, self.sha, self.gh.runner))

    def _main(self):
        value = self.gh.api(f"{API}/git/ref/heads/main")
        if (not isinstance(value, dict) or not isinstance(value.get("object"), dict)
                or value["object"].get("sha") != self.sha):
            raise PipelineError("pinned SHA is no longer exact origin/main; no dispatch is permitted")

    def _runs(self, workflow, event):
        value = self.gh.api(f"{API}/actions/workflows/{workflow}/runs?head_sha={self.sha}&per_page=100")
        if (not isinstance(value, dict) or type(value.get("total_count")) is not int
                or not 0 <= value["total_count"] <= 100 or not isinstance(value.get("workflow_runs"), list)
                or len(value["workflow_runs"]) != value["total_count"]):
            raise PipelineError("run listing is truncated, malformed, or exceeds the 100-run bound")
        values = value["workflow_runs"]
        if any(not isinstance(item, dict) or not positive_int(item.get("id")) for item in values):
            raise PipelineError("run listing contains malformed IDs")
        if len({item["id"] for item in values}) != len(values):
            raise PipelineError("run listing contains duplicate IDs")
        return sorted((item for item in values if item.get("event") == event), key=lambda item: item["id"], reverse=True)

    def _check_run(self, run_id, workflow, event):
        run = self.gh.api(f"{API}/actions/runs/{run_id}")
        status = validate_run(run, self.sha, workflow, event, run_id)
        return {"status": status, "run_id": run_id, "run_attempt": 1, "url": run["html_url"]}

    def _inputs(self, key, inputs, records):
        if key == "core-compare":
            record = records.get("core-baseline")
            baseline = record.get("run_id") if isinstance(record, dict) else None
            return {"baseline_run_id": str(baseline) if positive_int(baseline) else "<owned baseline run ID>"}
        return inputs

    def _gate(self, workflow, event, record=None, inputs=None):
        try:
            if record is not None:
                if (not isinstance(record, dict) or set(record) != {"workflow", "inputs", "phase", "run_id"}
                        or record["workflow"] != workflow or record["inputs"] != inputs
                        or record["phase"] not in {"intent", "unknown", "bound"}):
                    raise PipelineError("local dispatch record does not match this stage")
                if record["phase"] != "bound" or not positive_int(record["run_id"]):
                    raise PipelineError("dispatch outcome is unresolved; inspect GitHub manually, never auto-adopt or redispatch")
                return self._check_run(record["run_id"], workflow, event)
            runs = self._runs(workflow, event)
            if runs and (not inputs or runs[0].get("status") != "completed"):
                return self._check_run(runs[0]["id"], workflow, event)
            return {"status": "missing"}
        except PipelineError as error:
            return {"status": "blocked", "reason": str(error)}

    def status(self):
        self._main()
        state = self.store.load()
        gates = []
        for workflow in PREREQUISITES:
            gates.append({"key": workflow, "workflow": workflow, "event": "push", **self._gate(workflow, "push")})
        stages = []
        for key, workflow, configured in STAGES:
            inputs = self._inputs(key, configured, state["records"])
            gate = self._gate(workflow, "workflow_dispatch", state["records"].get(key), inputs)
            stages.append({"key": key, "workflow": workflow, "event": "workflow_dispatch", "inputs": inputs, **gate})
        next_stage = next((stage for stage in stages if stage["status"] != "passed"), None)
        if any(gate["status"] != "passed" for gate in gates):
            action = "blocked"
        elif next_stage is None:
            action = "verification_runs_complete"
        else:
            action = {"missing": "dry_run", "waiting": "waiting", "blocked": "blocked"}[next_stage["status"]]
        return {"repository": REPOSITORY, "sha": self.sha, "action": action,
                "next_stage": next_stage["key"] if next_stage else None,
                "prerequisites": gates, "stages": stages, "publication_allowed": False,
                "notice": DISCLAIMER}

    def advance(self, *, execute=False):
        if not execute:
            return self.status()
        with self.store.locked():
            report = self.status()
            if report["action"] != "dry_run":
                return report
            self.activate()
            self._main()
            # Revalidate accepted attempts immediately before creating the next intent.
            for gate in report["prerequisites"] + report["stages"]:
                if gate["status"] == "passed":
                    if self._check_run(gate["run_id"], gate["workflow"], gate["event"])["status"] != "passed":
                        raise PipelineError("a prerequisite is no longer completed successfully")
            stage = next(item for item in report["stages"] if item["key"] == report["next_stage"])
            before = self._runs(stage["workflow"], "workflow_dispatch")
            if any(item.get("status") != "completed" for item in before):
                raise PipelineError("a workflow run appeared before dispatch; retry status without dispatching")
            self._main()
            state = self.store.load()
            record = {"workflow": stage["workflow"], "inputs": stage["inputs"], "phase": "intent", "run_id": None}
            state["records"][stage["key"]] = record
            self.store.save(state)
            try:
                response = self.gh.api(f"{API}/actions/workflows/{stage['workflow']}/dispatches",
                                       payload={"ref": "main", "inputs": stage["inputs"]})
                run_id = dispatch_run_id(response)
                if run_id in {item["id"] for item in before}:
                    raise PipelineError("dispatch returned a pre-existing run ID")
                record.update(phase="bound", run_id=run_id)
                self.store.save(state)
            except (PipelineError, OSError) as error:
                record["phase"] = "unknown"
                self.store.save(state)
                return {**report, "action": "dispatch_unknown", "reason": str(error)}
            # A race moving main or a mismatched returned run is blocked on resume.
            self._main()
            self._check_run(run_id, stage["workflow"], "workflow_dispatch")
            return {**report, "action": "dispatched", "run_id": run_id,
                    "url": f"https://github.com/{REPOSITORY}/actions/runs/{run_id}"}

    def plan(self):
        self._main()
        catalog = decode_json(read_bounded(self.root / "scripts/package-identities.json", MAX_STATE_BYTES))
        if not isinstance(catalog, dict) or catalog.get("schema") != "rxls.package-identities.v1":
            raise PipelineError("unknown package identity catalog")
        products = []
        for ecosystem in ("cargo", "npm"):
            rules = catalog[ecosystem]
            if not isinstance(rules, dict) or len(rules) > 32:
                raise PipelineError("package identity count exceeded")
            for name, rule in rules.items():
                if not isinstance(rule, dict) or not isinstance(rule.get("manifest"), str):
                    raise PipelineError("invalid manifest rule")
                path = self.root / rule["manifest"]
                if not path.resolve().is_relative_to(self.root.resolve()):
                    raise PipelineError("package manifest escapes repository")
                content = read_bounded(path, MAX_RESPONSE_BYTES)
                manifest = tomllib.loads(content.decode())["package"] if ecosystem == "cargo" else decode_json(content)
                if not isinstance(manifest, dict):
                    raise PipelineError("invalid package manifest")
                version = manifest.get("version")
                if manifest.get("name") != name or not isinstance(version, str) or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?", version):
                    raise PipelineError("manifest identity/version does not match the catalog")
                prefix = {("cargo", "rxls"): "v", ("npm", "rxls-wasm"): "wasm-v", ("npm", "@rxls/render-worker"): "render-v"}.get((ecosystem, name))
                products.append({"name": name, "ecosystem": ecosystem, "version": version,
                                 "manifest": rule["manifest"], "tag": prefix + version if prefix else None})
        blockers = []
        releases = self.gh.api(f"{API}/releases?per_page=100")
        if not isinstance(releases, list) or len(releases) > 100 or any(not isinstance(item, dict) for item in releases):
            raise PipelineError("invalid bounded release listing")
        for product in products:
            tag = product["tag"]
            if tag:
                refs = self.gh.api(f"{API}/git/matching-refs/tags/{tag}")
                if not isinstance(refs, list) or len(refs) > 100 or any(not isinstance(item, dict) for item in refs):
                    raise PipelineError("invalid bounded tag listing")
                if any(item.get("ref") == f"refs/tags/{tag}" for item in refs):
                    blockers.append(f"{tag} already exists and is immutable; do not overwrite or republish automatically")
                if any(item.get("tag_name") == tag for item in releases):
                    blockers.append(f"GitHub release {tag} already exists; publication requires owner review")
        if len(releases) == 100:
            blockers.append("release listing reached its bound; older release existence is not established")
        return {"repository": REPOSITORY, "sha": self.sha, "products": products,
                "manifest_source": "local checkout; execution requires clean tracked source at the pinned main SHA",
                "prerequisites": [{"workflow": name, "event": "push"} for name in PREREQUISITES],
                "stages": [{"key": key, "workflow": workflow, "inputs": inputs if inputs is not None else {"baseline_run_id": "<owned baseline run ID>"}} for key, workflow, inputs in STAGES],
                "publication_allowed": False, "publication_blockers": blockers, "notice": DISCLAIMER,
                "manual_publication": [
                    "Review all exact-SHA artifact attestations and choose unused product versions; no automatic version bumps.",
                    "Core v<core version> publication remains maintainer-controlled after both release verification passes.",
                    "wasm-package-release.yml verification requires the already-published exact core tag/archive; it is intentionally not dispatched here.",
                    "wasm-v<core version>, render-v<render-worker version>, and VS Code distribution are separate maintainer publication boundaries.",
                ]}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("plan", "status", "advance"))
    parser.add_argument("--sha", required=True, help="exact lowercase 40-hex origin/main commit")
    parser.add_argument("--execute", action="store_true", help="advance only: dispatch at most one verification stage")
    args = parser.parse_args(argv)
    try:
        if args.execute and args.command != "advance":
            raise PipelineError("--execute is only valid with advance")
        cli = Pipeline(ROOT, args.sha, GitHub(Runner(ROOT)))
        result = cli.advance(execute=args.execute) if args.command == "advance" else getattr(cli, args.command)()
        print(json.dumps(result, sort_keys=True, indent=2))
        return 2 if args.execute and result.get("action") in {"blocked", "dispatch_unknown"} else 0
    except (PipelineError, OSError) as error:
        print(json.dumps({"error": str(error), "publication_allowed": False}), file=sys.stderr)
        return 2
    except (KeyError, TypeError, AttributeError, RecursionError, ValueError):
        print(json.dumps({"error": "malformed or over-complex pipeline input", "publication_allowed": False}), file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
