// Records the README's demo video: demo/agents.py runs for real against Tokenrein, its output is
// shown in a terminal window, and a person acts in the real dashboard. Run through demo/run.sh:
//   NODE_PATH=<dir with playwright> demo/run.sh node record.cjs <output.mp4>
"use strict";
const { chromium } = require("playwright");
const { spawn, execFileSync } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const ADMIN = "http://127.0.0.1:19788";
const STAGE = `${ADMIN}/__stage`;
const output = path.resolve(process.argv[2] || "demo.mp4");
const frames = fs.mkdtempSync(path.join(os.tmpdir(), "tokenrein-demo-"));
const sleep = ms => new Promise(r => setTimeout(r, ms));

const STAGE_HTML = `<!doctype html><meta charset="utf-8"><style>
*{box-sizing:border-box} html,body{margin:0;height:100%;overflow:hidden;background:#0b0e14;font-family:-apple-system,"Segoe UI",Inter,sans-serif}
#cap{position:absolute;inset:0 0 auto 0;height:60px;display:flex;align-items:center;gap:14px;padding:0 24px;background:#10141d;border-bottom:1px solid #232a38;color:#f2f5fa;font-size:23px;font-weight:650;letter-spacing:-.2px;z-index:5}
#chip{display:none;min-width:32px;height:32px;border-radius:9px;background:#f5a524;color:#111;font-size:18px;font-weight:800;align-items:center;justify-content:center}
#dash{position:absolute;left:0;right:0;top:60px;bottom:0;width:100%;height:calc(100% - 60px);border:0;background:#0b0e14}
#term{position:absolute;z-index:3;left:18px;right:18px;top:78px;bottom:18px;background:#0d1117;border:1px solid #2c3444;border-radius:12px;box-shadow:0 18px 50px rgba(0,0,0,.55);display:flex;flex-direction:column;overflow:hidden;pointer-events:none;transition:all .45s cubic-bezier(.2,.8,.2,1)}
#term.pip{left:auto;top:auto;right:16px;bottom:16px;width:660px;height:232px}
#term.gone{opacity:0;transform:translateY(24px);pointer-events:none}
#bar{height:30px;flex:none;display:flex;align-items:center;gap:7px;padding:0 12px;background:#161b26;color:#8a94a7;font-size:13px}
#bar i{width:11px;height:11px;border-radius:50%;display:block} #bar span{margin-left:8px}
#out{flex:1;overflow:hidden;padding:12px 16px;display:flex;flex-direction:column;justify-content:flex-end;font:18px/1.5 ui-monospace,Menlo,monospace;color:#d7dce5;white-space:pre}
#term.pip #out{font-size:13px;line-height:1.4;padding:8px 12px}
.b{font-weight:700}.d{opacity:.58}.r{color:#ff6b6b}.g{color:#4ade80}.y{color:#f5a524}.c{color:#56c8f5}.m{color:#c792ea}
#card{position:absolute;inset:0;z-index:8;background:#0b0e14;color:#f2f5fa;display:flex;flex-direction:column;align-items:center;justify-content:center;gap:16px;text-align:center;transition:opacity .4s}
#card.hide{opacity:0;pointer-events:none}
#card h1{font-size:50px;margin:0;letter-spacing:-1.2px;font-weight:750}#card h1 em{font-style:normal;color:#f5a524}
#card p{font-size:23px;margin:0;color:#aab3c4}
#card code{font:21px ui-monospace,Menlo,monospace;background:#161b26;border:1px solid #2c3444;border-radius:10px;padding:12px 20px;color:#4ade80}
#card small{font-size:15px;color:#7d8798}
#cursor{position:absolute;z-index:9;left:640px;top:400px;width:26px;height:26px;pointer-events:none;transition:left .7s cubic-bezier(.3,.9,.3,1),top .7s cubic-bezier(.3,.9,.3,1);filter:drop-shadow(1px 2px 3px rgba(0,0,0,.7));opacity:0}
</style>
<div id="cap"><span id="chip"></span><span id="captext"></span></div>
<iframe id="dash" src="/"></iframe>
<div id="term" class="gone"><div id="bar"><i style="background:#ff5f57"></i><i style="background:#febc2e"></i><i style="background:#28c840"></i><span>agents</span></div><div id="out"></div></div>
<div id="card" class="hide"></div>
<svg id="cursor" viewBox="0 0 24 24"><path d="M5 3L19 12L12 13L9 20L5 3Z" fill="#fff" stroke="#000" stroke-width="1.4" stroke-linejoin="round"/></svg>
<script>
const CL={"1":"b","2":"d","31":"r","32":"g","33":"y","36":"c","35":"m"};
function ansi(s){let open=0,h="";for(const p of s.split(/(\\x1b\\[\\d+m)/)){const m=p.match(/^\\x1b\\[(\\d+)m$/);
 if(m){if(m[1]==="0"){h+="</span>".repeat(open);open=0}else{h+='<span class="'+CL[m[1]]+'">';open++}}
 else h+=p.replace(/&/g,"&amp;").replace(/</g,"&lt;")}return h+"</span>".repeat(open)}
window.addLine=s=>{const o=document.getElementById("out"),d=document.createElement("div");d.innerHTML=ansi(s)||"&nbsp;";o.appendChild(d);while(o.children.length>40)o.firstChild.remove()};
window.caption=(n,t)=>{const c=document.getElementById("chip");c.style.display=n?"flex":"none";c.textContent=n||"";document.getElementById("captext").textContent=t};
window.term=m=>{document.getElementById("term").className=m;document.getElementById("dash").style.visibility=m===""?"hidden":"visible"};
window.card=h=>{const c=document.getElementById("card");if(h){c.innerHTML=h;c.className=""}else c.className="hide"};
window.cursorTo=(x,y)=>{const c=document.getElementById("cursor");c.style.opacity=1;c.style.left=x+"px";c.style.top=y+"px"};
window.cursorOff=()=>{document.getElementById("cursor").style.opacity=0};
</script>`;

