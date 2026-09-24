import pw from "../../node_modules/playwright-core/index.js";
const { chromium } = pw;
import http from "node:http"; import fs from "node:fs"; import path from "node:path";
const ROOT="tools/visual-harness/dist";
const MIME={".html":"text/html",".js":"text/javascript",".css":"text/css"};
const server=http.createServer((q,r)=>{const u=decodeURIComponent(q.url.split("?")[0]);const p=path.join(ROOT,u==="/"?"index.html":u);
fs.readFile(p,(e,b)=>{if(e){r.writeHead(404);r.end();return;}r.writeHead(200,{"Content-Type":MIME[path.extname(p)]??"application/octet-stream"});r.end(b);});});
await new Promise(r=>server.listen(0,r)); const port=server.address().port;
const browser=await chromium.launch({executablePath:"/opt/google/chrome/chrome",args:["--no-sandbox"]});
for (const w of [250, 300, 352]) {
  const page=await browser.newPage({viewport:{width:w,height:800},deviceScaleFactor:2});
  await page.goto(`http://127.0.0.1:${port}/?lang=zh&mode=modal&theme=mica&colorMode=light`,{waitUntil:"networkidle"});
  await page.waitForTimeout(700);
  const r=await page.evaluate((vw)=>{
    const modal=document.querySelector("[data-backup-list-modal]");
    const rows=Array.from(document.querySelectorAll("[data-backup-row]")).map(el=>({
      sw:el.scrollWidth, cw:el.clientWidth, w:+el.getBoundingClientRect().width.toFixed(1),
      over: el.scrollWidth > el.clientWidth + 1 }));
    // 对话框与列表容器是否横向溢出
    const list=document.querySelector("[data-backup-list]");
    return { vw, docOverflow: document.documentElement.scrollWidth > document.documentElement.clientWidth,
      modal:{w:+modal.getBoundingClientRect().width.toFixed(1), sw:modal.scrollWidth, cw:modal.clientWidth},
      list:{sw:list.scrollWidth, cw:list.clientWidth},
      rows, anyRowOverflow: rows.some(r=>r.over) };
  }, w);
  console.log(`viewport ${w}: doc溢出=${r.docOverflow} modal(w=${r.modal.w} sw=${r.modal.sw} cw=${r.modal.cw}) list(sw=${r.list.sw} cw=${r.list.cw}) 行溢出=${r.anyRowOverflow}`);
  for (const x of r.rows) if (x.over) console.log(`   行溢出: sw=${x.sw} cw=${x.cw}`);
  await page.close();
}
await browser.close(); server.close();
