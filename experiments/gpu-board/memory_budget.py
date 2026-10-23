#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Serialize heavy GPU research jobs and cap each process family at 24 GiB.

Independent executor jobs cannot share a nested Windows Job Object. A named
mutex admits one compiler/runner at a time, each with its own bounded job.
"""
import argparse
import ctypes as c
from ctypes import wintypes as w
import json
import os
import pathlib
import time
import threading
import sys
import subprocess
import atexit


JOB_NAME = "Local\\Simfleet-GPU-Board-Research-24GiB"
LIMIT_BYTES = min(24*1024,max(1024,int(os.environ.get("SIMFLEET_RESEARCH_JOB_LIMIT_MIB",24*1024))))*1024**2
STOP_BYTES = min(LIMIT_BYTES//1024**2-512,21 * 1024, int(os.environ.get("SIMFLEET_RESEARCH_STOP_MIB", 21 * 1024))) * 1024**2
_handles = []
_peak = {"private_bytes": 0, "resident_bytes": 0}


class ProcessMemory(c.Structure):
    _fields_ = [("cb", w.DWORD), ("faults", w.DWORD)] + [(name, c.c_size_t) for name in
        ("peak_ws", "ws", "peak_pool", "pool", "peak_nonpaged", "nonpaged", "page", "peak_page", "private")]


class Processes(c.Structure):
    _fields_ = [("assigned", w.DWORD), ("count", w.DWORD), ("ids", c.c_size_t * 32)]


def watch(job):
    """Keep 3 GiB below the Job quota for allocation bursts and driver mapping."""
    kernel = c.WinDLL("kernel32", use_last_error=True)
    psapi = c.WinDLL("psapi", use_last_error=True)
    kernel.QueryInformationJobObject.argtypes = [w.HANDLE, c.c_int, c.c_void_p, w.DWORD, c.c_void_p]
    kernel.OpenProcess.argtypes = [w.DWORD, w.BOOL, w.DWORD]
    kernel.OpenProcess.restype = w.HANDLE
    kernel.CloseHandle.argtypes = [w.HANDLE]
    kernel.TerminateProcess.argtypes = [w.HANDLE, w.UINT]
    psapi.GetProcessMemoryInfo.argtypes = [w.HANDLE, c.POINTER(ProcessMemory), w.DWORD]
    while True:
        members = Processes()
        if not kernel.QueryInformationJobObject(job, 3, c.byref(members), c.sizeof(members), None):
            os._exit(91)
        private = resident = 0
        for pid in members.ids[:members.count]:
            process = kernel.OpenProcess(0x0400 | 0x0010, False, pid)
            if process:
                info = ProcessMemory(); info.cb = c.sizeof(info)
                if psapi.GetProcessMemoryInfo(process, c.byref(info), c.sizeof(info)):
                    private += info.private; resident += info.ws
                kernel.CloseHandle(process)
        _peak["private_bytes"] = max(_peak["private_bytes"], private)
        _peak["resident_bytes"] = max(_peak["resident_bytes"], resident)
        if max(private, resident) > STOP_BYTES:
            print(json.dumps(dict(memory_budget_stop=True, private_bytes=private, resident_bytes=resident)), file=sys.stderr, flush=True)
            for pid in members.ids[:members.count]:
                process = kernel.OpenProcess(1, False, pid)
                if process:
                    kernel.TerminateProcess(process, 90)
            os._exit(90)
        time.sleep(0.1)


class Basic(c.Structure):
    _fields_ = [("process_time", c.c_int64), ("job_time", c.c_int64), ("flags", w.DWORD),
                ("min_ws", c.c_size_t), ("max_ws", c.c_size_t), ("active", w.DWORD),
                ("affinity", c.c_size_t), ("priority", w.DWORD), ("scheduling", w.DWORD)]


class IO(c.Structure):
    _fields_ = [(name, c.c_uint64) for name in ("read_ops", "write_ops", "other_ops", "read_bytes", "write_bytes", "other_bytes")]


class Extended(c.Structure):
    _fields_ = [("basic", Basic), ("io", IO), ("process_limit", c.c_size_t), ("job_limit", c.c_size_t),
                ("peak_process", c.c_size_t), ("peak_job", c.c_size_t)]


def limit(pid=None):
    kernel = c.WinDLL("kernel32", use_last_error=True)
    kernel.GetCurrentProcess.restype = w.HANDLE
    kernel.IsProcessInJob.argtypes = [w.HANDLE, w.HANDLE, c.POINTER(w.BOOL)]
    kernel.OpenJobObjectW.argtypes = [w.DWORD, w.BOOL, w.LPCWSTR]
    kernel.OpenJobObjectW.restype = w.HANDLE
    inherited = os.environ.get("SIMFLEET_RESEARCH_JOB")
    if inherited and not pid:
        owner = kernel.OpenJobObjectW(4, False, inherited)
        inside = w.BOOL()
        if owner and kernel.IsProcessInJob(kernel.GetCurrentProcess(), owner, c.byref(inside)) and inside.value:
            _handles.append(owner)
            return owner
    kernel.CreateMutexW.argtypes = [c.c_void_p, w.BOOL, w.LPCWSTR]
    kernel.CreateMutexW.restype = w.HANDLE
    kernel.WaitForSingleObject.argtypes = [w.HANDLE, w.DWORD]
    kernel.ReleaseMutex.argtypes = [w.HANDLE]
    mutex = kernel.CreateMutexW(None, False, JOB_NAME + "-exclusive")
    if not mutex:
        raise c.WinError(c.get_last_error())
    if kernel.WaitForSingleObject(mutex, 0xffffffff) not in (0, 0x80):
        raise c.WinError(c.get_last_error())
    _handles.append(mutex)
    atexit.register(lambda: kernel.ReleaseMutex(mutex))
    kernel.CreateJobObjectW.argtypes = [c.c_void_p, w.LPCWSTR]
    kernel.CreateJobObjectW.restype = w.HANDLE
    kernel.SetInformationJobObject.argtypes = [w.HANDLE, c.c_int, c.c_void_p, w.DWORD]
    kernel.AssignProcessToJobObject.argtypes = [w.HANDLE, w.HANDLE]
    kernel.IsProcessInJob.argtypes = [w.HANDLE, w.HANDLE, c.POINTER(w.BOOL)]
    kernel.OpenProcess.argtypes = [w.DWORD, w.BOOL, w.DWORD]
    kernel.OpenProcess.restype = w.HANDLE
    kernel.GetCurrentProcess.restype = w.HANDLE
    kernel.CloseHandle.argtypes = [w.HANDLE]
    name = JOB_NAME + "-" + str(pid or os.getpid())
    job = kernel.CreateJobObjectW(None, name)
    if not job:
        raise c.WinError(c.get_last_error())
    info = Extended()
    info.basic.flags = 0x200  # JOB_OBJECT_LIMIT_JOB_MEMORY, includes descendants.
    info.job_limit = LIMIT_BYTES
    if not kernel.SetInformationJobObject(job, 9, c.byref(info), c.sizeof(info)):
        raise c.WinError(c.get_last_error())
    process = kernel.OpenProcess(0x0100 | 0x0001 | 0x0400 | 0x00100000, False, pid) if pid else kernel.GetCurrentProcess()
    if not process:
        raise c.WinError(c.get_last_error())
    inside = w.BOOL()
    if not kernel.IsProcessInJob(process, job, c.byref(inside)):
        raise c.WinError(c.get_last_error())
    if not inside.value and not kernel.AssignProcessToJobObject(job, process):
        raise c.WinError(c.get_last_error())
    _handles.append(job)
    if not pid:
        os.environ["SIMFLEET_RESEARCH_JOB"] = name
    threading.Thread(target=watch, args=(job,), daemon=True).start()
    atexit.register(lambda: print(json.dumps(dict(memory_budget_peak=_peak, stop_bytes=STOP_BYTES, job_limit_bytes=LIMIT_BYTES)), file=sys.stderr, flush=True))
    if pid:
        kernel.CloseHandle(process)
    return job


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--pid", type=int)
    p.add_argument("--out", type=pathlib.Path)
    p.add_argument("command", nargs=argparse.REMAINDER)
    args = p.parse_args()
    if args.command:
        limit()
        command = args.command[1:] if args.command[0] == "--" else args.command
        raise SystemExit(subprocess.call(command))
    if not args.pid or not args.out:
        p.error("provide --pid and --out, or -- command...")
    job = limit(args.pid)
    kernel = c.WinDLL("kernel32", use_last_error=True)
    kernel.QueryInformationJobObject.argtypes = [w.HANDLE, c.c_int, c.c_void_p, w.DWORD, c.c_void_p]
    kernel.OpenProcess.argtypes = [w.DWORD, w.BOOL, w.DWORD]
    kernel.OpenProcess.restype = w.HANDLE
    kernel.WaitForSingleObject.argtypes = [w.HANDLE, w.DWORD]
    process = kernel.OpenProcess(0x00100000, False, args.pid)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    print(f"Research process family memory limit: {LIMIT_BYTES // 1024**3} GiB", flush=True)
    with args.out.open("w") as out:
        while True:
            info = Extended()
            if not kernel.QueryInformationJobObject(job, 9, c.byref(info), c.sizeof(info), None):
                raise c.WinError(c.get_last_error())
            out.write(json.dumps(dict(time=time.time(), limit=info.job_limit, peak_commit=info.peak_job)) + "\n")
            out.flush()
            if kernel.WaitForSingleObject(process, 1000) != 258:
                break


if __name__ == "__main__": main()
