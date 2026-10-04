// No VM: exercise real dashboard forms against a synthetic HTTP API.
const {chromium}=require(process.env.OW_PLAYWRIGHT_MODULE);
const http=require('node:http'),fs=require('node:fs'),path=require('node:path'),assert=require('node:assert/strict');
const ui=path.resolve(__dirname,'../../crates/ow/ui');
let state={workspaces:[],snapshots:[],stats:{limit_memory_mib:8192},host_controls:true,nodes:[{id:'friend',online:true,memory_mib:512,owner_limits:{memory:512,slots:2,cpus:2}}]};
const requests=[];
const server=http.createServer(async(req,res)=>{
  if(req.url.startsWith('/api/')) {
    let result=state;
    if(req.method==='POST') {
      let text='';for await(const chunk of req)text+=chunk;
      const body=JSON.parse(text);requests.push({path:req.url,body});
      if(req.url==='/api/operation') {
        assert.equal(typeof body.memory_mib,'number');assert.equal(typeof body.vcpu_count,'number');
        result={...body,state:'running'};state.workspaces.push(result);
      } else {
        for(const key of ['memory','slots','cpus']) assert.equal(typeof body[key],'number');
        assert.equal(body.node,'friend');assert.ok(!('ttl' in body));result=body;
      }
    }
    res.setHeader('content-type','application/json');res.end(JSON.stringify({ok:true,result}));return;
  }
  if(req.url==='/favicon.ico') {res.writeHead(204);res.end();return;}
  const filename=req.url==='/'?'index.html':req.url.slice(1);
  assert.ok(!filename.includes('..'));
  res.setHeader('content-type',filename.endsWith('.js')?'text/javascript':filename.endsWith('.css')?'text/css':'text/html');
  res.end(fs.readFileSync(path.join(ui,filename)));
});
(async()=>{
  await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
  const browser=await chromium.launch({executablePath:'/usr/bin/chromium',headless:true});
  try {
    const page=await browser.newPage();const errors=[];page.on('pageerror',e=>errors.push(e.message));
    await page.goto(`http://127.0.0.1:${server.address().port}`);
    await page.locator('#host-controls').waitFor({state:'visible'});
    for(const [name,memory,cpus] of [['default',256,1],['larger',4096,2]]) {
      await page.locator('#create').click();await page.locator('[name=id]').fill(name);
      await page.locator('[name=memory_mib]').selectOption(String(memory));await page.locator('[name=vcpu_count]').selectOption(String(cpus));
      await page.locator('#dialog-submit').click();await page.locator('#operation-dialog').waitFor({state:'hidden'});
      assert.equal(requests.at(-1).body.memory_mib,memory);assert.equal(requests.at(-1).body.vcpu_count,cpus);
    }
    await page.locator('#host-node-list button').click();
    await page.locator('#host-form [name=memory]').fill('8192');await page.locator('#host-form [name=cpus]').fill('8');
    await page.locator('#host-submit').click();await page.locator('#host-dialog').waitFor({state:'hidden'});
    assert.equal(requests.at(-1).path,'/api/hosts/budget');assert.equal(requests.at(-1).body.memory,8192);
    assert.deepEqual(errors,[]);
    console.log(JSON.stringify({passed:true,evidence:'real Chromium forms; synthetic API, no VM',requests}));
  } finally {await browser.close();server.close();}
})().catch(e=>{console.error(e);server.close();process.exitCode=1;});
