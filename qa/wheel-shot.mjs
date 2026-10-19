#!/usr/bin/env node
/**
 * QA screenshot: serve the production frontend and photograph the
 * sun-disc wheel open over a dropped (simulated) file, using the app's
 * built-in demo mode (#demo-wheel). Run: node qa/wheel-shot.mjs
 */
import { spawn } from "node:child_process";
import { createRequire } from "node:module";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
// Reuse the Playwright install from the sibling Uruk Cut project.
const require2 = createRequire(join(ROOT, "..", "uruk-cut", "package.json"));
const { chromium } = require2("playwright");

const PORT = process.env.QA_PORT ?? "4173";
const server = process.env.QA_PORT
  ? { kill() {} }
  : spawn("npx", ["vite", "preview", "--port", PORT], { cwd: ROOT, stdio: "ignore" });
const wait = (ms) => new Promise((r) => setTimeout(r, ms));

try {
  await wait(1800);
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1120, height: 780 } });
  await page.goto("http://localhost:4178/#demo-wheel", { waitUntil: "networkidle" });
  await page.waitForSelector(".wheel-wrap", { timeout: 10_000 });
  await wait(650); // let the spring entrance settle
  await page.screenshot({ path: join(ROOT, "qa", "wheel-open.png") });
  const home = await browser.newPage({ viewport: { width: 1120, height: 780 } });
  await home.goto("http://localhost:4178/", { waitUntil: "networkidle" });
  await wait(500);
  await home.screenshot({ path: join(ROOT, "qa", "home.png") });
  await browser.close();
  console.log("screenshots written to qa/");
} finally {
  server.kill();
}
