"""Offline safety tests for the verification-only release pipeline."""

from __future__ import annotations

import copy
import importlib.util
import io
import json
import tempfile
import unittest
from types import SimpleNamespace
from pathlib import Path
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("release_pipeline", ROOT / "scripts/release_pipeline.py")
pipeline = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(pipeline)
SHA = "a" * 40


def run(run_id, workflow, event="workflow_dispatch", **changes):
    value = {
        "id": run_id, "path": f".github/workflows/{workflow}", "head_sha": SHA,
        "head_branch": "main", "event": event, "status": "completed", "conclusion": "success",
        "run_attempt": 1, "repository": {"full_name": pipeline.REPOSITORY},
        "head_repository": {"full_name": pipeline.REPOSITORY},
        "html_url": f"https://github.com/{pipeline.REPOSITORY}/actions/runs/{run_id}",
    }
    value.update(changes)
    return value


class FakeGitHub:
    def __init__(self):
        self.main = SHA
        self.runs = {i: run(i, workflow, "push") for i, workflow in enumerate(pipeline.PREREQUISITES, 1)}
        self.posts = []
        self.post_result = "normal"
        self.tag_exists = False
        self.before_post = lambda: None

    def api(self, endpoint, *, payload=None):
        if endpoint.endswith("/git/ref/heads/main"):
            return {"object": {"sha": self.main}}
        if "/git/matching-refs/tags/" in endpoint:
            tag = endpoint.split("/tags/")[1]
            return [{"ref": f"refs/tags/{tag}"}] if self.tag_exists else []
        if endpoint.endswith("/releases?per_page=100"):
            return []
        if endpoint.endswith("/dispatches"):
            self.before_post()
            self.posts.append((endpoint, copy.deepcopy(payload)))
            workflow = endpoint.split("/workflows/")[1].split("/")[0]
            new_id = max(self.runs, default=0) + 1
            self.runs[new_id] = run(new_id, workflow, status="queued", conclusion=None)
            if self.post_result == "lost":
                raise pipeline.PipelineError("simulated lost POST response")
            if self.post_result == "empty":
                return None
            return {
                "workflow_run_id": new_id,
                "run_url": f"https://api.github.com/repos/{pipeline.REPOSITORY}/actions/runs/{new_id}",
                "html_url": self.runs[new_id]["html_url"],
            }
        if "/workflows/" in endpoint:
            workflow = endpoint.split("/workflows/")[1].split("/")[0]
            values = [copy.deepcopy(value) for value in self.runs.values() if value["path"] == f".github/workflows/{workflow}"]
            return {"total_count": len(values), "workflow_runs": values}
        if "/actions/runs/" in endpoint:
            return copy.deepcopy(self.runs[int(endpoint.rsplit("/", 1)[1])])
        raise AssertionError(f"unexpected fake API request: {endpoint}")


class PipelineTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.gh = FakeGitHub()
        self.store = pipeline.StateStore(Path(self.tmp.name), SHA)
        self.activation = mock.Mock()
        self.cli = pipeline.Pipeline(ROOT, SHA, self.gh, store=self.store, activate=self.activation)

    def test_sha_requires_full_lowercase_hex(self):
        for invalid in ("a" * 39, "A" * 40, "main", "a" * 40 + "/../../other", "g" * 40):
            with self.subTest(invalid=invalid), self.assertRaises(pipeline.PipelineError):
                pipeline.validate_sha(invalid)

    def test_plan_reports_versions_and_never_authorizes_publication(self):
        value = self.cli.plan()
        self.assertFalse(value["publication_allowed"])
        self.assertIn("rxls", {item["name"] for item in value["products"]})
        self.assertTrue(value["manual_publication"])
        self.assertEqual(self.gh.posts, [])
        self.assertFalse(self.store.path.exists())

    def test_existing_tags_are_publication_blockers(self):
        self.gh.tag_exists = True
        self.assertTrue(self.cli.plan()["publication_blockers"])

    def test_dry_run_does_not_mutate_state_or_dispatch(self):
        value = self.cli.advance()
        self.assertEqual(value["next_stage"], "hardening")
        self.assertEqual(value["action"], "dry_run")
        self.assertEqual(self.gh.posts, [])
        self.assertFalse(self.store.path.exists())

    def test_stale_main_prevents_status_and_post(self):
        self.gh.main = "b" * 40
        with self.assertRaisesRegex(pipeline.PipelineError, "main"):
            self.cli.advance(execute=True)
        self.assertEqual(self.gh.posts, [])

    def test_prerequisite_dispatch_does_not_substitute_for_push(self):
        self.gh.runs[1]["event"] = "workflow_dispatch"
        self.assertEqual(self.cli.advance()["action"], "blocked")
        self.assertEqual(self.gh.posts, [])

    def test_run_identity_and_failed_conclusions_fail_closed(self):
        for field, value in (
            ("run_attempt", 2), ("run_attempt", True), ("head_sha", "b" * 40),
            ("head_branch", "feature"), ("path", ".github/workflows/evil.yml"),
            ("head_repository", {"full_name": "fork/rxls"}),
            ("repository", {"full_name": "fork/rxls"}),
            ("conclusion", "cancelled"), ("conclusion", "skipped"),
        ):
            with self.subTest(field=field, value=value), self.assertRaises(pipeline.PipelineError):
                pipeline.validate_run(run(10, "ci.yml", "push", **{field: value}), SHA, "ci.yml", "push")

    def test_intent_is_persisted_before_post_and_pending_does_not_redispatch(self):
        self.gh.before_post = lambda: self.assertEqual(self.store.load()["records"]["hardening"]["phase"], "intent")
        result = self.cli.advance(execute=True)
        self.assertEqual(result["action"], "dispatched")
        record = self.store.load()["records"]["hardening"]
        self.assertEqual(record["run_id"], 4)
        self.assertEqual(self.cli.advance(execute=True)["action"], "waiting")
        self.assertEqual(len(self.gh.posts), 1)

    def test_lost_or_empty_post_response_never_adopts_nearby_run(self):
        for mode in ("lost", "empty"):
            with self.subTest(mode=mode):
                with tempfile.TemporaryDirectory() as tmp:
                    gh = FakeGitHub()
                    gh.post_result = mode
                    cli = pipeline.Pipeline(ROOT, SHA, gh, store=pipeline.StateStore(Path(tmp), SHA), activate=lambda: None)
                    self.assertEqual(cli.advance(execute=True)["action"], "dispatch_unknown")
                    self.assertEqual(cli.advance(execute=True)["action"], "blocked")
                    self.assertEqual(len(gh.posts), 1)

    def test_input_bearing_unowned_success_is_not_adopted(self):
        self.gh.runs[4] = run(4, "render-hardening.yml")
        self.gh.runs[5] = run(5, "render-oracle.yml")
        self.assertEqual(self.cli.advance()["next_stage"], "oracle")

    def test_owned_run_rerun_or_identity_change_blocks_resume(self):
        self.cli.advance(execute=True)
        self.gh.runs[4].update(status="completed", conclusion="success", run_attempt=2)
        self.assertEqual(self.cli.advance(execute=True)["action"], "blocked")
        self.assertEqual(len(self.gh.posts), 1)

    def test_main_race_before_dispatch_has_no_post(self):
        def race():
            self.gh.main = "b" * 40
        self.activation.side_effect = race
        with self.assertRaises(pipeline.PipelineError):
            self.cli.advance(execute=True)
        self.assertEqual(self.gh.posts, [])

    def test_activation_failure_has_no_post(self):
        self.activation.side_effect = pipeline.PipelineError("dirty checkout")
        with self.assertRaisesRegex(pipeline.PipelineError, "dirty"):
            self.cli.advance(execute=True)
        self.assertEqual(self.gh.posts, [])

    def test_two_pass_release_uses_exact_owned_first_run(self):
        for _ in pipeline.STAGES:
            result = self.cli.advance(execute=True)
            self.assertEqual(result["action"], "dispatched", result)
            record = self.store.load()["records"][result["next_stage"]]
            self.gh.runs[record["run_id"]].update(status="completed", conclusion="success")
        release_posts = [(endpoint, body) for endpoint, body in self.gh.posts if "/release.yml/" in endpoint]
        self.assertEqual(len(release_posts), 3)
        baseline = self.store.load()["records"]["core-baseline"]["run_id"]
        self.assertEqual(release_posts[0][1]["inputs"], {"baseline_run_id": ""})
        self.assertEqual(release_posts[1][1]["inputs"], {"baseline_run_id": str(baseline)})
        self.assertEqual(release_posts[2][1]["inputs"], {"baseline_run_id": "", "rehearse_publication": True})
        self.assertEqual(self.cli.advance()["action"], "verification_runs_complete")
        self.assertFalse(self.cli.status()["publication_allowed"])

    def test_full_oracle_and_viewer_inputs_are_verification_only(self):
        for _ in range(4):
            result = self.cli.advance(execute=True)
            record = self.store.load()["records"][result["next_stage"]]
            self.gh.runs[record["run_id"]].update(status="completed", conclusion="success")
        oracle = next(body for endpoint, body in self.gh.posts if "render-oracle.yml" in endpoint)
        viewer = next(body for endpoint, body in self.gh.posts if "viewer-pages.yml" in endpoint)
        self.assertEqual(oracle["inputs"], {"campaign": "full", "baseline_mode": "verify", "bootstrap_identities": False})
        self.assertEqual(viewer["inputs"], {"deploy": False})
        self.assertTrue(all(body["ref"] == "main" for _, body in self.gh.posts))

    def test_foreign_or_oversized_state_is_rejected(self):
        state = self.store.load()
        state["sha"] = "b" * 40
        self.store.path.parent.mkdir(parents=True)
        self.store.path.write_text(json.dumps(state), encoding="utf-8")
        with self.assertRaises(pipeline.PipelineError):
            self.store.load()
        self.store.path.write_bytes(b"x" * (pipeline.MAX_STATE_BYTES + 1))
        with self.assertRaises(pipeline.PipelineError):
            self.store.load()

    def test_dispatch_response_rejects_wrong_run_urls(self):
        response = {"workflow_run_id": 1, "run_url": "https://evil.example/1", "html_url": "https://evil.example/1"}
        with self.assertRaises(pipeline.PipelineError):
            pipeline.dispatch_run_id(response)

    def test_malformed_nested_identities_fail_closed(self):
        for key in ("repository", "head_repository"):
            with self.subTest(key=key), self.assertRaises(pipeline.PipelineError):
                pipeline.validate_run(run(1, "ci.yml", "push", **{key: None}), SHA, "ci.yml", "push")
        with mock.patch.object(self.gh, "api", return_value={"object": None}), self.assertRaises(pipeline.PipelineError):
            self.cli.status()

    def test_file_lock_prevents_concurrent_dispatch(self):
        if pipeline.os.name != "posix":
            self.skipTest("execution intentionally requires POSIX locks")
        with self.store.locked(), self.assertRaises(pipeline.PipelineError):
            self.cli.advance(execute=True)
        self.assertEqual(self.gh.posts, [])

    def test_dangling_state_symlink_is_not_treated_as_fresh_state(self):
        if pipeline.os.name != "posix":
            self.skipTest("symlink fixture requires POSIX")
        self.store.path.parent.mkdir(parents=True)
        self.store.path.symlink_to(self.store.path.parent / "missing.json")
        with self.assertRaisesRegex(pipeline.PipelineError, "symlink"):
            self.store.load()

    def test_existing_run_id_in_dispatch_response_is_never_accepted(self):
        self.gh.runs[4] = run(4, "render-oracle.yml")
        self.gh.runs[5] = run(5, "render-hardening.yml")
        original = self.gh.api
        def api(endpoint, *, payload=None):
            if endpoint.endswith("/dispatches"):
                self.gh.posts.append((endpoint, payload))
                return {"workflow_run_id": 4, "run_url": f"https://api.github.com/repos/{pipeline.REPOSITORY}/actions/runs/4", "html_url": self.gh.runs[4]["html_url"]}
            return original(endpoint, payload=payload)
        self.gh.api = api
        self.assertEqual(self.cli.advance(execute=True)["action"], "dispatch_unknown")
        self.assertEqual(self.cli.advance(execute=True)["action"], "blocked")
        self.assertEqual(len(self.gh.posts), 1)


