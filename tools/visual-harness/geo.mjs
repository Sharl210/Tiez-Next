import pw from "/root/Tiez-Next/node_modules/playwright-core/index.js";
const { chromium } = pw;
import http from "node:http"; import fs from "node:fs"; import path from "node:path";
const ROOT="tools/visual-harness/dist";
const MIME={".html":"text/html",".js":"text/javascript",".css":"text/css"};
const server=http.createServer((q,r)=>{const u=decodeURIComponent(q.url.split("?")[0]);const p=path.join(ROOT,u==="/"?"index.html":u);
fs.readFile(p,(e,b)=>{if(e){r.writeHead(404);r.end();return;}r.writeHead(200,{"Content-Type":MIME[path.extname(p)]??"application/octet-stream"});r.end(b);});});
await new Promise(r=>server.listen(0,r)); const port=server.address().port;
const browser=await chromium.launch({executablePath:"/opt/google/chrome/chrome",args:["--no-sandbox"]});
const page=await browser.newPage({viewport:{width:352,height:900},deviceScaleFactor:2});
await page.goto(`http://127.0.0.1:${port}/?lang=zh&mode=groups&theme=mica&colorMode=light`,{waitUntil:"networkidle"});
await page.waitForTimeout(800);
const r=await page.evaluate(()=>{
  const info=(el)=>{const cs=getComputedStyle(el);const b=el.getBoundingClientRect();
    return {label:(el.textContent??"").trim().slice(0,12), radiusPx:parseFloat(cs.borderTopLeftRadius),
            h:+b.height.toFixed(1), w:+b.width.toFixed(1), textTransform:cs.textTransform, fontSize:cs.fontSize};};
  const mine=document.querySelector("[data-auto-backup-open-list]");
  const sameShape=Array.from(document.querySelectorAll("button.btn-icon"))
     .filter(b=>!b.hasAttribute("data-auto-backup-open-list") && (b.textContent??"").trim().length>2);
  return {mine:info(mine), others:sameShape.map(info)};
});
console.log(JSON.stringify(r,null,1));
// 每个可见按钮的高度 → 全项目一致性
console.log("\n=== 每个按钮 radius / height（CSS px）===");
console.log(JSON.stringify(r,null,1));
await browser.close(); server.close();
