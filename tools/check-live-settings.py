#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Offline settings/update integration using two real cached engine releases.

python tools/check-live-settings.py --exe target/release/simfleet.exe \
  --old-assets <cache-and-engines-dir> --new-assets <cache-and-engines-dir> --out <new-dir>
Does not request work from the public task server. Only this test's subprocess
and HTTP server are stopped. The fixed fixtures match this optimization series.
"""
import argparse
import datetime
import hashlib
import json
import pathlib
import shutil
import subprocess
import threading
import time
import tomllib
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer

OLD={'engine':'872e90ffaf0d','wasm':'3b94b29dbc45','data':'dd8982bee216','protocols':[8,7],'accepted':[]}
NEW={'engine':'9b3a5b500838','wasm':'abf3db1f5bbd','data':'b4ab1c0955c5','protocols_served':[8,7],'protocol':8}

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    for name in ['exe','old-assets','new-assets','out']:parser.add_argument('--'+name,type=pathlib.Path,required=True)
    args=parser.parse_args();args.out.mkdir(parents=True,exist_ok=False)
    current={**NEW,'accepted':[{'engine':OLD['engine'],'until':(datetime.datetime.now(datetime.timezone.utc)+datetime.timedelta(days=3)).isoformat().replace('+00:00','Z')}]}
    received=[];http_paths=[];lock=threading.Lock()
    class Handler(BaseHTTPRequestHandler):
        def log_message(self,*a):pass
        def answer(self,value):
            body=json.dumps(value).encode();self.send_response(200);self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
        def do_GET(self):
            http_paths.append(self.path)
            if self.path=='/release.json':self.answer(current)
            elif self.path=='/work.json':self.answer({'v':1,'at':datetime.datetime.now(datetime.timezone.utc).isoformat().replace('+00:00','Z'),'engine':NEW['engine'],'open':0,'urgent':0,'poll_ms':1000,'jitter_ms':0})
            elif self.path.startswith('/pkg/'):
                name=self.path.rsplit('/',1)[1];path=args.new_assets/'cache'/name
                if not path.is_file():self.send_error(404);return
                body=path.read_bytes();self.send_response(200);self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
            else:self.answer({'ok':True})
        def do_POST(self):
            body=json.loads(self.rfile.read(int(self.headers.get('Content-Length',0))) or '{}')
            if self.path=='/api/board/sync':
                with lock:received.append(body)
                self.answer({'ok':True,'engine':NEW['engine'],'accepted_until':current['accepted'][0]['until'] if current['accepted'] else '',
                             'protocol':8,'protocols_served':[8,7],'taken':[],'dropped':[],'states':[],'held':[],'next_ms':1000,'pack':None})
            else:self.answer({'ok':True})
    server=ThreadingHTTPServer(('127.0.0.1',0),Handler);threading.Thread(target=server.serve_forever,daemon=True).start()
    config=f'''server = "http://127.0.0.1:{server.server_port}"
verifier = "fedcba9876543210fedcba98"
consent_at = "2026-10-21T00:00:00.000Z"
consent_v = 3
idle_cores = 2
active_cores = 2
idle_after_secs = 60
pause_on_battery = false
priority = "normal"
placement = "system"
native_engine = true
native_engine_url = ""
official_engine = false
native_riven = false
watch = false
max_slots = 1
memory_limit_mb = 0
native_wait_hours = 48
'''
    exe=args.out/'simfleet.exe';shutil.copy2(args.exe,exe)
    config_path=args.out/'simfleet.toml';config_path.write_text(config,encoding='utf-8')
    shutil.copytree(args.old_assets/'cache',args.out/'cache');shutil.copytree(args.old_assets/'engines',args.out/'engines')
    old_ms=int(time.time()*1000)-3600*1000
    state_path=args.out/'engine-state.json';state_path.write_text(json.dumps({'current':OLD,'waiting_since_ms':old_ms}),encoding='utf-8')
    def wait(predicate,timeout=60):
        deadline=time.monotonic()+timeout
        while time.monotonic()<deadline:
            if predicate():return
            if process.poll() is not None:raise AssertionError('client exited: '+str(process.returncode))
            time.sleep(.1)
        raise AssertionError('condition timed out')
    process=None;summary={}
    try:
        for case in ['save-and-hold','restart-retains-wait','deadline-fallback','acceptance-expired','wait-disabled','new-native-available','menu-topology-restart']:
            if case=='deadline-fallback':
                state=json.loads(state_path.read_text());state['waiting_since_ms']=int(time.time()*1000)-49*3600*1000;state_path.write_text(json.dumps(state))
            elif case in ['acceptance-expired','wait-disabled','new-native-available']:
                # Restart with the old native release still selected, but add a
                # new native build during this process; it should switch early.
                shutil.copytree(args.old_assets/'cache',args.out/'cache',dirs_exist_ok=True)
                state_path.write_text(json.dumps({'current':OLD,'waiting_since_ms':old_ms}))
                current['accepted']=[] if case=='acceptance-expired' else [{'engine':OLD['engine'],'until':(datetime.datetime.now(datetime.timezone.utc)+datetime.timedelta(days=3)).isoformat().replace('+00:00','Z')}]
                config_path.write_text(config.replace('native_wait_hours = 48','native_wait_hours = 0') if case=='wait-disabled' else config,encoding='utf-8')
                if case=='new-native-available':
                    lib=args.new_assets/'engines'/(NEW['engine']+'-sfe1-v3.dll')
                    shutil.copy2(lib,args.out/'engines'/lib.name)
            if case=='menu-topology-restart':
                config_path.write_text(config.replace('watch = false','watch = true'),encoding='utf-8')
            with lock:received.clear()
            with (args.out/(case+'.log')).open('wb') as log:
                command=[str(exe.resolve())]+([] if case=='menu-topology-restart' else ['--no-watch'])
                process=subprocess.Popen(command,stdin=subprocess.PIPE,stdout=log,stderr=subprocess.STDOUT)
                if case in ['save-and-hold','restart-retains-wait']:
                    wait(lambda:any(r.get('engine')==OLD['engine'] for r in received))
                    assert json.loads(state_path.read_text())['waiting_since_ms']==old_ms
                    assert not any(r.get('engine')==NEW['engine'] for r in received)
                    if case=='save-and-hold':
                        # Force one save failure, then verify retry without
                        # another keypress, preserving the account/identity.
                        temp=args.out/'simfleet.toml.tmp';temp.mkdir()
                        process.stdin.write(b'+\n');process.stdin.flush();time.sleep(1.5)
                        assert tomllib.loads(config_path.read_text(encoding='utf-8'))['idle_cores']==2
                        temp.rmdir()
                        wait(lambda:tomllib.loads(config_path.read_text(encoding='utf-8'))['idle_cores']==3,10)
                        assert process.poll() is None
                        assert tomllib.loads(config_path.read_text(encoding='utf-8'))['verifier']=='fedcba9876543210fedcba98'
                elif case=='menu-topology-restart':
                    wait(lambda:any(r.get('engine')==NEW['engine'] for r in received),120)
                    # The same menu/key path used in the terminal, without
                    # requiring a desktop window in an automated regression.
                    process.stdin.write(b's\ndown\ndown\ndown\nright\n');process.stdin.flush()
                    wait(lambda:tomllib.loads(config_path.read_text(encoding='utf-8'))['placement']=='numa',10)
                    wait(lambda:(args.out/(case+'.log')).read_text(encoding='utf-8').count('simfleet 0.8.')>=2,90)
                    wait(lambda:len([r for r in received if r.get('engine')==NEW['engine']])>=2,30)
                else:
                    wait(lambda:json.loads(state_path.read_text())['current']['engine']==NEW['engine'],120)
                    assert json.loads(state_path.read_text())['current']['engine']==NEW['engine']
                summary[case]='passed'
                process.stdin.write(b'q\n');process.stdin.flush();process.wait(timeout=60);assert process.returncode==0
                process=None
        summary['package_requests']=http_paths.count('/pkg/wfsim_wasm_bg.'+NEW['wasm']+'.wasm')
        (args.out/'summary.json').write_text(json.dumps(summary,indent=2),encoding='utf-8');print(json.dumps(summary))
    finally:
        if process and process.poll() is None:
            process.stdin.write(b'q\n');process.stdin.flush()
            try:process.wait(timeout=30)
            except subprocess.TimeoutExpired:process.kill();process.wait()
        server.shutdown();server.server_close()

if __name__=='__main__':main()
