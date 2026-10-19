// Fixed-fixture desktop/narrow proof, separate from live worker/store acceptance.
// OW_PLAYWRIGHT_MODULE=/path/to/playwright node recovery-dashboard-test.cjs OUTPUT_DIR
const {chromium} = require(process.env.OW_PLAYWRIGHT_MODULE || 'playwright');
const fs = require('node:fs'), path = require('node:path'), http = require('node:http');
const {execFileSync} = require('node:child_process'), {createHash} = require('node:crypto');
const assert = require('node:assert/strict');
const root = path.resolve(__dirname, '../..'), ui = path.join(root, 'crates/ow/ui');
const output = path.resolve(process.argv[2]); fs.mkdirSync(output, {recursive:true});
const beforeRef = process.env.OW_BASE_REF || '5ae9871';
const before = name => execFileSync('git',['show',`${beforeRef}:crates/ow/ui/${name}`],{cwd:root});
const now = 1792324800; // 2026-10-18T12:00:00Z
const retained = 'latest local point plus pending; off-host points kept until explicit GC';
const fixture = {user:{email:'owner@example.test'},host_controls:false,workspaces:[
  {id:'protected',durability:{last_durable_at:now-900,retention:retained}},
  {id:'overdue',durability:{last_durable_at:now-7200,retention:retained}},
  {id:'retrying',durability:{last_durable_at:now-1800,error:'Upload failed; retry scheduled.',pending:'recovery-pending',retry_at:now+120,retention:retained}},
  {id:'first-upload',durability:{pending:'recovery-new',retention:retained}},
  {id:'unconfigured'},
  {id:'sleep-protected',state:'hibernated',durability:{last_durable_at:now-86400,current_state_durable:true,retention:retained}}
].map(item=>({state:'running',image:'ubuntu',memory_mib:1024,vcpu_count:1,disk_bytes:32*1073741824,disk_used_bytes:2*1073741824,...item})),snapshots:[],stats:{},nodes:[]};
let baseline=false;
const server=http.createServer((req,res)=>{
  if(req.url==='/api/state'){res.setHeader('content-type','application/json');return res.end(JSON.stringify({ok:true,result:fixture}));}
  const name=req.url==='/'?'index.html':req.url.slice(1);
  if(!/^(index.html|app.js|style.css|vendor\/[\w.-]+)$/.test(name)){res.statusCode=404;return res.end();}
  res.setHeader('content-type',name.endsWith('.js')?'application/javascript':name.endsWith('.css')?'text/css':'text/html');
  res.end(baseline && ['app.js','style.css'].includes(name)?before(name):fs.readFileSync(path.join(ui,name)));
});
(async()=>{
  await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
  const browser=await chromium.launch({channel:process.env.OW_BROWSER_CHANNEL || 'chrome',headless:true});
  try {
    const page=await browser.newPage({timezoneId:'UTC'}),errors=[];
    page.on('pageerror',error=>errors.push(error.message));
    await page.clock.install({time:new Date(now*1000)});
    for(const mode of ['before','after']){
      baseline=mode==='before';
      for(const [label,width,height] of [['desktop',1440,1000],['narrow',390,844]]){
        await page.setViewportSize({width,height});await page.goto(`http://127.0.0.1:${server.address().port}`);
        await page.locator('[data-machine=protected]').waitFor();
        if(!baseline){
          assert.match(await page.locator('[data-machine=protected]').textContent(),/Last durable.*11:45/);
          assert.match(await page.locator('[data-machine=overdue]').textContent(),/one-hour disk recovery target exceeded/);
          assert.match(await page.locator('[data-machine=retrying]').textContent(),/Upload failed; retry scheduled.*retry 12:02/);
          assert.match(await page.locator('[data-machine=first-upload]').textContent(),/No durable recovery point yet.*upload pending/);
          assert.match(await page.locator('[data-machine=unconfigured]').textContent(),/Off-host recovery status unavailable/);
          assert.equal(await page.locator('[data-machine=overdue] .recovery-warning').count(),1);
          assert.equal(await page.locator('[data-machine=protected] .recovery-warning').count(),0);
          assert.equal(await page.locator('[data-machine=sleep-protected] .recovery-warning').count(),0);
          assert(!/target exceeded/.test(await page.locator('[data-machine=sleep-protected]').textContent()));
        }
        assert(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth),'horizontal overflow');
        await page.screenshot({path:path.join(output,`${mode}-${label}.png`),fullPage:true});
      }
    }
    assert.deepEqual(errors,[]);
    fs.writeFileSync(path.join(output,'result.json'),JSON.stringify({passed:true,before:beforeRef,fixture,errors,ui_sha256:createHash('sha256').update(fs.readFileSync(path.join(ui,'app.js'))).digest('hex')},null,2));
    console.log('PASS: durable time, overdue, failure/retry, pending and unavailable states; desktop/narrow; no overflow or page errors');
  } finally {await browser.close();server.close();}
})().catch(error=>{console.error(error);server.close();process.exitCode=1;});
