import { rm } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
for (const relative of ["dist", "media/viewer"]) {
  const target = path.resolve(root, relative);
  if (!target.startsWith(`${root}${path.sep}`)) {
    throw new Error(`refusing unsafe clean target: ${target}`);
  }
  await rm(target, { recursive: true, force: true });
}
