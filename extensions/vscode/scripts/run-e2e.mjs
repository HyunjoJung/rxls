import { copyFile, mkdir, open, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { spawn } from "node:child_process";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { downloadAndUnzipVSCode, runVSCodeCommand } from "@vscode/test-electron";

const extensionRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const repositoryRoot = path.resolve(extensionRoot, "../..");
const installed = process.argv.slice(2).includes("--installed");
const version = process.env.RXLS_VSCODE_VERSION || "1.134.0";
const target = path.join(tmpdir(), `rxls-vscode-e2e-${process.pid}`);
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

const executable = await downloadAndUnzipVSCode({ version });
let installedExtensionRoot;
let testHarness = extensionRoot;
if (installed) {
  const manifest = JSON.parse(await readFile(path.join(extensionRoot, "package.json"), "utf8"));
  const vsix = path.join(extensionRoot, "target", `${manifest.name}-${manifest.version}.vsix`);
  const extensions = path.join(target, "extensions-installed");
  const userData = path.join(target, "user-install");
  await mkdir(extensions, { recursive: true });
  await runVSCodeCommand(
    [
      "--user-data-dir",
      userData,
      "--extensions-dir",
      extensions,
      "--install-extension",
      vsix,
      "--force"
    ],
    { version }
  );
  installedExtensionRoot = await verifyInstalledExtension(extensions, manifest);
  testHarness = path.join(target, "test-harness");
  await mkdir(testHarness, { recursive: true });
  await writeFile(
    path.join(testHarness, "package.json"),
    `${JSON.stringify(
      {
        name: "rxls-installed-test-harness",
        publisher: "rxls-test",
        version: "0.0.0",
        private: true,
        engines: { vscode: "^1.96.0" },
        activationEvents: ["*"],
        main: "./extension.js"
      },
      null,
      2
    )}\n`,
    "utf8"
  );
  await writeFile(
    path.join(testHarness, "extension.js"),
    '"use strict";\nexports.activate = () => undefined;\n',
    "utf8"
  );
}

const modes = installed ? ["installed"] : parseModes(process.env.RXLS_E2E_MODES);
for (const mode of modes) {
  let launchWorkspace = workspace;
  if (mode === "virtual") {
    // Start with only a virtual root. Replacing the first folder from inside
    // the test can restart the extension host before its change event arrives.
    launchWorkspace = path.join(target, "virtual.code-workspace");
    await writeFile(
      launchWorkspace,
      `${JSON.stringify(
        { folders: [{ uri: "rxls-e2e-memory:/workspace", name: "rxls virtual E2E" }] },
        null,
        2
      )}\n`,
      "utf8"
    );
  }
  const userData = path.join(target, `user-${mode}`);
  const extensions = installed
    ? path.join(target, "extensions-installed")
    : path.join(target, `extensions-${mode}`);
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
    launchWorkspace,
    "--no-sandbox",
    "--disable-gpu-sandbox",
    "--disable-updates",
    "--skip-release-notes",
    "--skip-welcome",
    "--no-cached-data",
    `--extensionDevelopmentPath=${testHarness}`,
    `--extensionTestsPath=${path.join(extensionRoot, "dist", "test", "e2e", "index.js")}`,
    "--user-data-dir",
    userData,
    "--extensions-dir",
    extensions
  ];
  if (!installed) {
    launchArgs.push("--disable-extensions");
  }
  if (mode === "trusted") {
    launchArgs.push("--disable-workspace-trust");
  }
  if (mode === "virtual" || mode === "installed") {
    launchArgs.push("--disable-workspace-trust");
  }
  const representativeOnly = mode === "virtual" || mode === "installed";
  await runVSCode(executable, launchArgs, {
    RXLS_E2E_FIXTURES: JSON.stringify({ ...fixtures, invalidFile, oversizedFile }),
    RXLS_E2E_FORMATS: JSON.stringify(
      representativeOnly ? ["xlsx"] : ["xls", "xlsx", "xlsm", "xlsb", "ods"]
    ),
    RXLS_E2E_FAILURE_BOUNDARIES: mode === "trusted" ? "true" : "false",
    RXLS_E2E_VIRTUAL: mode === "virtual" ? "true" : "false",
    RXLS_E2E_SEED_DIRECTORY: mode === "virtual" ? workspace : "",
    RXLS_EXPECT_EXTENSION_ROOT: installedExtensionRoot ?? "",
    RXLS_EXPECT_PACKAGED: installed ? "true" : "false",
    RXLS_EXPECT_TRUST: mode === "untrusted" ? "false" : "true"
  });
}

function parseModes(value) {
  const modes = value ? value.split(",").map((mode) => mode.trim()) : ["trusted", "untrusted"];
  if (
    modes.length === 0 ||
    modes.some((mode) => !["trusted", "untrusted", "virtual"].includes(mode)) ||
    new Set(modes).size !== modes.length
  ) {
    throw new Error("RXLS_E2E_MODES must contain unique trusted, untrusted, or virtual modes");
  }
  return modes;
}

async function verifyInstalledExtension(extensions, manifest) {
  const installedEntries = (await readdir(extensions, { withFileTypes: true })).filter((entry) =>
    entry.isDirectory()
  );
  const prefix = `${manifest.publisher}.${manifest.name}-`.toLowerCase();
  const matches = installedEntries.filter((entry) => entry.name.toLowerCase().startsWith(prefix));
  if (matches.length !== 1) {
    throw new Error(`clean VSIX install created ${matches.length} ${prefix}* directories`);
  }
  const installedRoot = path.join(extensions, matches[0].name);
  const installedManifest = JSON.parse(
    await readFile(path.join(installedRoot, "package.json"), "utf8")
  );
  if (
    installedManifest.name !== manifest.name ||
    installedManifest.publisher !== manifest.publisher ||
    installedManifest.version !== manifest.version
  ) {
    throw new Error("clean VSIX install produced the wrong extension identity");
  }
  return installedRoot;
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