class AdapterTests(unittest.TestCase):
    def test_execute_blocked_or_unknown_is_nonzero_but_status_remains_read_only(self):
        for action in ("blocked", "dispatch_unknown"):
            with self.subTest(action=action), mock.patch.object(pipeline, "Pipeline") as cli, \
                    mock.patch.object(pipeline.sys, "stdout", new_callable=io.StringIO):
                cli.return_value.advance.return_value = {"action": action, "publication_allowed": False}
                self.assertEqual(pipeline.main(["advance", "--sha", SHA, "--execute"]), 2)
                cli.return_value.status.return_value = {"action": action, "publication_allowed": False}
                self.assertEqual(pipeline.main(["status", "--sha", SHA]), 0)

    def test_api_pins_version_and_uses_no_shell_post_input(self):
        runner = mock.Mock()
        runner.run.return_value = '{"ok":true}'
        self.assertEqual(pipeline.GitHub(runner).api("endpoint", payload={"ref": "main"}), {"ok": True})
        command, body = runner.run.call_args.args
        self.assertIn("X-GitHub-Api-Version: 2026-03-10", command)
        self.assertIn("POST", command)
        self.assertEqual(json.loads(body), {"ref": "main"})

    def test_shared_subprocess_call_budget_rejects_before_launch(self):
        runner = pipeline.Runner(ROOT)
        runner.calls = pipeline.MAX_CALLS
        with mock.patch.object(pipeline.subprocess, "Popen") as popen, self.assertRaises(pipeline.PipelineError):
            runner.run(["gh", "api", "endpoint"])
        popen.assert_not_called()

    def test_streaming_output_cap_kills_child_without_shell(self):
        process = mock.Mock(pid=123)
        process.poll.return_value = None
        selector = mock.MagicMock()
        selector.__enter__.return_value = selector
        selector.get_map.return_value = {1: object()}
        selector.select.return_value = [(SimpleNamespace(fileobj=process.stdout, data=True), None)]
        with mock.patch.object(pipeline.subprocess, "Popen", return_value=process) as popen, \
                mock.patch.object(pipeline.selectors, "DefaultSelector", return_value=selector), \
                mock.patch.object(pipeline.os, "read", return_value=b"x" * (pipeline.MAX_RESPONSE_BYTES + 1)), \
                mock.patch.object(pipeline.os, "killpg", create=True) as killpg:
            with self.assertRaisesRegex(pipeline.PipelineError, "byte budget"):
                pipeline.Runner(ROOT).run(["gh", "api", "endpoint"])
            self.assertNotIn("shell", popen.call_args.kwargs)
            if pipeline.os.name == "posix":
                killpg.assert_called_once()
            else:
                process.kill.assert_called_once()

    def test_clean_activation_runs_central_workflow_policy(self):
        def result(command, *unused):
            if command[1:3] == ["rev-parse", "HEAD"]:
                return SHA
            if command[1:4] == ["remote", "get-url", "origin"]:
                return f"git@github.com:{pipeline.REPOSITORY}.git"
            return ""
        runner = mock.Mock()
        runner.run.side_effect = result
        pipeline.activate_checkout(ROOT, SHA, runner)
        runner.run.assert_any_call([pipeline.sys.executable, "scripts/check_workflow_policy.py", "--root", str(ROOT)])
        tracked = next(call.args[0] for call in runner.run.call_args_list if "ls-files" in call.args[0])
        self.assertIn("scripts/check_workflow_policy.py", tracked)
        self.assertIn("scripts/core_release_handoff.py", tracked)

    def test_timeout_kills_group_even_after_leader_exits_with_open_pipes(self):
        if pipeline.os.name != "posix":
            self.skipTest("process groups require POSIX")
        process = mock.Mock(pid=123)
        process.poll.return_value = 0
        selector = mock.MagicMock()
        selector.__enter__.return_value = selector
        selector.get_map.return_value = {1: object()}
        with mock.patch.object(pipeline.subprocess, "Popen", return_value=process), \
                mock.patch.object(pipeline.selectors, "DefaultSelector", return_value=selector), \
                mock.patch.object(pipeline.time, "monotonic", side_effect=[0, 0, 0, 31]), \
                mock.patch.object(pipeline.os, "killpg") as killpg:
            with self.assertRaisesRegex(pipeline.PipelineError, "timeout"):
                pipeline.Runner(ROOT).run(["gh", "api", "endpoint"])
            killpg.assert_called_once_with(123, pipeline.signal.SIGKILL)

    def test_dirty_wrong_head_and_foreign_origin_activation_fail_closed(self):
        for bad_field in ("head", "status", "origin"):
            def result(command, *unused):
                if command[1] == "rev-parse":
                    return "b" * 40 if bad_field == "head" else SHA
                if command[1] == "status":
                    return " M file" if bad_field == "status" else ""
                if command[1] == "remote":
                    return "git@github.com:fork/rxls.git" if bad_field == "origin" else f"git@github.com:{pipeline.REPOSITORY}.git"
                return ""
            runner = mock.Mock()
            runner.run.side_effect = result
            with self.subTest(field=bad_field), self.assertRaises(pipeline.PipelineError):
                pipeline.activate_checkout(ROOT, SHA, runner)


if __name__ == "__main__":
    unittest.main()
