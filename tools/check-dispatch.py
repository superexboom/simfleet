#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Replay an underfilled protocol-8 pack against a local server with a 60s gate.

Uses copied engine/cache files and synthetic credentials. Never contacts the
production task queue. The old client stays on one group; the fix fills its
configured second group without asking either identity before its deadline.
"""
import argparse,datetime,http.server,json,pathlib,secrets,shutil,subprocess,threading,time

def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--client',type=pathlib.Path,required=True);p.add_argument('--installed',type=pathlib.Path,required=True)
    p.add_argument('--out',type=pathlib.Path,required=True);p.add_argument('--expect-groups',type=int,default=2)
    a=p.parse_args();a.out.mkdir(parents=True,exist_ok=True)
    for folder in ('cache','engines'):(a.out/folder).mkdir(exist_ok=True)
    shutil.copy2(a.client,a.out/'simfleet.exe')
    state=json.loads((a.installed/'engine-state.json').read_text(encoding='utf8'))['current']
    for name in [f"{state['wasm']}.wasmtime49.cwasm",f"wfsim_wasm_bg.{state['wasm']}.wasm",f"pack.data.{state['data']}.json"]:
        shutil.copy2(a.installed/'cache'/name,a.out/'cache'/name)
    name=f"{state['engine']}-sfe1-v3.dll";shutil.copy2(a.installed/'engines'/name,a.out/'engines'/name)
    orders=json.loads((pathlib.Path(__file__).resolve().parents[1]/'experiments/prepared-board/orders.json').read_text());order=orders[9]
    ids={};grants={};next_at={};events=[];start=time.monotonic();first=[None];lock=threading.Lock()
    def event(kind,**fields):
        with lock:events.append(dict(event=kind,seconds=time.monotonic()-start,**fields))
    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self,*args):pass
        def send(self,data):
            raw=json.dumps(data).encode();self.send_response(200);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(raw)));self.end_headers()
            try:self.wfile.write(raw)
            except (ConnectionResetError,BrokenPipeError):pass
        def do_GET(self):
            if self.path=='/release.json':self.send(dict(state,protocols_served=[8]));return
            if self.path=='/work.json':
                event('signal');self.send(dict(v=1,at=datetime.datetime.now(datetime.timezone.utc).isoformat().replace('+00:00','Z'),engine=state['engine'],open=1000,urgent=0,poll_ms=1000,jitter_ms=0,urgent_jitter_ms=0));return
            self.send(dict(ok=True,points=0,account=None,contributors=[]))
        def do_POST(self):
            data=json.loads(self.rfile.read(int(self.headers.get('Content-Length','0'))));identity=data.get('verifier','none')
            if identity not in ids:ids[identity]=len(ids)+1
            group=ids[identity]
            if self.path=='/api/account/devices/claim':
                event('claim_start',group=group);time.sleep(1);event('claim_done',group=group);self.send(dict(ok=True));return
            if self.path!='/api/board/sync':self.send(dict(ok=True,points=0));return
            now=time.monotonic();want=data.get('want_ms',0);old=next_at.get(identity,0)
            early=want>0 and now<old-0.05
            event('sync',group=group,want_ms=want,early=early)
            reply=dict(ok=True,protocol=8,next_ms=max(0,int((old-now)*1000)),taken=[r['question'] for r in data.get('results',[])],dropped=[],held=[],pack=None,states=[],
                dials=dict(pack8_ms=600000,pack8_max=200,sync_floor_ms=60000,sync_low_ms=120000,flush_ms=600000,poll_ms=1000,jitter_ms=0,urgent_jitter_ms=0,report_ms=3600000))
            if early:reply['early']=True
            elif want:
                grants[identity]=grants.get(identity,0)+1;first[0]=first[0] or now;next_at[identity]=now+60
                lease=f'local-{group}-{grants[identity]}'
                reply.update(next_ms=60000,pack=dict(lease=lease,lease_ms=300000,tasks=[dict(order,kind='board',question=f'{lease}-{i}') for i in range(3)]))
            self.send(reply)
    server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler);threading.Thread(target=server.serve_forever,daemon=True).start()
    config=f'''server = "http://127.0.0.1:{server.server_port}"
verifier = "{secrets.token_hex(12)}"
consent_at = "2026-10-23T00:00:00.000Z"
consent_v = 3
idle_cores = 4
active_cores = 4
idle_after_secs = 60
pause_on_battery = false
priority = "normal"
native_engine = true
native_engine_url = ""
native_riven = false
official_engine = false
watch = false
slots = 0
max_slots = 2
gpu_enabled = false
protocol = 8
[account]
session = "local-test-only"
name = "local"
'''
    (a.out/'simfleet.toml').write_text(config,encoding='utf8')
    with (a.out/'client.log').open('w',encoding='utf8') as log:
        proc=subprocess.Popen([str((a.out/'simfleet.exe').resolve()),'--no-watch'],stdin=subprocess.PIPE,stdout=log,stderr=subprocess.STDOUT,text=True,creationflags=0x08000000)
        try:
            while time.monotonic()-start<45:
                if proc.poll() is not None:raise RuntimeError('client stopped')
                if first[0] and time.monotonic()-first[0]>23:break
                time.sleep(.1)
            proc.stdin.write('q\n');proc.stdin.flush();proc.wait(timeout=15)
        finally:
            if proc.poll() is None:proc.kill();proc.wait()
            server.shutdown();server.server_close()
    report=dict(granted_groups=len(grants),expected=a.expect_groups,early_requests=sum(e.get('early',False) for e in events),events=events)
    (a.out/'result.json').write_text(json.dumps(report,indent=2)+'\n')
    assert len(grants)==a.expect_groups,report
    assert not report['early_requests'],report
    if a.expect_groups>1:
        claim=next(e for e in events if e['event']=='claim_start')
        signal=next(e for e in events if e['event']=='signal')
        assert signal['seconds']-claim['seconds']<.5,'signal was blocked by account binding'
    print(json.dumps({k:v for k,v in report.items() if k!='events'}),flush=True)

if __name__=='__main__':main()
