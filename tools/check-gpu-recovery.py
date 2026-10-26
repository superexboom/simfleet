#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Reproduce per-row page exhaustion with public Torid builds, then recover."""
import argparse,concurrent.futures,importlib.util,json,pathlib,queue,subprocess,threading
def main():
    p=argparse.ArgumentParser(description=__doc__)
    for name in ('client','package','native','out'):p.add_argument('--'+name,type=pathlib.Path,required=True)
    p.add_argument('--board',type=pathlib.Path)
    p.add_argument('--request',type=pathlib.Path,help='One recorded resource-fallback report; replay that row only')
    p.add_argument('--force-private-kib',type=int,help='Controlled lower quota when a fresh worker no longer reproduces the long-run failure')
    p.add_argument('--memory-mib',type=int,default=8192)
    a=p.parse_args();a.out.mkdir(parents=True,exist_ok=True)
    spec=importlib.util.spec_from_file_location('board',pathlib.Path(__file__).with_name('bench-board.py'));mod=importlib.util.module_from_spec(spec);spec.loader.exec_module(mod)
    native=mod.Native(a.native);repo=pathlib.Path(__file__).resolve().parents[1]
    if a.request:
        check_recorded(a,native);return
    if not a.board:p.error('--board or --request is required')
    base=json.loads((repo/'experiments/prepared-board/orders.json').read_text())[9]
    good=json.loads(native.call('/api/board/order',base))['request']
    source=[r for r in json.loads(a.board.read_text(encoding='utf8')) if r['benchmark']=='demolisher' and r['mode']=='cycle']
    rows=[]
    for i in range(30):
        row=source[i*(len(source)-1)//29]
        record=dict(weapon='torid',**{k:row[k] for k in ('mods','evolutions','arcanes','exilus','valence') if k in row})
        if row.get('riven'):
            r=row['riven'];record.update(riven_pos=sorted(r['bonuses']),riven_rolls=r['rolls'],riven_neg=r.get('malus',''))
        order=dict(record=record,ruler='demolisher',mode='cycle')
        req=json.loads(native.call('/api/board/order',order))['request'];req['runs']=8
        rows.append(dict(id=i+1,request=req,ruler='demolisher'))
    proc=subprocess.Popen([str(a.client.resolve()),'gpu-worker',str(a.package.resolve()),native.engine,'--memory-mib',str(a.memory_mib)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,encoding='utf8',creationflags=0x08000000)
    q=queue.Queue()
    def read():
        for line in proc.stdout:q.put(json.loads(line))
        q.put(dict(error='worker exited'))
    threading.Thread(target=read,daemon=True).start();records=[]
    def batch(inputs):
        proc.stdin.write(json.dumps(inputs)+'\n');proc.stdin.flush();v=q.get(timeout=50)
        while 'phase' in v:v=q.get(timeout=50)
        if v.get('error'):raise RuntimeError(v)
        if [r['id'] for r in v['rows']]!=[r['id'] for r in inputs]:raise AssertionError('wrong IDs')
        return v['rows']
    try:
        ready=q.get(timeout=180)
        if not ready.get('ready'):raise RuntimeError(ready)
        assert ready['memory_limit_bytes']==(a.memory_mib-64)*1024**2,ready
        records.append(dict(stage='configured-process-limit',configured_mib=a.memory_mib,observed_bytes=ready['memory_limit_bytes']))
        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
            failed=None
            for start in (0,15):
                light=dict(id=100,request=dict(good,runs=8),ruler=base['ruler'])
                inputs=[light,*rows[start:start+15]];out=batch(inputs)
                failures=[]
                for inp,result in zip(inputs,out):
                    if result.get('fallback'):
                        assert result['fallback']==120 and result['score'] is None
                        failures.append(inp['id']);failed=inp
                    else:
                        assert result['score']==json.loads(native.row(inp['request'],inp['ruler'],pool,8))
                records.append(dict(stage='mixed',success=len(out)-len(failures),fallback_ids=failures))
                print(json.dumps(records[-1]),flush=True)
                if failed:break
            if not failed:raise AssertionError('corpus did not reproduce page exhaustion')
            (a.out/'failing-row.json').write_text(json.dumps(failed,indent=2)+'\n')
            for kib in (384,768):
                retry=dict(failed,id=200+kib,private_kib=kib);out=batch([retry])[0]
                if not out.get('fallback'):
                    assert out['score']==json.loads(native.row(retry['request'],retry['ruler'],pool,8))
                records.append(dict(stage='larger-budget',private_kib=kib,fallback=out.get('fallback')))
                if not out.get('fallback'):break
            # This is the same process, module and context that hit status 120.
            after=[dict(id=1000+i,request=dict(good,seed=good['seed']+i),ruler=base['ruler']) for i in range(4)]
            out=batch(after)
            for inp,result in zip(after,out):
                assert not result.get('fallback')
                assert result['score']==json.loads(native.row(inp['request'],inp['ruler'],pool,8))
            records.append(dict(stage='same-worker-after-failure',complete_scores=4,runs=4000,pid=proc.pid))
            wide=[dict(row,id=row['id']+10000,private_kib=1536) for row in after[:2]]
            out=batch(wide)
            for inp,result in zip(wide,out):
                assert not result.get('fallback')
                assert result['score']==json.loads(native.row(inp['request'],inp['ruler'],pool,8))
            records.append(dict(stage='1536-kib-budget',complete_scores=2,runs=2000,pid=proc.pid))
        proc.stdin.close();proc.wait(timeout=15);assert proc.returncode==0
    finally:
        if proc.poll() is None:proc.kill();proc.wait()
    report=dict(engine=native.engine,records=records)
    (a.out/'result.json').write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report),flush=True)
