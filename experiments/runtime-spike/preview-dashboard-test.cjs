// Same-fixture desktop/narrow UI proof; no real VM or live identity claims.
// OW_PLAYWRIGHT_MODULE=/path/to/playwright node preview-dashboard-test.cjs OUTPUT
const {chromium} = require(process.env.OW_PLAYWRIGHT_MODULE || 'playwright');
const {readFileSync,mkdirSync,writeFileSync}=require('node:fs');
const path=require('node:path'),http=require('node:http'),assert=require('node:assert/strict');
const {execFileSync}=require('node:child_process');
const root=path.resolve(__dirname,'../..'),out=path.resolve(process.argv[2]);mkdirSync(out,{recursive:true});
let before=false,ports=[],requests=[];
const state=()=>({previews:true,user:{email:'owner@example.test'},workspaces:[{id:'demo',state:'running',memory_mib:1024,vcpu_count:1,published_ports:ports}],shared:[{id:'friend@example.test/shared',owner:'friend@example.test',role:'admin',state:'running',memory_mib:512,published_ports:[{port:3000,https_port:11001,url:'https://host.example-net.ts.net:11001/'}]}],snapshots:[],stats:{},nodes:[]});
const server=http.createServer(async(req,res)=>{
  const json=value=>{res.setHeader('content-type','application/json');res.end(JSON.stringify({ok:true,result:value}));};
  if(req.url==='/api/state')return json(state());
  if(req.url==='/api/operation'){
    let body='';for await(const chunk of req)body+=chunk;const op=JSON.parse(body);requests.push(op);
    if(op.op==='publish'){ports=[{port:op.port,https_port:11000,url:'https://host.example-net.ts.net:11000/'}];return json(ports[0]);}
    if(op.op==='unpublish'){ports=[];return json({published_ports:[]});}
    res.statusCode=400;return json({});
  }
  const file=req.url==='/'?'index.html':req.url.slice(1);
  if(!/^(index.html|app.js|style.css|vendor\/[\w.-]+)$/.test(file)){res.statusCode=404;return res.end();}
  res.setHeader('content-type',file.endsWith('.js')?'application/javascript':file.endsWith('.css')?'text/css':'text/html');
  res.end(before?execFileSync('git',['show',`${process.env.OW_BASE_REF||'v0.7.3'}:crates/ow/ui/${file}`],{cwd:root}):readFileSync(path.join(root,'crates/ow/ui',file)));
});
(async()=>{
  await new Promise(r=>server.listen(0,'127.0.0.1',r));const browser=await chromium.launch({channel:'chrome',headless:true});
  try{
    const page=await browser.newPage(),errors=[];page.on('pageerror',e=>errors.push(e.message));
    for(const mode of ['before','after']){before=mode==='before';ports=[];
      for(const [label,width,height]of [['desktop',1440,1000],['narrow',390,844]]){
        await page.setViewportSize({width,height});await page.goto(`http://127.0.0.1:${server.address().port}`);await page.locator('[data-machine=demo]').waitFor();
        await page.screenshot({path:path.join(out,`${mode}-${label}.png`),fullPage:true});
        assert(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
      }
    }
    await page.getByRole('button',{name:'Publish port',exact:true}).click();
    await page.locator('[name=port]').fill('0');assert.equal(await page.locator('[name=port]').evaluate(e=>e.checkValidity()),false);
    await page.locator('[name=port]').fill('3000');await page.locator('#dialog-submit').click();await page.locator('#operation-dialog').waitFor({state:'hidden'});
    assert.equal(requests.at(-1).port,3000);assert.equal(requests.at(-1).op,'publish');
    for(const [label,width,height]of [['desktop',1440,1000],['narrow',390,844]]){
      await page.setViewportSize({width,height});await page.screenshot({path:path.join(out,`published-${label}.png`),fullPage:true});assert(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
    }
    assert.equal(await page.getByRole('link',{name:'Open preview'}).getAttribute('href'),'https://host.example-net.ts.net:11000/');
    assert.equal(await page.getByRole('link',{name:'Open preview'}).getAttribute('rel'),'noopener noreferrer');
    await page.getByRole('button',{name:'Unpublish',exact:true}).click();await page.getByRole('link',{name:'Open preview'}).waitFor({state:'hidden'});assert.equal(requests.at(-1).op,'unpublish');
    await page.locator('[data-machine="friend@example.test/shared"]').click();assert.equal(await page.getByRole('button',{name:'Publish port',exact:true}).count(),0);assert.equal(await page.getByRole('button',{name:'Unpublish',exact:true}).count(),0);assert.equal(await page.getByRole('link',{name:'Open preview'}).getAttribute('href'),'https://host.example-net.ts.net:11001/');
    assert.deepEqual(errors,[]);writeFileSync(path.join(out,'result.json'),JSON.stringify({passed:true,requests,errors},null,2));console.log('Preview dashboard: publish, unpublish, shared access, port validation and desktop/narrow layout passed.');
  }finally{await browser.close();server.close();}
})().catch(e=>{console.error(e);process.exit(1);});
