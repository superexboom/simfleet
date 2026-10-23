#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Inspect the running client's named Windows memory job without modifying it."""
import argparse,ctypes as c,json,pathlib,time
from ctypes import wintypes as w

class Basic(c.Structure):
    _fields_=[('process_time',c.c_int64),('job_time',c.c_int64),('flags',w.DWORD),('min_ws',c.c_size_t),('max_ws',c.c_size_t),('active',w.DWORD),('affinity',c.c_size_t),('priority',w.DWORD),('scheduling',w.DWORD)]
class IO(c.Structure):
    _fields_=[(n,c.c_uint64) for n in ('read_ops','write_ops','other_ops','read_bytes','write_bytes','other_bytes')]
class Extended(c.Structure):
    _fields_=[('basic',Basic),('io',IO),('process_limit',c.c_size_t),('job_limit',c.c_size_t),('peak_process',c.c_size_t),('peak_job',c.c_size_t)]
class Processes(c.Structure):
    _fields_=[('assigned',w.DWORD),('count',w.DWORD),('ids',c.c_size_t*64)]
class Memory(c.Structure):
    _fields_=[('cb',w.DWORD),('faults',w.DWORD)]+[(n,c.c_size_t) for n in ('peak_ws','ws','peak_paged','paged','peak_nonpaged','nonpaged','page','peak_page','private')]

def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('pid',type=int);p.add_argument('--seconds',type=int,default=3);p.add_argument('--out',type=pathlib.Path);a=p.parse_args()
    k=c.WinDLL('kernel32',use_last_error=True);ps=c.WinDLL('psapi',use_last_error=True)
    k.OpenJobObjectW.argtypes=[w.DWORD,w.BOOL,w.LPCWSTR];k.OpenJobObjectW.restype=w.HANDLE
    k.QueryInformationJobObject.argtypes=[w.HANDLE,c.c_int,c.c_void_p,w.DWORD,c.c_void_p]
    k.OpenProcess.argtypes=[w.DWORD,w.BOOL,w.DWORD];k.OpenProcess.restype=w.HANDLE;k.CloseHandle.argtypes=[w.HANDLE]
    ps.GetProcessMemoryInfo.argtypes=[w.HANDLE,c.POINTER(Memory),w.DWORD]
    handle=k.OpenJobObjectW(4,False,f'Local\\simfleet-compute-{a.pid}')
    if not handle:raise c.WinError(c.get_last_error())
    record=dict(pid=a.pid,samples=[])
    try:
        for _ in range(max(1,min(60,a.seconds))):
            limits=Extended();members=Processes()
            if not k.QueryInformationJobObject(handle,9,c.byref(limits),c.sizeof(limits),None):raise c.WinError(c.get_last_error())
            if not k.QueryInformationJobObject(handle,3,c.byref(members),c.sizeof(members),None):raise c.WinError(c.get_last_error())
            assert limits.basic.flags&0x200 and limits.job_limit>0
            total=0;processes=[]
            for pid in members.ids[:members.count]:
                proc=k.OpenProcess(0x0400|0x0010,False,pid)
                if not proc:continue
                try:
                    memory=Memory();memory.cb=c.sizeof(memory)
                    if not ps.GetProcessMemoryInfo(proc,c.byref(memory),memory.cb):raise c.WinError(c.get_last_error())
                    total+=memory.private;processes.append(dict(pid=pid,private_bytes=memory.private,resident_bytes=memory.ws))
                finally:k.CloseHandle(proc)
            sample=dict(job_limit_bytes=limits.job_limit,peak_job_bytes=limits.peak_job,private_bytes=total,members=processes)
            assert total<=limits.job_limit
            record['samples'].append(sample);print(json.dumps(sample),flush=True);time.sleep(1)
    finally:k.CloseHandle(handle)
    if a.out:a.out.parent.mkdir(parents=True,exist_ok=True);a.out.write_text(json.dumps(record,indent=2)+'\n')
if __name__=='__main__':main()
