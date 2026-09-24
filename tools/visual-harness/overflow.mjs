import pw from "/root/Tiez-Next/node_modules/playwright-core/index.js";
const { chromium } = pw;
import http from "node:http"; import fs from "node:fs"; import path from "node:path";
const ROOT="tools/visual-harness/dist";
const MIME={".html":"text/html",".js":"text/javascript",".css":"text/css"};
const server=http.createServer((q,r)=>{const u=decodeURIComponent(q.url.split("?")[0]);const p=path.join(ROOT,u==="/"?"index.html":u);
fs.readFile(p,(e,b)=>{if(e){r.writeHead(404);r.end();return;}r.writeHead(200,{"Content-Type":MIME[path.extname(p)]??"application/octet-stream"});r.end(b);});});
await new Promise(r=>server.listen(0,r)); const port=server.address().port;
const browser=await chromium.launch({executablePath:"/opt/google/chrome/chrome",args:["--no-sandbox"]});
for (const w of [250, 300, 352, 420]) {
  const page=await browser.newPage({viewport:{width:w,height:900},deviceScaleFactor:2});
  await page.goto(`http://127.0.0.1:${port}/?lang=zh&mode=groups&theme=mica&colorMode=light`,{waitUntil:"networkidle"});
  await page.waitForTimeout(700);
  const r=await page.evaluate((vw)=>{
    const doc=document.documentElement;
    const groups=Array.from(document.querySelectorAll(".settings-group")).map(g=>({
      t:(g.querySelector("h3")?.textContent??"").slice(0,8),
      w:+g.getBoundingClientRect().width.toFixed(1),
      scrollW:g.scrollWidth, clientW:g.clientWidth,
      right:+g.getBoundingClientRect().right.toFixed(1)}));
    // 找出所有"比视口更宽"的行内元素（真实横向溢出源）
    const over=[];
    document.querySelectorAll(".settings-group *").forEach(el=>{
      const b=el.getBoundingClientRect();
      if (b.right > vw + 0.5 && b.width>0) over.push({cls:(el.className||"").toString().slice(0,30), tag:el.tagName, right:+b.right.toFixed(1), w:+b.width.toFixed(1)});
    });
    return {vw, docScrollW:doc.scrollWidth, docClientW:doc.clientWidth, groups, overflow:over.slice(0,6)};
  }, w);
  console.log(`\n=== viewport ${w} ===  doc scrollW=${r.docScrollW} clientW=${r.docClientW} ${r.docScrollW>r.docClientW?"<<< 横向溢出":""}`);
  for (const g of r.groups) console.log(`   group ${g.t} w=${g.w} scrollW=${g.scrollW} clientW=${g.clientW}`);
  if (r.overflow.length) { console.log("   溢出元素："); for (const o of r.overflow) console.log(`     ${o.tag}.${o.cls} right=${o.right} w=${o.w}`); }
  await page.close();
}
await browser.close(); server.close();