def check_recorded(a,native):
    report=json.loads(a.request.read_text(encoding='utf-8-sig'))
    assert report['engine']==native.engine
    original=next(r['input'] for r in report['rows'] if r['status']==120)
    original=dict(original,id=1,probe=False)
    original_budget=original['private_kib']
    if a.force_private_kib is not None:original['private_kib']=a.force_private_kib
    proc=subprocess.Popen([str(a.client.resolve()),'gpu-worker',str(a.package.resolve()),native.engine,'--memory-mib',str(a.memory_mib)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,encoding='utf8',creationflags=0x08000000)
    q=queue.Queue()
    def read():
        for line in proc.stdout:q.put(json.loads(line))
        q.put(dict(error='worker exited'))
    threading.Thread(target=read,daemon=True).start()
    def batch(row):
        proc.stdin.write(json.dumps([row])+'\n');proc.stdin.flush();v=q.get(timeout=60)
        while 'phase' in v:v=q.get(timeout=60)
        if v.get('error'):raise RuntimeError(v)
        assert len(v['rows'])==1 and v['rows'][0]['id']==row['id']
        return v['rows'][0]
    records=[]
    try:
        ready=q.get(timeout=180);assert ready.get('ready'),ready
        assert ready['memory_limit_bytes']==(a.memory_mib-64)*1024**2
        failed=batch(original)
        assert failed.get('fallback')==120 and failed['score'] is None,failed
        records.append(dict(private_kib=original['private_kib'],fallback=120))
        budget=max(original_budget,next(k for k in (64,96,128,160,192,256,384,512,768,1536) if k>=original['private_kib']*1.5))
        recovered=batch(dict(original,id=2,private_kib=budget))
        assert not recovered.get('fallback'),recovered
        assert 0<recovered['max_private_kib']<=budget,recovered
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
            expected=json.loads(native.row(original['request'],original['ruler'],pool,8))
        assert recovered['score']==expected,(recovered,expected)
        records.append(dict(private_kib=budget,max_private_kib=recovered['max_private_kib'],fallback=None,full_score_exact=True,runs=original['request']['runs']))
        proc.stdin.close();proc.wait(timeout=15);assert proc.returncode==0
    finally:
        if proc.poll() is None:proc.kill();proc.wait()
    result=dict(engine=native.engine,memory_mib=a.memory_mib,configured_bytes=ready['memory_limit_bytes'],same_worker=True,original_budget_kib=original_budget,controlled_lower_budget=a.force_private_kib,records=records)
    (a.out/'result.json').write_text(json.dumps(result,indent=2)+'\n',encoding='utf8');print(json.dumps(result),flush=True)

if __name__=='__main__':main()
