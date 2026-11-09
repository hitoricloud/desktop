// Screenshots of the TSX prototype for every state in scripts/states.txt: node web-states.mjs <outdir> [base-url]
import { readFileSync } from "node:fs";
import { chromium } from "/home/user/.local/lib/node_modules/playwright/index.mjs";

const [, , out, base = "http://localhost:5176/"] = process.argv;
const states = readFileSync(new URL("./states.txt", import.meta.url), "utf8").split("\n").filter((l) => l.trim() && !l.startsWith("#")).map((l) => l.split("|").map((s) => s.trim()));
const browser = await chromium.launch({ args: ["--ozone-platform=headless", "--use-angle=swiftshader", "--enable-unsafe-swiftshader"] });
for (const [name, size, , steps] of states) {
  const [width, height] = size.split("x").map(Number);
  const page = await browser.newPage({ viewport: { width, height } });
  for (const step of steps.split(";")) {
    const [op, ...rest] = step.split(":");
    const arg = rest.join(":");
    if (op === "url") { await page.goto(base + arg); await page.waitForTimeout(6000); }
    else if (op === "click") await page.getByRole("button", { name: arg, exact: true }).first().click();
    else if (op === "key") await page.keyboard.press(arg);
    else if (op === "wait") await page.waitForTimeout(Number(arg));
    else if (op === "nav") await page.locator("[data-sidebar=footer] button").click();
    else if (op === "item") await page.getByRole("menuitem", { name: arg }).click();
    else if (op === "rule") await page.locator("[data-sidebar=content] button", { hasText: arg }).first().click();
    else if (op === "type") await page.keyboard.type(arg);
  }
  await page.mouse.move(width - 1, height - 1);
  await page.waitForTimeout(1200);
  await page.screenshot({ path: `${out}/${name}.png` });
  await page.close();
}
await browser.close();
