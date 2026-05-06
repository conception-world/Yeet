"use strict";

const esbuild = require("esbuild");

const watch = process.argv.includes("--watch");

const ctx = {
  entryPoints: ["src/extension.ts"],
  bundle: true,
  outfile: "out/extension.js",
  platform: "node",
  target: "node18",
  format: "cjs",
  sourcemap: true,
  external: ["vscode"],
  logLevel: "info",
};

async function run() {
  if (watch) {
    const context = await esbuild.context(ctx);
    await context.watch();
    return;
  }
  await esbuild.build(ctx);
}

run().catch((err) => {
  console.error(err);
  process.exit(1);
});
