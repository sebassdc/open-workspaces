#!/usr/bin/env python3
"""Publish only the fixed platform CLI artifacts on the existing project hostname."""
import importlib.util,json
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('access',ROOT/'scripts/cloudflare-access.py');access=importlib.util.module_from_spec(spec);spec.loader.exec_module(access)
private=access.PRIVATE
parent=json.loads(access.STATE.read_text());hostname=parent['hostname']
context=access.credential();token=access.access_token()
app=access.api(token,access.account_path(context,'apps/'+parent['app_id']))
if app.get('domain')!=hostname:raise RuntimeError('Managed dashboard hostname changed')
state_path=private/'cli-access.json'
state=json.loads(state_path.read_text()) if state_path.exists() else None
path=hostname+'/cli/*'
apps=access.api(token,access.account_path(context,'apps')+'?per_page=1000') or []
matches=[a for a in apps if a.get('domain')==path]
if matches and (len(matches)!=1 or not state or matches[0]['id']!=state['app_id']):raise RuntimeError('Refusing to adopt an unrelated Access application')
if not matches:
    if state:raise RuntimeError('Previously managed CLI application disappeared')
    public=access.api(token,access.account_path(context,'apps'),'POST',{'name':'Open Workspaces CLI Downloads','type':'self_hosted','domain':path,'app_launcher_visible':False,'policies':[{'name':'Public CLI artifacts only','decision':'bypass','include':[{'everyone':{}}],'exclude':[],'require':[],'precedence':1}]})
    state={'app_id':public['id'],'hostname':hostname,'path':'/cli/*'};access.private_write(state_path,state)
policies=access.api(token,access.account_path(context,'apps/'+state['app_id']+'/policies')) or []
if len(policies)!=1 or policies[0].get('decision')!='bypass' or policies[0].get('include')!=[{'everyone':{}}]:raise RuntimeError('Unexpected public CLI policy; inspect before changing the tunnel')
config_path=private/'tunnel.json';config=json.loads(config_path.read_text())
old_regex=r'^/cli/(install\.sh|ow-linux-amd64(\.sha256)?)$'
regex=r'^/cli/(install\.sh|ow-(linux-amd64|darwin-(arm64|amd64))(\.sha256)?)$'
route={'hostname':hostname,'path':regex,'service':'http://127.0.0.1:8787','originRequest':{'access':{'required':False}}}
old_route={'hostname':hostname,'path':old_regex,'service':'http://127.0.0.1:8787','originRequest':{'access':{'required':False}}}
old=[i for i in config['ingress'] if i.get('hostname')==hostname and i.get('path')==old_regex]
if old:
    if old!=[old_route]:raise RuntimeError('Previous CLI ingress changed; refusing to replace it')
    config['ingress'].remove(old_route)
owned=[i for i in config['ingress'] if i.get('hostname')==hostname and i.get('path')==regex]
if not owned:
    backup=private/'tunnel-before-public-cli.json'
    if not backup.exists():access.private_write(backup,config)
    config['ingress'].insert(0,route);access.private_write(config_path,config)
elif owned!=[route]:raise RuntimeError('CLI ingress changed; inspect before editing')
print('Public CLI application and exact artifact ingress configured. Dashboard/API/terminal gate retained.')
