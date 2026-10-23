#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Validate generic GPU probes and complete previously unlisted board rows."""
import argparse,concurrent.futures,importlib.util,json,pathlib,queue,subprocess,threading,time

def main():
    p=argparse.ArgumentParser(description=__doc__)
    for n in ('client','package','native','orders','out'):p.add_argument('--'+n,type=pathlib.Path,required=True)
    p.add_argument('--select',default='6,10,11,14')
    p.add_argument('--full-probe-limit-ms',type=float,default=3000)
    a=p.parse_args();a.out.mkdir(parents=True,exist_ok=True)
    spec=importlib.util.spec_from_file_location('board',pathlib.Path(__file__).with_name('bench-board.py'));m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
    native=m.Native(a.native);orders=json.loads(a.orders.read_text(encoding='utf8'))
    command=[str(a.client.resolve()),'gpu-worker',str(a.package.resolve()),native.engine,'--native',str(a.native.resolve())]
    proc=subprocess.Popen(command,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,encoding='utf8',creationflags=0x08000000)
    q=queue.Queue()
    def reader():
        for line in proc.stdout:q.put(json.loads(line))
        q.put(dict(error='worker exited'))
    threading.Thread(target=reader,daemon=True).start()
    def batch(rows):
        proc.stdin.write(json.dumps(rows)+'\n');proc.stdin.flush();v=q.get(timeout=60)
        while 'phase' in v:v=q.get(timeout=60)
        if v.get('error'):raise AssertionError(v)
        assert [r['id'] for r in v['rows']]==[r['id'] for r in rows];return v['rows']
    records=[]
    try:
        ready=q.get(timeout=180);assert ready.get('ready') and ready.get('probe'),ready
        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
            for i in map(int,a.select.split(',')):
                order=orders[i];request=json.loads(native.call('/api/board/order',order))['request']
                probe=dict(id=i,request=request,ruler=order['ruler'],probe=True,private_kib=1536)
                started=time.perf_counter();out=batch([probe])[0]
                record=dict(index=i,weapon=request['weapon'],ruler=order['ruler'],probe=out.get('probe'),fallback=out.get('fallback'),probe_wall=time.perf_counter()-started)
                assert out['score'] is None
                if out.get('probe'):
                    stats=out['probe'];assert stats['samples']==min(8,request['runs']);assert stats['cpu_ms']>0
                    budget=next((b for b in (192,384,768,1536) if b>=(stats['max_pages']+12)*4),1536)
                    if stats['gpu_ms']<a.full_probe_limit_ms:
                        rows=[dict(id=10000+i*10+j,request=dict(request,seed=request['seed']+j),ruler=order['ruler'],private_kib=budget) for j in range(min(4,16000*192//budget//request['runs']))]
                        started=time.perf_counter();results=batch(rows);record['full_seconds']=time.perf_counter()-started
                        record['full_scores']=0;record['full_fallback']=0
                        for row,result in zip(rows,results):
                            if result.get('fallback'):assert result['score'] is None;record['full_fallback']+=1;continue
                            expected=json.loads(native.row(row['request'],row['ruler'],pool,8))
                            assert result['score']==expected,(row,result,expected)
                            assert result.get('probe') is None;record['full_scores']+=1
                records.append(record);(a.out/'results.json').write_text(json.dumps(dict(engine=native.engine,records=records),indent=2)+'\n')
                print(json.dumps(record),flush=True)
        proc.stdin.close();proc.wait(timeout=15);assert proc.returncode==0
    finally:
        if proc.poll() is None:proc.kill();proc.wait()
    print(json.dumps(dict(verified=len(records),complete_scores=sum(r.get('full_scores',0) for r in records))),flush=True)
if __name__=='__main__':main()
