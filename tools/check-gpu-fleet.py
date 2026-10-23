#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Replay real board inputs through Fleet; kill its GPU worker to test fallback.

All tasks are synthetic offline leases. No work is claimed or submitted.
"""
import argparse
import collections
import concurrent.futures
import ctypes as c
import importlib.util
import json
import pathlib
import queue
import re
import subprocess
import threading
import time


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--client',type=pathlib.Path,required=True)
    parser.add_argument('--native',type=pathlib.Path,required=True)
    parser.add_argument('--orders',type=pathlib.Path,required=True)
    parser.add_argument('--out',type=pathlib.Path,required=True)
    parser.add_argument('--modes',default='cpu,gpu,killed-gpu')
    parser.add_argument('--lanes',type=int,default=16)
    parser.add_argument('--expect-restart',action='store_true')
    parser.add_argument('--expect-evidence',action='store_true')
    parser.add_argument('--release',type=pathlib.Path)
    parser.add_argument('--order-index',type=int)
    parser.add_argument('--mixed-orders',action='store_true',help='cycle distinct orders and compare the complete multiset of scores/work')
    parser.add_argument('--rows',type=int,default=96)
    parser.add_argument('--expect-no-gpu',action='store_true')
    parser.add_argument('--compute-memory-mib',type=int,default=8192)
    parser.add_argument('--available-memory-mib',type=int,help='offline ceiling on real available physical memory')
    args=parser.parse_args();args.out.mkdir(parents=True,exist_ok=True)
    if not 1<=args.rows<=1000:parser.error('rows must be 1..1000')
    spec=importlib.util.spec_from_file_location('board',pathlib.Path(__file__).with_name('bench-board.py'))
    module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
    native=module.Native(args.native)
    # Hold a query handle through exit so the Job's actual peak remains readable.
    memspec=importlib.util.spec_from_file_location('memory',pathlib.Path(__file__).with_name('inspect-compute-memory.py'))
    memory=importlib.util.module_from_spec(memspec);memspec.loader.exec_module(memory)
    kernel=c.WinDLL('kernel32',use_last_error=True)
    kernel.OpenJobObjectW.argtypes=[c.c_uint32,c.c_int,c.c_wchar_p];kernel.OpenJobObjectW.restype=c.c_void_p
    kernel.QueryInformationJobObject.argtypes=[c.c_void_p,c.c_int,c.c_void_p,c.c_uint32,c.c_void_p]
    kernel.CloseHandle.argtypes=[c.c_void_p]
    orders=json.loads(args.orders.read_text());order=orders[0] if args.mixed_orders else orders[args.order_index] if args.order_index is not None else next(o for o in orders if o['record']['weapon']=='fulmin_prime' and o['mode']=='semi_auto' and o['ruler']=='heavy_gunner')
    selected=orders if args.mixed_orders else [order]
    expected_rows=[]
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.lanes) as pool:
        for item in selected:
            req=json.loads(native.call('/api/board/order',item))['request']
            expected_rows.append(json.loads(native.row(req,item['ruler'],pool,8)))
    fingerprint=lambda score:tuple(score[k] for k in ('score','metric','work'))
    expected_scores=collections.Counter(fingerprint(expected_rows[i%len(selected)]) for i in range(args.rows))
    expected_work=sum(expected_rows[i%len(selected)]['work'] for i in range(args.rows))
    paths=[]
    for i in range(args.rows):
        file=args.out/f'row-{i:03}.json';file.write_text(json.dumps(dict(selected[i%len(selected)],kind='board',lease=f'offline-{i}',question=f'offline-{i}'))+'\n');paths.append(str(file.resolve()))
    reports=[]
    modes=args.modes.split(',')
    if any(mode not in ('cpu','gpu','killed-gpu') for mode in modes):parser.error('invalid mode')
    for mode in modes:
        command=[str(args.client.resolve()),'replay',*paths,'--lanes',str(args.lanes),'--compute-memory-mib',str(args.compute_memory_mib)]
        if args.available_memory_mib is not None:command+=['--available-memory-mib',str(args.available_memory_mib)]
        if mode!='cpu':command+=['--gpu']
        if args.release:command+=['--release',str(args.release.resolve())]
        proc=subprocess.Popen(command,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True,encoding='utf8',creationflags=0x08000000)
        q=queue.Queue()
        def read_output():
            for line in proc.stdout:q.put(line)
            q.put(None)
        threading.Thread(target=read_output,daemon=True).start()
        records=[];scores=[];killed=False;gpu_rows=0;deadline=time.monotonic()+240
        memory_job=None;peak_commit=0;job_limit=0
        try:
            while True:
                line=q.get(timeout=max(1,deadline-time.monotonic()))
                if line is None:break
                if not memory_job:memory_job=kernel.OpenJobObjectW(4,False,f'Local\\simfleet-compute-{proc.pid}')
                records.append(line)
                if line.startswith('{'):
                    v=json.loads(line)
                    if v.get('kind')=='board': scores.append(v)
                if '[GPU] 完成' in line:gpu_rows+=1
                if mode=='killed-gpu' and not killed and '[GPU] 已分配' in line:
                    query=f"Get-CimInstance Win32_Process -Filter 'ParentProcessId={proc.pid}' | Where-Object {{$_.Name -eq 'simfleet.exe'}} | Select-Object -ExpandProperty ProcessId"
                    found=subprocess.check_output(['powershell','-NoProfile','-Command',query],text=True,creationflags=0x08000000)
                    kernel=c.WinDLL('kernel32');kernel.OpenProcess.argtypes=[c.c_uint32,c.c_int,c.c_uint32];kernel.OpenProcess.restype=c.c_void_p
                    kernel.TerminateProcess.argtypes=[c.c_void_p,c.c_uint];kernel.CloseHandle.argtypes=[c.c_void_p]
                    for value in found.split():
                        handle=kernel.OpenProcess(1,False,int(value))
                        if handle:
                            killed=bool(kernel.TerminateProcess(handle,93));kernel.CloseHandle(handle)
                if time.monotonic()>deadline:raise TimeoutError('Fleet replay did not complete')
            proc.wait(timeout=15)
        finally:
            if proc.poll() is None:proc.kill()
            proc.wait(timeout=15)
            (args.out/f'{mode}.log').write_text(''.join(records),encoding='utf8')
            if memory_job:
                info=memory.Extended()
                try:
                    if not kernel.QueryInformationJobObject(memory_job,9,c.byref(info),c.sizeof(info),None):raise c.WinError(c.get_last_error())
                    peak_commit=info.peak_job;job_limit=info.job_limit
                    assert job_limit==(args.compute_memory_mib-64)*1024**2 and peak_commit<=job_limit
                finally:kernel.CloseHandle(memory_job)
        if not memory_job:raise AssertionError('Replay memory job was not observed')
        if proc.returncode or len(scores)!=args.rows:raise AssertionError(dict(mode=mode,code=proc.returncode,rows=len(scores),tail=records[-10:]))
        if collections.Counter(map(fingerprint,scores))!=expected_scores:raise AssertionError(dict(mode=mode,reason='complete score/work multiset differs'))
        fallback=any('任务回到 CPU' in line for line in records)
        tally=next((re.fullmatch(r'REPLAY-WORK total=(\d+) gpu=(\d+)\s*',line) for line in records if line.startswith('REPLAY-WORK ')),None)
        gpu_work=0;next_gpu=False
        for line in records:
            if '[GPU] 完成' in line:next_gpu=True
            elif line.startswith('{') and json.loads(line).get('kind')=='board':
                if next_gpu:gpu_work+=json.loads(line)['work']
                next_gpu=False
        if not tally or int(tally[1])!=expected_work or int(tally[2])!=gpu_work:
            raise AssertionError(dict(reason='CPU/GPU contribution accounting mismatch',tally=tally.groups() if tally else None,gpu_rows=gpu_rows,work=expected_work))
        if mode=='gpu' and not gpu_rows and not args.expect_no_gpu:raise AssertionError('Fleet never used GPU')
        if args.expect_no_gpu and gpu_rows:raise AssertionError('slow probe produced GPU scores')
        if mode=='killed-gpu' and not (killed and fallback):raise AssertionError('failed GPU worker was not recovered')
        restarted=any('重建计算进程' in line for line in records)
        failure_at=next((i for i,line in enumerate(records) if '任务回到 CPU' in line),None)
        ready_after=next((i for i,line in enumerate(records) if failure_at is not None and i>failure_at and '[GPU]' in line and '就绪' in line),None)
        cpu_during_recovery=sum(line.startswith('{') and json.loads(line).get('kind')=='board' for line in records[failure_at+1:ready_after]) if failure_at is not None else 0
        if args.expect_restart and mode=='killed-gpu' and (not restarted or sum('[GPU]' in line and '就绪' in line for line in records)<2):raise AssertionError('GPU worker did not become ready again')
        if args.expect_restart and mode=='killed-gpu' and not cpu_during_recovery:raise AssertionError('CPU made no progress while GPU recovered')
        if args.expect_evidence and mode=='killed-gpu':
            evidence=json.loads((args.client.parent/'gpu-diagnostics/last-failure.json').read_text(encoding='utf8'))
            assert evidence['retryable'] and evidence['requests'] and evidence['engine']==native.engine
            assert 'GPU worker' in evidence['error']
            assert 'account' not in evidence and 'session' not in evidence
        report=dict(mode=mode,complete_scores=len(scores),gpu_rows=gpu_rows,total_work=int(tally[1]),gpu_work=int(tally[2]),compute_memory_mib=args.compute_memory_mib,available_memory_mib=args.available_memory_mib,job_limit_bytes=job_limit,peak_commit_bytes=peak_commit,worker_killed=killed,fallback_observed=fallback,worker_restarted=restarted,cpu_rows_during_gpu_recovery=cpu_during_recovery,summary=next(line.strip() for line in records if line.startswith('REPLAY tasks=')))
        reports.append(report);print(json.dumps(report),flush=True)
    (args.out/'validation.json').write_text(json.dumps(dict(engine=native.engine,runs=reports),indent=2)+'\n')


if __name__=='__main__':main()
