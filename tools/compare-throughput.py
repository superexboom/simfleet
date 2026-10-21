#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Alternate fixed-input client benchmarks, save raw logs and exact answers.
No leases are requested. Keep the served engine pinned throughout the run.
"""
import argparse
import hashlib
import json
import os
import pathlib
import re
import statistics
import subprocess

def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--baseline',type=pathlib.Path,required=True)
    p.add_argument('--candidate',type=pathlib.Path,required=True)
    p.add_argument('--out',type=pathlib.Path,required=True)
    p.add_argument('--order',type=pathlib.Path)
    p.add_argument('--lanes',type=int,default=6)
    p.add_argument('--repeats',type=int,default=3)
    p.add_argument('--candidate-reference',action='store_true',help='same client and DLL; baseline disables opaque board ABI')
    p.add_argument('--placement',choices=['auto','system','cache','numa'],default='auto')
    args=p.parse_args();args.out.mkdir(parents=True,exist_ok=False)
    all_rows=[];engine=None;answer=None
    for trial in range(args.repeats):
        for name in (['baseline','candidate'] if trial%2==0 else ['candidate','baseline']):
            exe=args.candidate if args.candidate_reference else getattr(args,name)
            command=[str(exe.resolve()),'bench','--backend','native','--lanes',str(args.lanes)]
            if name=='candidate' or args.candidate_reference:command+=['--placement',args.placement]
            if args.order:command+=['--order',str(args.order.resolve())]
            env=os.environ.copy()
            if args.candidate_reference and name=='baseline':env['SIMFLEET_NATIVE_FAST']='0'
            else:env.pop('SIMFLEET_NATIVE_FAST',None)
            result=subprocess.run(command,env=env,capture_output=True,text=True,encoding='utf-8',timeout=600,check=True)
            text=result.stdout+result.stderr
            (args.out/f'{trial}-{name}.log').write_text(text,encoding='utf-8')
            current=re.search(r'网站引擎 (\S+) · wasm (\S+)',text).groups()
            if engine is None:engine=current
            assert current==engine,'engine changed during measurement'
            if name=='candidate':assert '原生批次接口通过' in text,'candidate fast path not validated'
            found=re.findall(r'Native ×\s+(\d+) 核: ([\d.]+)s.*?分数 (\S+) (\S+)\s+work (\d+)',text)
            assert found,text
            for lanes,seconds,score,metric,work in found:
                now=(score,metric,work)
                if answer is None:answer=now
                assert now==answer,'answer changed'
                all_rows.append(dict(variant=name,trial=trial,lanes=int(lanes),seconds=float(seconds)))
            print(trial,name,found,flush=True)
    summary=[]
    for lanes in sorted(set(r['lanes'] for r in all_rows)):
        med={name:statistics.median(r['seconds'] for r in all_rows if r['variant']==name and r['lanes']==lanes) for name in ['baseline','candidate']}
        summary.append(dict(lanes=lanes,**med,speedup=med['baseline']/med['candidate']))
    binaries={name:hashlib.sha256(getattr(args,name).read_bytes()).hexdigest() for name in ['baseline','candidate']}
    report=dict(engine=engine,answer=answer,rows=all_rows,summary=summary,placement=args.placement,binaries=binaries)
    (args.out/'results.json').write_text(json.dumps(report,indent=2),encoding='utf-8')
    print(json.dumps(summary),flush=True)

if __name__=='__main__':main()
