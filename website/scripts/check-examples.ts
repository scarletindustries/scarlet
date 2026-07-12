#!/usr/bin/env bun
// Type-check every AL snippet on the site against the real compiler.
//
// Run via `bun run check`, locally or in CI beside a compiler build — NOT on
// Vercel, which has no Rust toolchain and so no `al` binary. The docs are the
// language's whole reference, so an example that no longer parses or types is
// a broken promise; the gate lives where the compiler lives. Snippets that are DELIBERATELY wrong (showing a compile
// error) set `check: false`; snippets marked `plain` are shell/output, not AL.
//
// Each snippet is written into its own scratch dir alongside any `files` it
// declares (so `import ./util` resolves), then run through `al check`.

import { examples } from "../src/app.tsx";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";

// Resolve the `al` binary the site is built against. `AL_BIN` overrides for
// CI; the default is the release build in the repo checkout beside `website/`.
const AL =
  process.env.AL_BIN ??
  join(import.meta.dir, "..", "..", "target", "release", "al");

let failed = 0;
let checked = 0;
let skipped = 0;

for (const ex of examples) {
  if (ex.check === false) {
    skipped++;
    continue;
  }

  const dir = await mkdtemp(join(tmpdir(), "al-site-"));
  try {
    for (const [name, src] of Object.entries(ex.files ?? {})) {
      await writeFile(join(dir, name), src);
    }
    const entry = join(dir, `${ex.id}.al`);
    await writeFile(entry, ex.code + "\n");

    const { status, stderr, stdout, error } = spawnSync(AL, ["check", entry], {
      encoding: "utf-8",
    });
    checked++;
    if (status !== 0) {
      failed++;
      console.error(`\n✗ [${ex.id}] ${ex.title}`);
      // Print the source with line numbers so the diagnostic's spans read.
      ex.code.split("\n").forEach((l, i) => console.error(`  ${i + 1} | ${l}`));
      // `stderr`/`stdout` are null when the spawn itself failed (no binary).
      console.error(
        (stderr || stdout || error?.message || `could not run ${AL}`).trimEnd(),
      );
    }
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}

console.error(
  `\n${failed === 0 ? "✓" : "✗"} ${checked} checked, ${failed} failed, ${skipped} skipped`,
);
process.exit(failed === 0 ? 0 : 1);
