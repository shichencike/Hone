// hone_lib/render/render_js.js - Hone 爬虫库渲染后端（node + puppeteer / playwright）
// 用法: node render_js.js <url>
// 输出: 单行 JSON {"status":200,"headers":{...},"url":"...","html":"..."}
// 安装: npm i puppeteer  或  npm i playwright && npx playwright install chromium
// 说明: 自动探测 puppeteer -> playwright，二选一即可运行。

const url = process.argv[2];
if (!url) {
    console.error("usage: node render_js.js <url>");
    process.exit(1);
}

(async () => {
    let browser = null;
    try {
        // 优先 puppeteer，其次 playwright
        let puppeteer = null;
        let playwright = null;
        try { puppeteer = require("puppeteer"); } catch (e) { /* 未安装 */ }
        try { playwright = require("playwright"); } catch (e) { /* 未安装 */ }

        if (!puppeteer && !playwright) {
            console.error("render_js.js: 未安装 puppeteer/playwright，请运行: npm i puppeteer");
            process.exit(1);
        }

        const timeout = 30000;
        let page = null;
        let resp = null;

        if (puppeteer) {
            browser = await puppeteer.launch({
                headless: "new",
                args: ["--no-sandbox", "--disable-setuid-sandbox", "--disable-dev-shm-usage"]
            });
            page = await browser.newPage();
            await page.setUserAgent(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36"
            );
            resp = await page.goto(url, { waitUntil: "networkidle2", timeout });
        } else {
            const pw = playwright;
            browser = await pw.chromium.launch({ args: ["--no-sandbox", "--disable-dev-shm-usage"] });
            page = await browser.newPage();
            resp = await page.goto(url, { waitUntil: "networkidle", timeout });
        }

        const html = await page.content();
        const finalUrl = page.url();
        const status = resp && resp.status ? resp.status() : 0;
        const headers = resp && resp.headers ? resp.headers() : {};

        // 单行 JSON 输出到 stdout（sys.run 会合并 stderr，务必只把 JSON 打到 stdout）
        process.stdout.write(JSON.stringify({ status, headers, url: finalUrl, html }) + "\n");
    } catch (e) {
        console.error("render_js.js: 渲染失败: " + (e && e.message ? e.message : String(e)));
        process.exit(1);
    } finally {
        if (browser) { try { await browser.close(); } catch (e) {} }
    }
})();
