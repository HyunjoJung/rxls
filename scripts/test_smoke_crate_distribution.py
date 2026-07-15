#!/usr/bin/env python3
"""Tests for the isolated crate-distribution smoke helper."""

from __future__ import annotations

import io
import json
import os
import subprocess
import sys
import tarfile
import tempfile
import textwrap
import unittest
from pathlib import Path

from smoke_crate_distribution import SmokeError, _safe_extract_crate
from test_support import make_python_stub_executable


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "smoke_crate_distribution.py"


class CrateDistributionSmokeTests(unittest.TestCase):
    def test_crate_extraction_rejects_cross_platform_traversal_and_devices(self) -> None:
        drive = "C" + ":"
        extended_prefix = "\\" * 2 + "?" + "\\"
        unsafe_names = [
            "rxls-0.1.2/../../outside",
            r"rxls-0.1.2\..\..\outside",
            r"rxls-0.1.2/..\..\outside",
            f"{drive}/outside",
            f"rxls-0.1.2/{drive}/outside",
            "rxls-0.1.2/NUL",
            "rxls-0.1.2/COM1.txt",
            extended_prefix + drive + r"\outside",
        ]
        for unsafe_name in unsafe_names:
            with self.subTest(member=unsafe_name), tempfile.TemporaryDirectory() as temporary:
                work = Path(temporary)
                crate = work / "malicious.crate"
                with tarfile.open(crate, "w:gz") as archive:
                    manifest = b'[package]\nname = "rxls"\nversion = "0.1.2"\n'
                    manifest_info = tarfile.TarInfo("rxls-0.1.2/Cargo.toml")
                    manifest_info.size = len(manifest)
                    archive.addfile(manifest_info, io.BytesIO(manifest))
                    payload = b"outside"
                    payload_info = tarfile.TarInfo(unsafe_name)
                    payload_info.size = len(payload)
                    archive.addfile(payload_info, io.BytesIO(payload))

                destination = work / "destination"
                destination.mkdir()
                with self.assertRaises(SmokeError):
                    _safe_extract_crate(crate, destination)
                self.assertFalse((work / "outside").exists())

    def test_local_crate_runs_external_consumer_and_installed_cli_contracts(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            work = Path(temporary)
            package = work / "rxls-0.1.2"
            package.mkdir()
            (package / "Cargo.toml").write_text(
                '[package]\nname = "rxls"\nversion = "0.1.2"\n',
                encoding="utf-8",
            )
            crate = work / "rxls-0.1.2.crate"
            with tarfile.open(crate, "w:gz") as archive:
                archive.add(package, arcname=package.name)

            fixture = work / "fixture.xlsx"
            fixture.write_bytes(b"fixture")
            log = work / "cargo-log.jsonl"
            fake_cli = textwrap.dedent(
                f'''\
                #!/usr/bin/env python3
                import json
                import sys
                args = sys.argv[1:]
                if args == ["--version"]:
                    print("rxls 0.1.2")
                elif args == ["--help"]:
                    print("usage: rxls <command> <file>")
                    print("commands:")
                elif len(args) == 2 and args[0] == "diagnose":
                    print(json.dumps({{"schema_version": 1}}))
                elif args == ["diagnose"]:
                    print("usage: rxls diagnose <file>", file=sys.stderr)
                    raise SystemExit(64)
                else:
                    raise SystemExit(1)
                '''
            )
            fake_cargo_source = work / "fake-cargo.py"
            fake_cargo_source.write_text(
                textwrap.dedent(
                    f'''\
                    #!/usr/bin/env python3
                    import json
                    import os
                    from pathlib import Path
                    import sys

                    args = sys.argv[1:]
                    log = Path(os.environ["FAKE_CARGO_LOG"])
                    with log.open("a", encoding="utf-8") as stream:
                        stream.write(json.dumps(args) + "\\n")

                    if args[0] == "generate-lockfile":
                        manifest = Path(args[args.index("--manifest-path") + 1])
                        manifest.with_name("Cargo.lock").write_text("# fake lock\\n", encoding="utf-8")
                    elif args[0] == "run":
                        print("rxls external consumer ok: sheets=1")
                    elif args[0] == "install":
                        package_root = Path(args[args.index("--path") + 1])
                        assert package_root.is_dir()
                        assert 'version = "0.1.2"' in (package_root / "Cargo.toml").read_text(encoding="utf-8")
                        root = Path(args[args.index("--root") + 1])
                        bindir = root / "bin"
                        bindir.mkdir(parents=True)
                        cli = bindir / "rxls.py"
                        cli.write_text({fake_cli!r}, encoding="utf-8")
                        if os.name == "nt":
                            (bindir / "rxls.cmd").write_text(
                                '@echo off\\n"{sys.executable}" "%~dp0rxls.py" %*\\n',
                                encoding="utf-8",
                            )
                        else:
                            cli.rename(bindir / "rxls")
                            os.chmod(bindir / "rxls", 0o755)
                    else:
                        raise SystemExit(2)
                    '''
                ),
                encoding="utf-8",
            )
            fake_cargo = make_python_stub_executable(fake_cargo_source)
            report = work / "report.json"
            env = os.environ.copy()
            env["FAKE_CARGO_LOG"] = str(log)

            result = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--crate",
                    str(crate),
                    "--fixture",
                    str(fixture),
                    "--cargo",
                    str(fake_cargo),
                    "--write-report",
                    str(report),
                ],
                cwd=work,
                env=env,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            evidence = json.loads(report.read_text(encoding="utf-8"))
            self.assertEqual(evidence["schema"], "rxls.crate-distribution-smoke.v1")
            self.assertEqual(evidence["mode"], "local-crate")
            self.assertEqual(evidence["version"], "0.1.2")
            self.assertEqual(evidence["external_consumer"], "passed")
            self.assertEqual(evidence["cargo_install"], "passed")

            calls = [json.loads(line) for line in log.read_text(encoding="utf-8").splitlines()]
            self.assertEqual([call[0] for call in calls], ["generate-lockfile", "run", "install"])
            self.assertIn("--locked", calls[1])
            self.assertIn("--locked", calls[2])


if __name__ == "__main__":
    unittest.main()
