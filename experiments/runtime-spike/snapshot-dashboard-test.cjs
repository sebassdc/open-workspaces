// Fixed API fixture: error display, failed capture controls, and deliberate vs uncertain retries.
// OW_PLAYWRIGHT_MODULE=/path/to/playwright node snapshot-dashboard-test.cjs OUTPUT_DIR
const {chromium} = require(process.env.OW_PLAYWRIGHT_MODULE || 'playwright');
const assert = require('node:assert/strict');
const fs = require('node:fs'), path = require('node:path'), http = require('node:http');
const {execFileSync} = require('node:child_process');
const root = path.resolve(__dirname, '../..'), ui = path.join(root, 'crates/ow/ui');
const output = path.resolve(process.argv[2]); fs.mkdirSync(output, {recursive:true});
const before = execFileSync('git', ['show', `${process.env.OW_BASE_REF || 'v0.7.3'}:crates/ow/ui/app.js`], {cwd:root});
const cause = 'storage headroom exhausted: 72000000000 bytes available, 73000000000 bytes required';
let baseline = false, failure = 'failed', requests = [];
const state = () => ({workspaces:[{id:'demo',state:'running',image:'ubuntu',memory_mib:4096,vcpu_count:2,node:'node-a'}], shared:[],
  snapshots:[{name:'saved',state:baseline?'pending':'failed',node:'node-a'}],stats:{},nodes:[]});
const server = http.createServer(async (req,res) => {
  const send = value => {res.setHeader('content-type','application/json');res.end(JSON.stringify(value));};
  if (req.url === '/api/state') return send({ok:true,result:state()});
  if (req.url === '/api/operation') {
    let text=''; for await (const chunk of req) text+=chunk;
    requests.push(JSON.parse(text)); res.statusCode=409;
    return send({ok:false,error:baseline?'Operation did not complete. Check its status before retrying; use the same retry key.':cause,
      operation:{id:17,state:failure,retry_key:requests.at(-1).operation_key}});
  }
  const name = req.url === '/' ? 'index.html' : req.url.slice(1);
  if (!/^(index.html|app.js|style.css|vendor\/[\w.-]+)$/.test(name)) {res.statusCode=404;return res.end();}
  res.setHeader('content-type',name.endsWith('.js')?'application/javascript':name.endsWith('.css')?'text/css':'text/html');
  res.end(name==='app.js' && baseline ? before : fs.readFileSync(path.join(ui,name)));
});
(async()=>{
  await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
  const browser=await chromium.launch({channel:process.env.OW_BROWSER_CHANNEL || 'chrome',headless:true});
  try {
    const page=await browser.newPage(); const errors=[];page.on('pageerror',e=>errors.push(e.message));
    const url=`http://127.0.0.1:${server.address().port}`;
    for (const mode of ['before','after']) {
      baseline=mode==='before';failure='failed';
      for (const [label,width,height] of [['desktop',1440,1000],['narrow',390,844]]) {
        requests=[];await page.setViewportSize({width,height});await page.goto(url);
        await page.locator('[data-machine=demo]').waitFor();
        await page.getByRole('button',{name:'Snapshot',exact:true}).click();
        await page.locator('[name=name]').fill('saved');
        const submit=page.locator('#dialog-submit');await submit.click();
        await page.locator('#dialog-error').waitFor();await submit.waitFor({state:'visible'});
        await page.screenshot({path:path.join(output,`${mode}-${label}.png`),fullPage:true});
        const text=await page.locator('#dialog-error').textContent();
        assert(text.includes(baseline?'Operation did not complete':cause));assert(text.includes('Operation 17'));
        await submit.click();await page.waitForFunction(()=>!document.querySelector('#dialog-submit').disabled);
        assert.equal(requests.length,2);
        assert.equal(requests[0].operation_key===requests[1].operation_key,baseline);
        await page.locator('#cancel-dialog').click();
        await page.locator('[data-view=snapshots]').click();
        assert.equal(await page.locator('#snapshot-list button').count(),baseline?2:0);
        if (!baseline) assert((await page.locator('#snapshot-list').textContent()).includes('Capture failed'));
        assert(await page.evaluate(()=>document.documentElement.scrollWidth<=window.innerWidth));
      }
    }
    // A lost/uncertain reply must retain the original key, including across reload.
    failure='uncertain';requests=[];await page.goto(url);
    await page.getByRole('button',{name:'Snapshot',exact:true}).click();await page.locator('[name=name]').fill('uncertain');
    await page.locator('#dialog-submit').click();await page.waitForFunction(()=>!document.querySelector('#dialog-submit').disabled);
    await page.reload();await page.getByRole('button',{name:'Snapshot',exact:true}).click();await page.locator('[name=name]').fill('uncertain');
    await page.locator('#dialog-submit').click();await page.waitForFunction(()=>!document.querySelector('#dialog-submit').disabled);
    assert.equal(requests.length,2);assert.equal(requests[0].operation_key,requests[1].operation_key);
    assert.deepEqual(errors,[]);
    fs.writeFileSync(path.join(output,'result.json'),JSON.stringify({passed:true,base_ref:process.env.OW_BASE_REF || 'v0.7.3',checks:['safe cause and operation ID','definitive failure retries with a new key','uncertainty survives reload with the same key','incomplete captures have no restore/fork actions','desktop/narrow before/after']},null,2));
  } finally {await browser.close();server.close();}
})().catch(e=>{console.error(e);process.exitCode=1;server.close();});
