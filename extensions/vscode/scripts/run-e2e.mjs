import { copyFile, mkdir, open, rm, writeFile } from "node:fs/promises";
import { spawn } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { downloadAndUnzipVSCode } from "@vscode/test-electron";

const extensionRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const repositoryRoot = path.resolve(extensionRoot, "../..");
const target = path.join(extensionRoot, "target", "e2e");
const workspace = path.join(target, "workspace");
const fixtures = {
  xls: "reader-basic.xls",
  xlsx: "operations-report.xlsx",
  xlsm: "apache-poi-simple-macro.xlsm",
  xlsb: "reader-basic.xlsb",
  ods: "repeated-hidden.ods"
};
const sources = {
  xls: path.join(repositoryRoot, "tests", "fixtures", "xls", "reader-basic.xls"),
  xlsx: path.join(repositoryRoot, "viewer", "samples", "operations-report.xlsx"),
  xlsm: path.join(repositoryRoot, "viewer", "samples", "apache-poi-simple-macro.xlsm"),
  xlsb: path.join(repositoryRoot, "tests", "fixtures", "xlsb", "reader-basic.xlsb"),
  ods: path.join(repositoryRoot, "tests", "fixtures", "ods", "repeated-hidden.ods")
};

await rm(target, { recursive: true, force: true });
await mkdir(workspace, { recursive: true });
for (const [format, fileName] of Object.entries(fixtures)) {
  await copyFile(sources[format], path.join(workspace, fileName));
}
const invalidFile = "untrusted-input.xlsx";
const oversizedFile = "oversized.xlsx";
await writeFile(path.join(workspace, invalidFile), "not an OOXML package\n", "utf8");
const oversized = await open(path.join(workspace, oversizedFile), "w");
try {
  await oversized.truncate(32 * 1024 * 1024 + 1);
} finally {
  await oversized.close();
}

const version = process.env.RXLS_VSCODE_VERSION || "1.134.0";
const executable = await downloadAndUnzipVSCode({ version });
for (const mode of ["trusted", "untrusted"]) {
  const userData = path.join(target, `user-${mode}`);
  const extensions = path.join(target, `extensions-${mode}`);
  await mkdir(path.join(userData, "User"), { recursive: true });
  await mkdir(extensions, { recursive: true });
  await writeFile(
    path.join(userData, "User", "settings.json"),
    `${JSON.stringify(
      {
        "security.workspace.trust.enabled": mode === "untrusted",
        "security.workspace.trust.startupPrompt": "never",
        "security.workspace.trust.banner": "never",
        "workbench.startupEditor": "none",
        "telemetry.telemetryLevel": "off",
        "update.mode": "none",
        "extensions.autoUpdate": false,
        "extensions.autoCheckUpdates": false
      },
      null,
      2
    )}\n`,
    "utf8"
  );
  const launchArgs = [
    workspace,
    "--no-sandbox",
    "--disable-gpu-sandbox",
    "--disable-extensions",
    "--disable-updates",
    "--skip-release-notes",
    "--skip-welcome",
    "--no-cached-data",
    `--extensionDevelopmentPath=${extensionRoot}`,
    `--extensionTestsPath=${path.join(extensionRoot, "dist", "test", "e2e", "index.js")}`,
    "--user-data-dir",
    userData,
    "--extensions-dir",
    extensions
  ];
  if (mode === "trusted") {
    launchArgs.push("--disable-workspace-trust");
  }
  await runVSCode(executable, launchArgs, {
      RXLS_E2E_FIXTURES: JSON.stringify({ ...fixtures, invalidFile, oversizedFile }),
      RXLS_EXPECT_TRUST: mode === "trusted" ? "true" : "false"
  });
}

function runVSCode(executable, args, testEnvironment) {
  return new Promise((resolve, reject) => {
    const child = spawn(executable, args, {
      env: { ...process.env, ...testEnvironment },
      stdio: "inherit",
      windowsHide: true
    });
    child.once("error", reject);
    child.once("exit", (code, signal) => {
      if (code === 0) {
        resolve();
      } else {
        reject(
          new Error(
            `VS Code E2E exited with ${code ?? `signal ${signal ?? "unknown"}`}`
          )
        );
      }
    });
  });
}
