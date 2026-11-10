// Pre-render country flags from the COLRv1 flag font in scripts/fonts: node scripts/flags.mjs [outdir] [CODE...]
// Needs `source ~/.local/share/playwright/env.sh` and `env -u WAYLAND_DISPLAY`.
import { mkdirSync, readFileSync } from "node:fs";
import { chromium } from "/home/user/.local/lib/node_modules/playwright/index.mjs";

const CODES = "AE AM AT AU BE BG BR BY CA CH CZ DE DK EE ES FI FR GB GE GR HK HU IE IL IN IS IT JP KR KZ LT LU LV MD NL NO PL PT RO RS RU SE SG TR UA US".split(" ");
// Box styles mirror the web: 24 = `.country-emoji grid size-6 place-items-center` (Flag), 16 = `text-base leading-none` (map chip), clipped to the box.
const SIZES = {
  24: "display:grid;place-items:center;width:24px;height:24px;font-size:20px;line-height:24px",
  16: "width:16px;height:16px;font-size:16px;line-height:16px;overflow:hidden",
};

const [, , out = new URL("../assets/flags", import.meta.url).pathname, ...args] = process.argv;
const codes = args.length ? args : CODES;
const font = readFileSync(new URL("./fonts/country-flags.woff2", import.meta.url)).toString("base64");
mkdirSync(out, { recursive: true });
const browser = await chromium.launch({ args: ["--ozone-platform=headless"] });
for (const scale of [1, 2]) {
  const page = await browser.newPage({ deviceScaleFactor: scale });
  await page.setContent(`<style>
@font-face { font-family: "Route Flag Emoji"; src: url(data:font/woff2;base64,${font}) format("woff2"); unicode-range: U+1F1E6-1F1FF; }
html, body { margin: 0; background: transparent; }
#flag { font-family: "Route Flag Emoji", sans-serif; }
</style><div id="flag"></div>`);
  for (const [size, css] of Object.entries(SIZES)) {
    for (const code of codes) {
      const glyph = String.fromCodePoint(...[...code.toUpperCase()].map((c) => 0x1f1a5 + c.charCodeAt(0)));
      await page.evaluate(([css, glyph]) => {
        const el = document.getElementById("flag");
        el.style.cssText = css;
        el.textContent = glyph;
        return document.fonts.ready;
      }, [css, glyph]);
      await page.locator("#flag").screenshot({ path: `${out}/${code.toLowerCase()}-${size}@${scale}x.png`, omitBackground: true });
    }
  }
  await page.close();
}
await browser.close();
