"use strict";

const esbuild = require("esbuild");
const packageJson = require("./package.json");

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
  define: {
    // Textually replaces `__YEET_CLIENT_VERSION__` (declared ambient
    // in src/websocket.ts) with the quoted package.json version at
    // bundle time, so the wire-protocol handshake version can never
    // drift from the extension's own release version again.
    __YEET_CLIENT_VERSION__: JSON.stringify(packageJson.version),
  },
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
