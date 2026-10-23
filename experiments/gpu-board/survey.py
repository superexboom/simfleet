#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Bounded full-duration GPU screening; does not claim or submit fleet work."""
import argparse,json,pathlib,subprocess,sys,time
from memory_budget import limit

def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--native',type=pathlib.Path,required=True)
    p.add_argument('--package',type=pathlib.Path,required=True)
    p.add_argument('--python-deps',type=pathlib.Path,required=True)
    p.add_argument('--out',type=pathlib.Path,required=True)
    p.add_argument('--runs',type=int,default=16)
    p.add_argument('--select',default='')
    args=p.parse_args();limit()
    here=pathlib.Path(__file__).resolve().parent;root=here.parents[1]
    cases=[]
    for prefix,path in [('training',root/'experiments/prepared-board/orders.json'),('holdout',root/'experiments/hardware/board-orders-holdout.json')]:
        for i,order in enumerate(json.loads(path.read_text(encoding='utf8'))):
            cases.append((f'{prefix}-{i}',order))
    torid=cases[2][1]
    for mode in ['base','incarnon']:
        value=json.loads(json.dumps(torid));value['mode']=mode;cases.append(('torid-'+mode,value))
    value=json.loads(json.dumps(torid));value['record']['arcanes']=[];cases.append(('torid-no-arcane',value))
    selected=set(args.select.split(',')) if args.select else None
    args.out.mkdir(parents=True,exist_ok=True)
    results=[]
    for label,order in cases:
        if selected and label not in selected:continue
        directory=args.out/label;directory.mkdir(exist_ok=True)
        orderfile=directory/'order.json';orderfile.write_text(json.dumps(order)+'\n')
        output=directory/'result.json'
        record=dict(label=label,weapon=order['record']['weapon'],ruler=order['ruler'],mode=order['mode'])
        if not output.exists():
            commands=[('prepare',[sys.executable,here/'prepare.py','--wasm',args.package/'bridge.wasm','--native',args.native,
                '--wire-library',args.package/'wire.dll','--python-deps',args.python_deps,'--orders',orderfile,
                '--runs',str(args.runs),'--references','1','--out',directory/'snapshot']),
                ('compute',[sys.executable,here/'run.py','--ptx',args.package/'engine.cubin','--native',args.native,
                '--wire-library',args.package/'wire.dll','--snapshot',directory/'snapshot','--count',str(args.runs),
                '--cow','--private-kib','768','--block','32','--repeats','1','--out',output])]
            for stage,command in commands:
                with (directory/(stage+'.log')).open('w',encoding='utf8') as log:
                    try: done=subprocess.run([str(c) for c in command],stdout=log,stderr=subprocess.STDOUT,timeout=65)
                    except subprocess.TimeoutExpired: record['error']=stage+' timeout';break
                if done.returncode:record['error']=stage+' failed';break
        if output.exists():
            result=json.loads(output.read_text(encoding='utf8'))
            if result.get('exact_shards')!=args.runs:record['error']='incomplete proof'
            else:
                record.update(gpu_seconds=result['compute_seconds'],cpu_seconds=result.get('cpu_full_row_seconds'),
                    private_pages=result.get('private_pages'),exact_shards=result['exact_shards'],score=result.get('exact_score'))
        results.append(record)
        (args.out/'results.json').write_text(json.dumps(results,indent=2)+'\n')
        print(json.dumps({k:v for k,v in record.items() if k!='score'}),flush=True)

if __name__=='__main__':main()
