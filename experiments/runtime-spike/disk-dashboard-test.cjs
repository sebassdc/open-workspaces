// UI regression with a fixed API fixture; does not claim real-VM acceptance.
// OW_PLAYWRIGHT_MODULE=/path/to/playwright node disk-dashboard-test.cjs OUTPUT_DIR
const {chromium} = require(process.env.OW_PLAYWRIGHT_MODULE || 'playwright');
const fs = require('node:fs'), path = require('node:path'), http = require('node:http');
const {execFileSync} = require('node:child_process');
const assert = require('node:assert/strict');
const root = path.resolve(__dirname, '../..'), ui = path.join(root, 'crates/ow/ui');
const output = path.resolve(process.argv[2]); fs.mkdirSync(output, {recursive:true});
const gib = 1073741824;
const fixture = () => ({user:{email:'owner@example.test'},workspaces:[
  {id:'legacy',state:'stopped',image:'ubuntu',memory_mib:1024,vcpu_count:2,disk_bytes:8*gib,disk_used_bytes:4*gib,node:'local'},
  {id:'running',state:'running',image:'ubuntu',memory_mib:1024,vcpu_count:1,disk_bytes:8*gib,disk_used_bytes:3*gib,node:'local'}],
  shared:[{id:'other@example.test/shared',owner:'other@example.test',role:'admin',state:'stopped',memory_mib:512,vcpu_count:1,disk_bytes:32*gib,disk_used_bytes:gib,node:'local'}],
  snapshots:[],stats:{},nodes:[{id:'local',online:true,capabilities:{disk_resize_v1:true}}]});
let baseline = false, state = fixture(); const requests = [];
const before = execFileSync('git', ['show','HEAD:crates/ow/ui/app.js'], {cwd:root});
const server = http.createServer(async (req,res) => {
  const send = value => {res.setHeader('content-type','application/json');res.end(JSON.stringify({ok:true,result:value}));};
  if (req.url === '/api/state') return send(state);
  if (req.url === '/api/operation') {
    let text=''; for await (const chunk of req) text+=chunk;
    const op=JSON.parse(text); requests.push(op); let result;
    if(op.op==='create') {result={id:op.id,state:'running',image:op.image,memory_mib:op.memory_mib,vcpu_count:op.vcpu_count,disk_bytes:op.disk_gib*gib,disk_used_bytes:gib};state.workspaces.push(result);}
    else if(op.op==='resize') {result=state.workspaces.find(w=>w.id===op.id);result.disk_bytes=op.disk_gib*gib;}
    else {res.statusCode=400;return send({});}
    return send(result);
  }
  const name = req.url === '/' ? 'index.html' : req.url.slice(1);
  if (!/^(index.html|app.js|style.css|vendor\/[\w.-]+)$/.test(name)) {res.statusCode=404; return res.end();}
  res.setHeader('content-type',name.endsWith('.js')?'application/javascript':name.endsWith('.css')?'text/css':'text/html');
  res.end(name==='app.js' && baseline ? before : fs.readFileSync(path.join(ui,name)));
});
(async()=>{
  await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
  const browser=await chromium.launch({channel:process.env.OW_BROWSER_CHANNEL || 'chrome',headless:true});
  const errors=[];
  try {
    const context=await browser.newContext(); const page=await context.newPage();
    page.on('pageerror',error=>errors.push(error.message));
    const url=`http://127.0.0.1:${server.address().port}`;
    for (const mode of ['before','after']) {
      baseline=mode==='before';state=fixture();
      for (const [label,width,height] of [['desktop',1440,1000],['narrow',390,844]]) {
        await page.setViewportSize({width,height}); await page.goto(url);
        await page.locator('[data-machine=legacy]').waitFor();
        await page.screenshot({path:path.join(output,`${mode}-${label}.png`),fullPage:true});
        assert(await page.evaluate(()=>document.documentElement.scrollWidth<=window.innerWidth));
      }
    }
    await page.setViewportSize({width:1440,height:1000});
    await page.getByRole('button',{name:'Resize',exact:true}).click();
    const disk=page.locator('[name=disk_gib]');
    assert.equal(await disk.inputValue(),'8'); assert.equal(await disk.getAttribute('min'),'8');
    await disk.fill('64'); await page.locator('#dialog-submit').click();
    await page.locator('#operation-dialog').waitFor({state:'hidden'});
    const resize=requests.at(-1);assert.equal(resize.op,'resize');assert.equal(resize.disk_gib,64);
    assert(!Object.hasOwn(resize,'memory_mib'));assert(!Object.hasOwn(resize,'vcpu_count'));
    assert.match(await page.locator('[data-machine=legacy]').textContent(),/64.0 GiB disk.*4.0 GiB used/);
    await page.locator('[data-machine=running]').click();assert.equal(await page.getByRole('button',{name:'Resize',exact:true}).count(),0);
    await page.locator('[data-machine="other@example.test/shared"]').click();assert.equal(await page.getByRole('button',{name:'Resize',exact:true}).count(),0);
    await page.locator('#create').click();assert.equal(await disk.inputValue(),'32');
    await page.locator('[name=image]').selectOption('ubuntu-arm64');assert(await disk.isDisabled());
    await page.locator('[name=image]').selectOption('ubuntu');assert(await disk.isEnabled());
    await page.locator('[name=id]').fill('larger');await disk.fill('64');
    for (const [label,width,height] of [['desktop',1440,1000],['narrow',390,844]]) {
      await page.setViewportSize({width,height});
      await page.screenshot({path:path.join(output,`create-${label}.png`)});
    }
    await page.locator('#dialog-submit').click();await page.locator('#operation-dialog').waitFor({state:'hidden'});
    assert.equal(requests.at(-1).disk_gib,64);assert.equal(requests.at(-1).op,'create');
    assert.deepEqual(errors,[]);
    fs.writeFileSync(path.join(output,'result.json'),JSON.stringify({passed:true,baseline:execFileSync('git',['rev-parse','HEAD'],{cwd:root,encoding:'utf8'}).trim(),fixture:'same fixed owner/stopped, owner/running, shared/admin boxes',requests,errors},null,2));
    console.log('PASS: create, disk-only resize, defaults, owner/state controls, Mac refusal, desktop/narrow rendering');
  } finally {await browser.close();server.close();}
})().catch(error=>{console.error(error);server.close();process.exitCode=1;});
