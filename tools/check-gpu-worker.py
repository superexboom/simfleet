#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Exercise the shipped client/worker IPC against complete native CPU scores."""
import argparse
import concurrent.futures
import ctypes as c
import hashlib
import importlib.util
import json
import pathlib
import queue
import subprocess
import threading
import time


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--client',type=pathlib.Path,required=True)
    p.add_argument('--package',type=pathlib.Path,required=True)
    p.add_argument('--native',type=pathlib.Path,required=True)
    p.add_argument('--orders',type=pathlib.Path,required=True)
    p.add_argument('--out',type=pathlib.Path,required=True)
    p.add_argument('--batches',type=int,default=70)
    p.add_argument('--meter',action='store_true')
    p.add_argument('--order-index',type=int)
    args=p.parse_args()
    if not 1 <= args.batches <= 1000: p.error('batches must be 1..1000')
    spec=importlib.util.spec_from_file_location('board',pathlib.Path(__file__).with_name('bench-board.py'))
    module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
    native=module.Native(args.native)
    orders=json.loads(args.orders.read_text())
    order=orders[args.order_index] if args.order_index is not None else next(o for o in orders if o['record']['weapon']=='fulmin_prime' and o['mode']=='semi_auto' and o['ruler']=='heavy_gunner')
    base=json.loads(native.call('/api/board/order',order))['request']
    rows=[];expected=[]
    with concurrent.futures.ThreadPoolExecutor(max_workers=16) as pool:
        for i in range(16):
            request=dict(base,seed=base['seed']+i)
            rows.append(dict(id=1000+i,request=request,ruler=order['ruler']))
            expected.append(json.loads(native.row(request,order['ruler'],pool,8)))
    proc=subprocess.Popen([str(args.client.resolve()),'gpu-worker',str(args.package.resolve()),native.engine],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,encoding='utf8',creationflags=0x08000000)
    messages=queue.Queue()
    def reader():
        for line in proc.stdout: messages.put(json.loads(line))
        messages.put(dict(error='worker exited',code=proc.wait()))
    threading.Thread(target=reader,daemon=True).start()
    class Memory(c.Structure):
        _fields_=[('cb',c.c_uint32),('faults',c.c_uint32)]+[(k,c.c_size_t) for k in ('peak_ws','ws','peak_paged','paged','peak_nonpaged','nonpaged','page','peak_page','private')]
    kernel=c.WinDLL('kernel32');kernel.OpenProcess.argtypes=[c.c_uint32,c.c_int,c.c_uint32];kernel.OpenProcess.restype=c.c_void_p
    kernel.CloseHandle.argtypes=[c.c_void_p]
    psapi=c.WinDLL('psapi');psapi.GetProcessMemoryInfo.argtypes=[c.c_void_p,c.POINTER(Memory),c.c_uint32]
    handle=kernel.OpenProcess(0x0400|0x0010,False,proc.pid)
    records=[];started=time.perf_counter();meter=None;meter_samples=[]
    try:
        ready=messages.get(timeout=180)
        if ready.get('error') or not ready.get('ready'): raise RuntimeError(ready)
        print(json.dumps(ready),flush=True)
        if args.meter:
            meter=subprocess.Popen([str(args.client.resolve()),'gpu-meter',str(proc.pid),'--seconds','10'],stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,encoding='utf8',creationflags=0x08000000)
        duplicate=subprocess.run([str(args.client.resolve()),'gpu-worker',str(args.package.resolve()),native.engine],capture_output=True,text=True,encoding='utf8',timeout=20,creationflags=0x08000000)
        if duplicate.returncode==0 or 'already owns GPU' not in duplicate.stdout: raise AssertionError('GPU device ownership was not exclusive')
        for trial in range(args.batches):
            # Different batch sizes exercise retained buffers and result offsets.
            count=4 if trial in (0,2) else 16
            current=[dict(row,id=row['id']+trial*100) for row in rows[:count]]
            before=time.perf_counter();proc.stdin.write(json.dumps(current)+'\n');proc.stdin.flush()
            out=messages.get(timeout=45)
            phases=[]
            while 'phase' in out:
                if out['id']!=current[0]['id']:raise AssertionError('progress belongs to wrong batch')
                phases.append(out['phase']);out=messages.get(timeout=45)
            if phases!=['prepare','compute','merge']:raise AssertionError(phases)
            if out.get('error'): raise RuntimeError(out)
            if len(out['rows'])!=count: raise AssertionError('wrong row count')
            for actual,want,request in zip(out['rows'],expected,current):
                if actual['id']!=request['id'] or actual['score']!=want: raise AssertionError(dict(trial=trial,actual=actual,expected=want))
            mem=Memory();mem.cb=c.sizeof(mem)
            if not psapi.GetProcessMemoryInfo(handle,c.byref(mem),mem.cb): raise RuntimeError('memory sample failed')
            records.append(dict(trial=trial,rows=count,seconds=time.perf_counter()-before,private_bytes=mem.private,resident_bytes=mem.ws))
            if trial%10==0: print(json.dumps(records[-1]),flush=True)
        # A malformed/duplicate task must fail without returning any usable row.
        proc.stdin.write(json.dumps([rows[0],rows[0]])+'\n');proc.stdin.flush()
        bad=messages.get(timeout=15)
        if not bad.get('error') or 'rows' in bad: raise AssertionError('invalid batch was accepted')
        proc.wait(timeout=15)
        if proc.returncode==0: raise AssertionError('invalid worker input should stop worker')
    finally:
        if proc.poll() is None: proc.kill()
        proc.wait(timeout=15);kernel.CloseHandle(handle)
        if meter:
            output,errors=meter.communicate(timeout=20)
            if meter.returncode:raise RuntimeError(errors)
            meter_samples=[json.loads(line) for line in output.splitlines()]
            if not any(s.get('utilization',0) and s['utilization']>0.05 for s in meter_samples):raise AssertionError(dict(no_gpu_activity=meter_samples))
            if not any(s.get('dedicated_bytes',0) and s['dedicated_bytes']>100*1024**2 for s in meter_samples):raise AssertionError('no GPU memory samples')
    plateau=[r['private_bytes'] for r in records[5:]]
    growth=max(plateau[-5:],default=0)-min(plateau[:5],default=0)
    if plateau and growth>256*1024**2: raise AssertionError(f'worker retained memory grew by {growth} bytes')
    mismatch=subprocess.run([str(args.client.resolve()),'gpu-worker',str(args.package.resolve()),'wrong-engine'],capture_output=True,text=True,encoding='utf8',timeout=20,creationflags=0x08000000)
    if mismatch.returncode==0 or 'engine or host mismatch' not in mismatch.stdout: raise AssertionError('wrong engine was accepted')
    report=dict(engine=native.engine,client_sha256=hashlib.sha256(args.client.read_bytes()).hexdigest(),
                complete_scores=sum(r['rows'] for r in records),different_requests=16,
                batches=args.batches,elapsed_seconds=time.perf_counter()-started,invalid_input_rejected=True,exclusive_device=True,engine_mismatch_rejected=True,
                retained_growth_bytes=growth,samples=records)
    if meter:report['meter_samples']=meter_samples
    args.out.parent.mkdir(parents=True,exist_ok=True);args.out.write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps({k:v for k,v in report.items() if k!='samples'}),flush=True)


if __name__=='__main__': main()
