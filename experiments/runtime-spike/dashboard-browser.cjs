// Authenticated dashboard browser regression against its real VM worker.
const {chromium} = require('../../data/dashboard-tests/node_modules/playwright');
const fs = require('node:fs');
const https = require('node:https'), http = require('node:http'), net = require('node:net');
const {execFileSync} = require('node:child_process');
const assert = require('node:assert/strict');
const input = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
(async () => {
  const keyPath = input.artifacts+'/tls-key.pem', certPath = input.artifacts+'/tls-cert.pem';
  execFileSync('openssl',['req','-x509','-newkey','rsa:2048','-nodes','-keyout',keyPath,'-out',certPath,'-days','1','-subj','/CN=localhost','-addext','subjectAltName=IP:127.0.0.1','-addext','basicConstraints=critical,CA:FALSE'],{stdio:'ignore'});
  function edge(req) {
    if (req.headers['cf-access-token']===input.assertion) req.headers['cf-access-jwt-assertion']=input.assertion;
    delete req.headers['cf-access-token'];
  }
  const upstream = new URL(input.upstream), publicUrl = new URL(input.url);
  const proxy = https.createServer({key:fs.readFileSync(keyPath),cert:fs.readFileSync(certPath)},(req,res) => {
    edge(req);
    const target = http.request({host:upstream.hostname,port:upstream.port,path:req.url,method:req.method,headers:req.headers}, response => {res.writeHead(response.statusCode,response.headers);response.pipe(res);});
    target.on('error',()=>{res.writeHead(502);res.end();});req.pipe(target);
  });
  proxy.on('upgrade',(req,socket,head) => {
    edge(req);
    const peer = net.connect({host:upstream.hostname,port:upstream.port},()=> {
      peer.write(`${req.method} ${req.url} HTTP/1.1\r\n`+Object.entries(req.headers).map(([k,v])=>`${k}: ${v}\r\n`).join('')+'\r\n');
      if (head.length) peer.write(head); socket.pipe(peer); peer.pipe(socket);
    });
    socket.on('error',()=>peer.destroy());peer.on('error',()=>socket.destroy());socket.on('close',()=>peer.destroy());peer.on('close',()=>socket.destroy());
  });
  await new Promise(resolve => proxy.listen(Number(publicUrl.port),'127.0.0.1',resolve));
  const browser = await chromium.launch({executablePath:'/usr/bin/chromium',headless:true});
  try {
    const context = await browser.newContext({viewport:{width:1440,height:1000},ignoreHTTPSErrors:true,extraHTTPHeaders:{'cf-access-jwt-assertion':input.assertion}});
    const page = await context.newPage();
    page.on('websocket', ws => ws.on('socketerror', error => console.error('WebSocket:',error)));
    const errors = []; page.on('pageerror', error => errors.push(error.message));
    await page.goto(input.url);
    await page.getByText('Your first machine is one click away.',{exact:false}).waitFor();
    async function submit(label) {
      await page.locator('#dialog-submit').click();
      await page.locator('#operation-dialog').waitFor({state:'hidden'});
    }
    async function run(command, contains) {
      await page.locator('#clear-console').click();
      await page.locator('#command').fill(command);
      await page.locator('#run-command').click();
      await page.waitForFunction(text => document.getElementById('console-output').textContent.includes(text),contains);
      await page.waitForFunction(() => !document.getElementById('run-command').disabled);
      const output = await page.locator('#console-output').textContent();
      assert.ok(output.slice(output.indexOf('\n') + 1).includes(contains), output);
    }
    await page.locator('#create').click();
    await page.locator('[name=id]').fill('browser-parent');
    await submit();
    await page.locator('[data-machine=browser-parent]').waitFor();
    await new Promise((resolve,reject) => require('node:child_process').execFile('python',[require('node:path').resolve(__dirname,'remote-cli-test.py'),process.argv[2],certPath],{timeout:60000},(error,stdout,stderr)=>{if(error) reject(Error(stdout+stderr));else {console.log(stdout);resolve();}}));
    await run('echo persisted-parent > /persist/browser; cat /persist/browser','persisted-parent');
    await page.getByRole('button',{name:'Open Terminal',exact:true}).click();
    try {await page.getByText('Connected',{exact:true}).waitFor({timeout:15000});} catch(error) {throw Error(await page.locator('#terminal-status').textContent() + ': ' + error.message);}
    const terminalText = () => page.evaluate(() => {
      let text=''; for (let i=0; i<terminalView.buffer.active.length; i++) text+=terminalView.buffer.active.getLine(i).translateToString(true)+'\n'; return text;
    });
    async function terminalInput(text) {
      for (const [index,part] of text.split('\r').entries()) {if (index) await page.keyboard.press('Enter');if (part) await page.keyboard.type(part);}
    }
    async function terminalWait(marker) {
      try {await page.waitForFunction(marker => {
        let text=''; for (let i=0; i<terminalView.buffer.active.length; i++) text+=terminalView.buffer.active.getLine(i).translateToString(true)+'\n'; return text.includes(marker);
      },marker,{timeout:10000});} catch(error) {await page.screenshot({path:input.artifacts+'/terminal-failure.png'});throw Error(await terminalText() + '\n' + error.message);}
    }
    await terminalInput("test -t 0 && test -t 1 && printf 'PTY_%s\\n' 'READY'\r");
    await terminalWait('PTY_READY');
    await terminalInput("cd /persist; export OW_SESSION=alive; printf 'STATE_%s_%s\\n' \"$PWD\" \"$OW_SESSION\"\r");
    await terminalWait('STATE_/persist_alive');
    await page.evaluate(() => terminalSocket.send(JSON.stringify({type:'resize',cols:103,rows:31})));
    await terminalInput('stty size\r'); await terminalWait('31 103');
    await terminalInput('sleep 30\r');
    await page.waitForTimeout(200);
    await page.evaluate(() => terminalSocket.send(new Uint8Array([3])));
    await terminalInput("printf 'INTERRUPT_%s\\n' 'OK'\r"); await terminalWait('INTERRUPT_OK');
    await terminalInput('vi /persist/terminal-edit\r');
    await page.waitForTimeout(300);
    await terminalInput('iNative guest PTY');
    await page.keyboard.press('Escape'); await page.waitForTimeout(300);
    await terminalInput(':wq\r');
    await page.waitForTimeout(300);
    await terminalInput("cat /persist/terminal-edit; printf 'EDITOR_%s\\n' 'OK'\r");
    await terminalWait('EDITOR_OK'); assert.ok((await terminalText()).includes('Native guest PTY'));
    // A management command must still work while the independent PTY is attached.
    const concurrent = await page.evaluate(async () => (await (await fetch('/api/operation',{method:'POST',headers:{'content-type':'application/json','x-ow-request':'dashboard'},body:JSON.stringify({op:'exec',id:'browser-parent',command:'echo control-still-ready'})})).json()));
    assert.equal(concurrent.result.output.trim(),'control-still-ready');
    await terminalInput('sleep 30\r'); await page.waitForTimeout(200); await page.keyboard.press('Control+z');
    await terminalInput('fg\r'); await page.waitForTimeout(200); await page.keyboard.press('Control+c');
    await terminalInput("printf 'JOBS_%s\\n' 'OK'\r"); await terminalWait('JOBS_OK');
    await terminalInput('yes BOUNDED_OUTPUT\r'); await page.waitForTimeout(400); await page.keyboard.press('Control+c');
    await terminalInput("printf 'FLOW_%s\\n' 'OK'\r"); await terminalWait('FLOW_OK');
    await page.setViewportSize({width:390,height:844}); await page.waitForTimeout(150);
    const dimensions = await page.evaluate(() => `${terminalView.rows} ${terminalView.cols}`);
    await terminalInput('stty size\r'); await terminalWait(dimensions);
    await page.screenshot({path:input.artifacts+'/terminal-mobile.png'});
    await page.setViewportSize({width:1440,height:1000}); await page.waitForTimeout(100);
    await page.screenshot({path:input.artifacts+'/terminal-desktop.png'});
    await terminalInput('exit 42\r'); await page.getByText('Shell exited · 42',{exact:true}).waitFor();
    await page.locator('#terminal-close').click();
    // Explicit disconnect terminates foreground/background jobs from this terminal session.
    await page.getByRole('button',{name:'Open Terminal',exact:true}).click();
    await page.getByText('Connected',{exact:true}).waitFor();
    await terminalInput('sleep 300 & echo $! > /persist/pty-background; sleep 300\r');
    await page.waitForTimeout(200); await page.locator('#terminal-close').click(); await page.waitForTimeout(250);
    await run('test ! -d /proc/$(cat /persist/pty-background) && echo session-cleaned','session-cleaned');
    await run('cat /persist/terminal-edit','Native guest PTY');
    await run('/http-fixture >/persist/http.log 2>&1 & sleep .1; echo service-ready','service-ready');
    await run('wget -qO- http://127.0.0.1:8080','Process counter: 1');
    await page.getByRole('button',{name:'Snapshot',exact:true}).click();
    await page.locator('[name=name]').fill('browser-checkpoint');
    await submit();
    await run('echo newer-parent > /persist/browser; cat /persist/browser','newer-parent');
    await run('wget -qO- http://127.0.0.1:8080','Process counter: 2');
    await page.getByRole('button',{name:'Fork',exact:true}).click();
    await page.locator('[name=child]').fill('browser-child');
    await page.locator('[name=snapshot]').selectOption('browser-checkpoint');
    await submit();
    await page.waitForFunction(() => document.getElementById('selected-name').textContent==='browser-child');
    await run('wget -qO- http://127.0.0.1:8080','Process counter: 2');
    await run('cat /persist/browser','persisted-parent');
    await run('echo independent-child > /persist/browser; cat /persist/browser','independent-child');
    await page.locator('[data-machine=browser-parent]').click();
    await run('cat /persist/browser','newer-parent');
    await run('wget -qO- http://127.0.0.1:8080','Process counter: 3');
    await page.getByRole('button',{name:'Hibernate',exact:true}).click();
    await page.getByRole('button',{name:'Resume machine',exact:true}).waitFor();
    await page.getByRole('button',{name:'Resume machine',exact:true}).click();
    await page.getByRole('button',{name:'Hibernate',exact:true}).waitFor();
    await run('wget -qO- http://127.0.0.1:8080','Process counter: 4');
    await run('cat /persist/browser','newer-parent');
    await page.locator('[data-view=snapshots]').click();
    const snapshot = page.locator('.snapshot-row').filter({hasText:'browser-checkpoint'});
    await snapshot.getByRole('button',{name:'Restore',exact:true}).click();
    await submit();
    await page.locator('[data-view=machines]').click();
    await page.locator('#clear-console').click();
    await run('cat /persist/browser','persisted-parent');
    await run('wget -qO- http://127.0.0.1:8080','Process counter: 2');
    await run('printf "<img src=x onerror=window.owInjected=1>"','<img src=x');
    assert.equal(await page.evaluate(() => window.owInjected),undefined);
    assert.equal(await page.locator('#console-output img').count(),0);
    await page.locator('#clear-console').click();
    await run('exit 7','[exit 7]');
    await page.screenshot({path:input.artifacts+'/dashboard-desktop.png',fullPage:true});
    await page.setViewportSize({width:390,height:844});
    await page.screenshot({path:input.artifacts+'/dashboard-mobile.png',fullPage:true});
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth<=window.innerWidth),true);
    await page.locator('[data-view=usage]').click();
    await page.getByText('Make room for your next idea.').waitFor();
    await page.locator('[data-view=machines]').click();
    for (const image of ['arch','ubuntu']) {
      await page.locator('#create').click(); await page.locator('[name=id]').fill(`${image}-browser`);
      await page.locator('[name=image]').selectOption(image); await submit();
      await page.waitForFunction(id => document.getElementById('selected-name').textContent===id,`${image}-browser`);
      await run('cat /etc/os-release',`ID=${image}`);
      await page.getByRole('button',{name:'Open Terminal',exact:true}).click();
      await page.getByText('Connected',{exact:true}).waitFor();
      await terminalInput(`test -t 0 && . /etc/os-release; printf 'PROFILE_%s\\n' "$ID"\r`);
      await terminalWait(`PROFILE_${image}`);
      await terminalInput('exit 42\r'); await page.getByText('Shell exited · 42',{exact:true}).waitFor();
      await page.locator('#terminal-close').click();
    }
    assert.deepEqual(errors,[]);
    const result={passed:true,checks:['remote HTTPS CLI list/exec and TLS verification','remote CLI WSS PTY, resize, Ctrl-C and exit status','browser create/exec','real guest PTY and persistent shell state','terminal resize and mobile fit','Ctrl-C and Ctrl-Z/fg job control','interactive vi editing','bounded heavy output remains interruptible','shell exit status 42','disconnect cleans session jobs','management exec while PTY attached','paired memory/disk snapshot','fork memory preservation and disk independence','hibernate/resume preserves process memory','restore checkpoint rewinds memory and disk','guest output is text, not HTML','nonzero guest exit status','mobile layout has no horizontal overflow','resource usage view','Arch and Ubuntu UI image selection and WSS PTY','no browser JavaScript errors']};
    fs.writeFileSync(input.artifacts+'/dashboard-result.json',JSON.stringify(result,null,2));
    console.log(JSON.stringify(result));
  } finally { await browser.close(); proxy.close(); }
})().catch(error=>{console.error(error);process.exitCode=1;});
