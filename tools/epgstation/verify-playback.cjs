// Decode an existing recording through EPGStation's actual MP4 playback route.
const { chromium } = require('playwright-core');
const fs = require('fs');
(async () => {
    if (!process.argv[2] || !process.argv[3]) throw Error('Usage: verify-playback.cjs RECORD_ID VIDEO_FILE_ID [output-directory]');
    const recordId = Number(process.argv[2]);
    const videoId = Number(process.argv[3]);
    const root = process.argv[4] || 'evidence/20261016-epgstation/';
    fs.mkdirSync(root, { recursive: true });
    const browser = await chromium.launch({ executablePath: '/usr/bin/chromium', headless: true,
        args: ['--no-sandbox', '--autoplay-policy=no-user-gesture-required'] });
    const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
    const errors = [];
    let state, failure;
    page.on('pageerror', e => errors.push(String(e)));
    page.on('response', r => { if (r.url().includes('/api/streams')) console.log(r.status(), r.url()); });
    try {
        await page.goto(`${process.env.EPGSTATION_URL || "http://127.0.0.1:8888"}/#/recorded/streaming/${videoId}?recordedId=${recordId}&streamingType=mp4&mode=1`,
            { waitUntil: 'domcontentloaded', timeout: 30000 });
        await page.locator('video').waitFor({ timeout: 30000 });
        await page.waitForFunction(() => {
            const v = document.querySelector('video');
            if (!v) return false;
            v.muted = true;
            if (v.paused) v.play().catch(() => {});
            return !!v.error || (v.currentTime >= 5 && v.videoWidth > 0);
        }, null, { timeout: Number(process.env.TEST_PLAYBACK_TIMEOUT_MS || 120000) });
    } catch (e) { failure = String(e); }
    try {
        state = await page.locator('video').evaluate(v => ({ currentTime: v.currentTime,
            readyState: v.readyState, networkState: v.networkState, width: v.videoWidth, height: v.videoHeight,
            paused: v.paused, buffered: Array.from({ length: v.buffered.length }, (_, i) => [v.buffered.start(i), v.buffered.end(i)]),
            error: v.error && { code: v.error.code, message: v.error.message } }));
        if (state.error || state.currentTime < 5 || !state.width) failure ||= 'Video did not decode five seconds';
        await page.screenshot({ path: root + 'browser-playback.png' });
    } catch (e) { failure ||= String(e); }
    finally {
        fs.writeFileSync(root + 'browser-playback.json', JSON.stringify({ recordId, videoId, state, errors, failure }, null, 2));
        await browser.close();
    }
    console.log(state);
    if (failure) throw Error(failure);
})().catch(e => { console.error(e); process.exitCode = 1; });
