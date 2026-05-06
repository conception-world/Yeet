"use strict";

const fs = require("fs");
const path = require("path");
const os = require("os");

const target = path.join(os.homedir(), ".antigravity", "extensions", "extensions.json");
const raw = fs.readFileSync(target, "utf8");
const data = JSON.parse(raw);
const before = data.length;
const filtered = data.filter((e) => !(e.identifier && e.identifier.id === "yeet.yeet-vscode"));
const backup = target + ".bak-" + Date.now();
fs.writeFileSync(backup, raw);
fs.writeFileSync(target, JSON.stringify(filtered));
console.log("backup:", backup);
console.log("before:", before, "after:", filtered.length, "removed:", before - filtered.length);
