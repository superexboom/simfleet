#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Offline native quick-fleet benchmark. Lead state stays on the main thread;
score chunks use a fixed pool. Input is a complete optimize request JSON.
"""
import argparse
import ctypes as c
import hashlib
import json
import math
import pathlib
import time
from concurrent.futures import ThreadPoolExecutor

def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('library',type=pathlib.Path)
    p.add_argument('request',type=pathlib.Path)
    p.add_argument('--lanes',type=int,default=6)
    p.add_argument('--out',type=pathlib.Path,required=True)
    args=p.parse_args()
    lib=c.CDLL(str(args.library.resolve()))
    fn=lib.sfe_optimize;fn.argtypes=[c.c_char_p,c.c_size_t,c.POINTER(c.c_size_t)];fn.restype=c.c_void_p
    free=lib.sfe_free;free.argtypes=[c.c_void_p,c.c_size_t]
    req=json.loads(args.request.read_text(encoding='utf-8'))
    def digest(value):
        return hashlib.sha256(json.dumps(value,sort_keys=True,separators=(',',':'),ensure_ascii=False).encode()).hexdigest()
    def call(fleet):
        body=json.dumps({**req,'quick_fleet':fleet}).encode()
        n=c.c_size_t();start=time.perf_counter();ptr=fn(body,len(body),c.byref(n))
        if not ptr:raise RuntimeError('native panic')
        try: out=json.loads(c.string_at(ptr,n.value))
        finally:free(ptr,n.value)
        if out.get('ok') is False:raise RuntimeError(out)
        return out,time.perf_counter()-start
    start=time.perf_counter();scores=[];work=0;fights=0;lead_seconds=0;score_seconds=0;rounds=[]
    with ThreadPoolExecutor(max_workers=max(1,args.lanes)) as pool:
        for step in range(5000):
            out,seconds=call({'lead':True,'fresh':step==0,'scores':scores})
            lead_seconds+=seconds
            if 'pending' not in out:
                result={'seconds':time.perf_counter()-start,'lead_seconds':lead_seconds,'score_lane_seconds':score_seconds,
                        'final_step_seconds':seconds,'steps':step+1,'answer':out,'work':work+out.get('final_work',0),'fights':fights,
                        'rounds':rounds,'library_sha256':hashlib.sha256(args.library.read_bytes()).hexdigest()}
                args.out.write_text(json.dumps(result,ensure_ascii=False,indent=2),encoding='utf-8')
                print(json.dumps({k:v for k,v in result.items() if k not in ('answer','rounds')}),flush=True)
                return
            trace={'step':step,'pending':len(out['pending']),'lead_seconds':seconds,'lead_sha256':digest(out)}
            pending=out['pending'];size=max(1,math.ceil(len(pending)/(args.lanes*2)))
            pieces=[pending[i:i+size] for i in range(0,len(pending),size)]
            scores=[];score_start=time.perf_counter();score_calls=[]
            for out,seconds in pool.map(lambda part:call({'score':part}),pieces):
                scores+=out.get('scores',[]);work+=out.get('work',0);fights+=out.get('fights',0);score_seconds+=seconds
                score_calls.append(digest(out))
            trace.update(score_wall_seconds=time.perf_counter()-score_start,score_sha256=digest(score_calls),seconds=time.perf_counter()-start)
            rounds.append(trace)
    raise RuntimeError('search did not settle')

if __name__=='__main__':main()
