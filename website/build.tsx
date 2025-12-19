#!/usr/bin/env bun
import { renderToStaticMarkup } from "react-dom/server";
import { codeToHtml } from "shiki";
import { App, examples } from "./src/app";
import grammar from "./src/al.tmLanguage.json";
import { existsSync } from "fs";
import { rm, mkdir } from "fs/promises";
import path from "path";
import plugin from "bun-plugin-tailwind";

const outdir = path.join(import.meta.dir, "dist");

if (existsSync(outdir)) {
  await rm(outdir, { recursive: true, force: true });
}
await mkdir(outdir);

console.log("Rendering code blocks with shiki...");

const renderedExamples = await Promise.all(
  examples.map(async (example) => ({
    title: example.title,
    description: example.description,
    light: await codeToHtml(example.code, {
      lang: grammar as any,
      theme: "github-light",
    }),
    dark: await codeToHtml(example.code, {
      lang: grammar as any,
      theme: "github-dark",
    }),
  }))
);

console.log("Rendering HTML...");

const body = renderToStaticMarkup(<App examples={renderedExamples} />);

console.log("Building CSS with Tailwind...");

// Build with plugin to scan source files for Tailwind classes
const result = await Bun.build({
  entrypoints: [path.join(import.meta.dir, "src/index.html")],
  outdir,
  minify: true,
  plugins: [plugin],
});

// Find the CSS output
const cssFile = result.outputs.find((o) => o.path.endsWith(".css"));
const css = cssFile ? await Bun.file(cssFile.path).text() : "";

const html = `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>AL - a small programming language</title>
<style>${css}</style>
</head>
<body class="bg-white dark:bg-neutral-950 text-black dark:text-white">
${body}
</body>
</html>`;

await Bun.write(path.join(outdir, "index.html"), html);

// Clean up other build outputs
for (const output of result.outputs) {
  if (!output.path.endsWith("index.html")) {
    await rm(output.path).catch(() => {});
  }
}

console.log("Done! Output: dist/index.html");
