// Faux téléphone pour tester streame sans iPhone : lance Chromium avec une caméra et un micro
// synthétiques (mire animée + bip) sur la page « téléphone » de streame, et suit exactement le
// même parcours que l'appareil réel (getUserMedia + WebRTC via web/app.js).
//
// Prérequis : node + `npm i playwright` (utilise le Chromium déjà mis en cache par Playwright).
// Chromium n'a pas d'encodeur H264 : la négociation retombe sur VP8 (le vrai iPhone, lui, envoie
// du H264). decodebin décode les deux ; c'est surtout utile pour tester l'ICE, le signaling, le
// jitter buffer et la reconnexion.
//
// Usage :
//   URL=https://127.0.0.1:8444/  ITER=3  HOLD=8000  HEADLESS=1  node scripts/fake-phone.mjs
//
//   URL      page téléphone de streame (défaut https://127.0.0.1:8444/)
//   ITER     nombre de connexions successives (reproduction d'intermittences) — défaut 1
//   HOLD     durée de maintien de chaque connexion en ms — défaut 8000
//   HEADLESS 0 pour voir la fenêtre du navigateur — défaut 1
import { chromium } from 'playwright';

const URL = process.env.URL || 'https://127.0.0.1:8444/';
const ITER = parseInt(process.env.ITER || '1', 10);
const HOLD = parseInt(process.env.HOLD || '8000', 10);
const HEADLESS = process.env.HEADLESS !== '0';

const browser = await chromium.launch({
  headless: HEADLESS,
  args: [
    '--use-fake-device-for-media-stream', // caméra + micro synthétiques
    '--use-fake-ui-for-media-stream',     // autorise caméra/micro sans clic
    '--autoplay-policy=no-user-gesture-required',
  ],
});
const ctx = await browser.newContext({ ignoreHTTPSErrors: true }); // certificat auto-signé
await ctx.grantPermissions(['camera', 'microphone']);

for (let i = 1; i <= ITER; i++) {
  const page = await ctx.newPage();
  page.on('console', (m) => console.log(`  [page ${i}] ${m.type()}: ${m.text()}`));
  page.on('pageerror', (e) => console.log(`  [page ${i}] ERREUR JS: ${e.message}`));
  console.log(`\n=== connexion ${i}/${ITER} → ${URL} ===`);
  try {
    await page.goto(URL, { waitUntil: 'domcontentloaded', timeout: 15000 });
    await page.waitForSelector('#start', { state: 'visible', timeout: 10000 });
    await page.waitForTimeout(800); // laisse enumerateDevices + startPreview se faire
    await page.click('#start');
    await page.waitForSelector('#live:not([hidden])', { timeout: 8000 }).catch(() => {});
    const t0 = Date.now();
    while (Date.now() - t0 < HOLD) {
      await page.waitForTimeout(1000);
      const status = await page.evaluate(() => document.getElementById('live-status')?.textContent || '');
      console.log(`  [${i}] statut = "${status}"`);
    }
  } catch (e) {
    console.log(`  [${i}] échec : ${e.message}`);
  }
  await page.close();
}

await browser.close();
console.log('\nterminé.');