const INTRO = `<h1>Your agents spend money <em>while you sleep.</em></h1><p>Tokenrein keeps a hand on the reins.</p>`;
const OUTRO = `<h1>Token<em>rein</em></h1><p>Budgets, loop guard and approvals for every AI agent. Self-hosted.</p>
<code>./setup.sh &amp;&amp; docker compose up -d</code><p style="font-size:20px">tokenrein.com &nbsp;·&nbsp; free up to 25 developers</p>
<small>Recorded live against a mock model: real Tokenrein, no real spend.</small>`;

let page;
(async () => {
  const browser = await chromium.launch();
  const context = await browser.newContext({ viewport: { width: 1280, height: 720 }, deviceScaleFactor: 2, colorScheme: "dark" });
  // The stage shows the real dashboard in a frame, which its security headers refuse: lift only
  // that header, in this browser only.
  await context.route(`${ADMIN}/**`, async route => {
    if (route.request().url() === STAGE) return route.fulfill({ contentType: "text/html", body: STAGE_HTML });
    const res = await route.fetch();
    const headers = { ...res.headers() };
    delete headers["content-security-policy"];
    delete headers["x-frame-options"];
    return route.fulfill({ response: res, headers });
  });
  const login = await context.request.post(`${ADMIN}/auth/login`, { data: { email: process.env.DEMO_EMAIL, password: process.env.DEMO_PASSWORD } });
  if (!login.ok()) throw new Error(`sign-in failed: ${login.status()}`);

  page = await context.newPage();
  page.on("response", r => { if (r.status() >= 400) console.error(r.status(), r.url()); });
  page.on("dialog", d => d.accept());
  page.on("pageerror", e => console.error("page error:", e.message));
  await page.goto(STAGE);
  const dash = page.frameLocator("#dash");
  await dash.locator('button[data-tab="approvals"]').waitFor();
  // Before the camera rolls: the first-run checklist out of the way, and Approvals open behind
  // the terminal.
  const hide = dash.getByRole("button", { name: "Hide", exact: true });
  await hide.click({ timeout: 5000 }).catch(() => console.error("no Get started checklist to hide"));
  await dash.locator('button[data-tab="approvals"]').click();

  const cdp = await context.newCDPSession(page);
  const shots = [];
  cdp.on("Page.screencastFrame", async f => {
    const file = path.join(frames, `${String(shots.length).padStart(6, "0")}.jpg`);
    shots.push({ file, t: f.metadata.timestamp });
    fs.writeFileSync(file, Buffer.from(f.data, "base64"));
    await cdp.send("Page.screencastFrameAck", { sessionId: f.sessionId }).catch(() => {});
  });
  await page.evaluate(h => window.card(h), INTRO);
  await cdp.send("Page.startScreencast", { format: "jpeg", quality: 92, everyNthFrame: 1 });
  await sleep(2800);

  const lines = [];
  const seen = text => lines.some(l => l.includes(text));
  const until = async (text, ms = 60000) => {
    const end = Date.now() + ms;
    while (!seen(text)) {
      if (Date.now() > end) throw new Error(`timed out waiting for "${text}"`);
      await sleep(50);
    }
  };
  const caption = (n, t) => page.evaluate(([n, t]) => window.caption(n, t), [n, t]);
  const add = s => page.evaluate(x => window.addLine(x), s);
  const term = m => page.evaluate(x => window.term(x), m);

  const clickIn = async (locator, label) => {
    await locator.waitFor({ timeout: 20000 });
    const box = await locator.boundingBox();
    if (!box) throw new Error(`no box for ${label}`);
    await page.evaluate(([x, y]) => window.cursorTo(x, y), [box.x + box.width / 2, box.y + box.height / 2]);
    await sleep(850);
    await locator.click();
  };
  const tab = async name => clickIn(dash.locator(`button[data-tab="${name}"]`), name);

  await page.evaluate(() => window.card(""));
  await caption("1", "Over budget in Check mode: a warning, nothing breaks");
  await term("");
  await sleep(500);
  await add("\x1b[32m$\x1b[0m python3 demo/agents.py");
  await sleep(500);

  const agents = spawn("python3", ["-u", "agents.py"], { env: { ...process.env, DEMO_PAUSE: "4.5" } });
  let tail = "";
  agents.stdout.on("data", chunk => {
    tail += chunk;
    const parts = tail.split("\n");
    tail = parts.pop();
    for (const l of parts) {
      lines.push(l);
      add(l).catch(() => {});
    }
  });
  agents.stderr.on("data", d => console.error(String(d)));

  await until("TOKEN WARNING");
  await sleep(2600);
  await until("nightly-summary");
  await until("✓ finished");
  await caption("2", "A run hits its $1 budget: it pauses and asks a person");
  await until("PAUSED");
  await sleep(1400);
  await caption("3", "An agent stuck in a loop gets caught");
  await until("LOOP DETECTED");
  await sleep(2600);

  await caption("4", "A person decides, in the console");
  await term("pip");
  const budget = dash.locator('[data-approval="ticket-4812"]');
  const loop = dash.locator('[data-approval="research-77"]');
  await budget.waitFor({ timeout: 20000 });
  await loop.waitFor({ timeout: 20000 });
  await sleep(2400);
  await clickIn(budget.locator('button[type="submit"]'), "approve");
  await until("approved, the run continues", 20000);
  await sleep(2200);
  await clickIn(loop.locator("[data-deny]"), "stop");
  await until("KILLED", 20000);
  await sleep(2400);

  await caption("5", "Enforce mode: a hard limit is a hard limit");
  await page.evaluate(() => window.cursorOff());
  await term("");
  await until("BLOCKED", 60000);
  await sleep(3400);

  await caption("6", "Every run, agent and dollar in one console");
  await term("gone");
  await tab("overview");
  await sleep(3000);
  await tab("runs");
  await sleep(1800);
  await clickIn(dash.locator('a[data-open-run="ticket-4812"]:visible, a:has-text("ticket-4812"):visible').first(), "open run");
  await sleep(3200);
  await page.keyboard.press("Escape");
  await sleep(500);
  await tab("spend");
  await sleep(2800);
  await tab("audit");
  await sleep(3000);

  await page.evaluate(() => window.cursorOff());
  await page.evaluate(h => window.card(h), OUTRO);
  await sleep(4800);
  await cdp.send("Page.stopScreencast");
  agents.kill();
  await browser.close();

  const list = shots.map((s, i) => {
    const next = shots[i + 1]?.t ?? s.t + 0.5;
    return `file '${s.file}'\nduration ${Math.max(next - s.t, 0.001).toFixed(4)}`;
  });
  fs.writeFileSync(path.join(frames, "list.txt"), `${list.join("\n")}\nfile '${shots.at(-1).file}'\n`);
  execFileSync("ffmpeg", ["-y", "-loglevel", "error", "-f", "concat", "-safe", "0", "-i", path.join(frames, "list.txt"),
    "-vf", "fps=30,scale=1920:1080:flags=lanczos,format=yuv420p", "-c:v", "libx264", "-crf", "20", "-preset", "slow", "-movflags", "+faststart", output], { stdio: "inherit" });
  fs.rmSync(frames, { recursive: true, force: true });
  console.log(`saved ${output}`);
})().catch(async err => {
  console.error(err);
  await page?.screenshot({ path: `${output}.failed.png` }).catch(() => {});
  process.exit(1);
});
