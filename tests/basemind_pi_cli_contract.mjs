import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { execFileSync } from "node:child_process";
const source = readFileSync(new URL("../.pi/extensions/basemind.ts", import.meta.url), "utf8");
const recipes = [...source.matchAll(/basemind (code|git|query) ([a-z-]+)/gu)].map((match) => match.slice(1));
assert.equal(recipes.length, 7, "Expected all seven navigation recipes");
for (const [domain, mode] of recipes) {
  execFileSync("basemind", [domain, mode, "--help"], { stdio: "pipe" });
}
console.log(`Actual Basemind CLI accepted ${recipes.length}/${recipes.length} Pi navigation recipes`);
