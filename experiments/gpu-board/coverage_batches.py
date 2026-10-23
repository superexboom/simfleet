#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Measure promising screen cases as normal 1,000-run rows with CPU overlap."""
import argparse,json,pathlib,subprocess,sys
from memory_budget import limit
def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--native',type=pathlib.Path,required=True);p.add_argument('--package',type=pathlib.Path,required=True)
    p.add_argument('--python-deps',type=pathlib.Path,required=True);p.add_argument('--screen',type=pathlib.Path,required=True)
    p.add_argument('--out',type=pathlib.Path,required=True);p.add_argument('--select',required=True)
    a=p.parse_args();limit();here=pathlib.Path(__file__).resolve().parent;a.out.mkdir(parents=True,exist_ok=True)
    for label in a.select.split(','):
        folder=a.out/label;folder.mkdir(exist_ok=True);order=a.screen/label/'order.json'
        commands=[('prepare',[sys.executable,here/'prepare.py','--wasm',a.package/'bridge.wasm','--native',a.native,'--wire-library',a.package/'wire.dll',
            '--python-deps',a.python_deps,'--orders',order,'--references','1','--out',folder/'snapshot']),
            ('mixed',[sys.executable,here/'packed.py','--module',a.package/'engine.cubin','--native',a.native,'--wire-library',a.package/'wire.dll',
            '--snapshot',folder/'snapshot','--copies','16','--private-kib','192','--block','32','--repeats','4','--cpu-lanes','16','--cpu-rows','240','--baseline-first','--out',folder/'result.json'])]
        for stage,command in commands:
            with (folder/(stage+'.log')).open('w',encoding='utf8') as log:
                try:r=subprocess.run([str(v) for v in command],stdout=log,stderr=subprocess.STDOUT,timeout=240)
                except subprocess.TimeoutExpired:print(json.dumps(dict(label=label,error=stage+' timeout')),flush=True);break
            if r.returncode:print(json.dumps(dict(label=label,error=stage+' failed')),flush=True);break
        if (folder/'result.json').exists():
            d=json.loads((folder/'result.json').read_text());print(json.dumps(dict(label=label,median=d['median_seconds'],mixed=d.get('mixed'),exact_shards=d['exact_shards'])),flush=True)
if __name__=='__main__':main()
