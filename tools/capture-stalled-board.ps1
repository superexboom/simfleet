# SPDX-License-Identifier: AGPL-3.0-or-later
# Read-only capture of retained board JSON. Does not suspend or change the target.
[CmdletBinding()]
param(
    [int]$ProcessId = 0,
    [string]$Weapon = 'strun_wraith',
    [string]$OutDir = '',
    [ValidateRange(16,4096)][int]$MaxReadMiB = 1024,
    [ValidateRange(1,120)][int]$MaxSeconds = 30
)
$ErrorActionPreference = 'Stop'
if (-not [Environment]::Is64BitProcess) { throw 'Run with 64-bit PowerShell.' }
if ($Weapon -notmatch '^[a-z0-9_]+$') { throw 'Weapon must be an engine weapon ID.' }
function Get-CaptureCandidates($processes) {
    # A normal launch has a watch parent and a client with the same executable.
    # Prefer processes actually holding an engine; otherwise keep child leaves.
    $engines = @($processes | Where-Object { $_.EngineLoaded })
    if ($engines.Count -gt 0) { return $engines }
    $parents = @($processes | ForEach-Object { $_.ParentId })
    @($processes | Where-Object { $_.Id -notin $parents })
}
if ($ProcessId -eq 0) {
    $targets = @(Get-Process | Where-Object { $_.ProcessName -match '^simfleet' })
    if ($targets.Count -eq 0) { throw 'No simfleet process found. Keep the stalled client running.' }
    $parents = @{}
    $gpuWorkers = @{}
    try {
        Get-CimInstance Win32_Process | Where-Object { $_.ProcessId -in $targets.Id } | ForEach-Object {
            $parents[[int]$_.ProcessId] = [int]$_.ParentProcessId
            if ($_.CommandLine -match '(?i)\s"?gpu-worker"?(\s|$)') { $gpuWorkers[[int]$_.ProcessId] = $true }
        }
    } catch { } # Module identification still works if CIM is unavailable.
    $descriptions = @($targets | Where-Object { -not $gpuWorkers.ContainsKey([int]$_.Id) } | ForEach-Object {
        $loaded = $false
        try { $loaded = @($_.Modules | Where-Object { $_.ModuleName -match '([0-9a-f]{12}.*\.dll$|wfsim.*\.dll$)' }).Count -gt 0 } catch { }
        $path = $null
        try { $path = $_.Path } catch { }
        [pscustomobject]@{Id=$_.Id; ParentId=$parents[[int]$_.Id]; EngineLoaded=$loaded
            Name=$_.ProcessName; MemoryMiB=[math]::Round($_.WorkingSet64 / 1MB); Path=$path}
    })
    $choices = @(Get-CaptureCandidates $descriptions | Sort-Object Id)
    if ($choices.Count -eq 0) { throw 'The client exited while listing processes. Run capture again while it is stalled.' }
    if ($choices.Count -eq 1) {
        $ProcessId = $choices[0].Id
        Write-Host "Selected compute PID $ProcessId (watch process skipped)."
    } else {
        Write-Host 'Several compute instances are running. Choose the stalled instance:'
        for ($i=0; $i -lt $choices.Count; $i++) {
            $c = $choices[$i]
            Write-Host ("[{0}] PID {1}  {2} MiB  {3}" -f ($i+1),$c.Id,$c.MemoryMiB,$c.Path)
        }
        do {
            $answer = Read-Host 'Enter the number in brackets (Q to cancel)'
            if ($answer -match '^[qQ]$') { return }
            $selection = 0
            $valid = [int]::TryParse($answer,[ref]$selection) -and $selection -ge 1 -and $selection -le $choices.Count
            if (-not $valid) { Write-Host 'Enter one of the listed numbers.' }
        } until ($valid)
        $ProcessId = $choices[$selection-1].Id
    }
}
$target = Get-Process -Id $ProcessId
if (-not $OutDir) { $OutDir = Join-Path $PSScriptRoot ('strun-capture-' + (Get-Date -Format 'yyyyMMdd-HHmmss')) }
$OutDir = [IO.Path]::GetFullPath($OutDir)
if (Test-Path -LiteralPath $OutDir) { throw "Output already exists: $OutDir" }
if (-not ('Simfleet.BoardCapture' -as [type])) {
Add-Type -TypeDefinition @"
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Text;
namespace Simfleet {
public sealed class CaptureResult {
    public List<string> Candidates = new List<string>();
    public long ReadBytes;
    public int PartialReads;
    public long AnchorHits;
    public long WeaponHits;
    public bool Limited;
    public double Seconds;
}
public static class BoardCapture {
    [StructLayout(LayoutKind.Sequential)] struct Region {
        public IntPtr BaseAddress, AllocationBase;
        public uint AllocationProtect;
        public ushort PartitionId;
        public UIntPtr RegionSize;
        public uint State, Protect, Type;
    }
    [DllImport("kernel32.dll", SetLastError=true)] static extern IntPtr OpenProcess(uint access, bool inherit, int pid);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool ReadProcessMemory(IntPtr process, IntPtr address, byte[] data, UIntPtr size, out UIntPtr read);
    [DllImport("kernel32.dll", SetLastError=true)] static extern UIntPtr VirtualQueryEx(IntPtr process, IntPtr address, out Region region, UIntPtr length);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool IsWow64Process(IntPtr process, out bool wow64);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
    const int Chunk = 4 * 1024 * 1024, Context = 256 * 1024, MaxCandidates = 256;
    static readonly UTF8Encoding Utf8 = new UTF8Encoding(false, true);
    // Walk complete JSON objects, respecting escaped quotes and nested arrays/objects.
    static int End(byte[] data, int start, int limit) {
        int depth = 0; bool text = false, escape = false;
        for (int i=start; i<limit; i++) {
            byte b=data[i];
            if (text) { if (escape) escape=false; else if (b==92) escape=true; else if (b==34) text=false; }
            else if (b==34) text=true;
            else if (b==123 || b==91) depth++;
            else if (b==125 || b==93) { if (--depth==0) return i+1; }
            else if (b<32 && b!=9 && b!=10 && b!=13) return -1;
        }
        return -1;
    }
    static void Find(byte[] data, int length, byte[] needle, HashSet<string> seen, CaptureResult result, Stopwatch timer, int seconds) {
        for (int at=0; at<=length-needle.Length; at++) {
            if ((at & 65535)==0 && timer.Elapsed.TotalSeconds>=seconds) return;
            if (data[at]!=needle[0]) continue;
            int n=1; while(n<needle.Length && data[at+n]==needle[n]) n++;
            if(n!=needle.Length) continue;
            result.AnchorHits++;
            int tried=0;
            for(int start=at-1; start>=Math.Max(0,at-Context) && tried<128; start--) {
                if(data[start]!=123) continue;
                tried++;
                int end=End(data,start,Math.Min(length,start+Context));
                if(end<at+needle.Length) continue;
                try {
                    string raw=Utf8.GetString(data,start,end-start);
                    if(raw.Contains("\"weapon\"") && raw.Contains("\"mods\"") && raw.Contains("\"duration\"") && seen.Add(raw)) result.Candidates.Add(raw);
                } catch(DecoderFallbackException) {}
                if(result.Candidates.Count>=MaxCandidates || timer.Elapsed.TotalSeconds>=seconds) return;
            }
        }
    }
    [StructLayout(LayoutKind.Sequential)] struct FileTime { public uint Low, High; }
    [StructLayout(LayoutKind.Sequential)] struct ThreadEntry {
        public uint ExitStatus;
        public IntPtr TebBaseAddress;
        public uint ProcessId, ThreadId;
        public UIntPtr AffinityMask;
        public int Priority, BasePriority;
        public IntPtr LastSyscallFirstArgument;
        public ushort LastSyscallNumber;
        public FileTime CreateTime, ExitTime, KernelTime, UserTime;
        public IntPtr Win32StartAddress;
        public FileTime CaptureTime;
        public uint Flags;
        public ushort SuspendCount, SizeOfContextRecord;
        public IntPtr ContextRecord;
    }
    [DllImport("kernel32.dll")] static extern uint PssCaptureSnapshot(IntPtr process, uint flags, uint contextFlags, out IntPtr snapshot);
    [DllImport("kernel32.dll")] static extern uint PssFreeSnapshot(IntPtr process, IntPtr snapshot);
    [DllImport("kernel32.dll")] static extern uint PssWalkMarkerCreate(IntPtr allocator, out IntPtr marker);
    [DllImport("kernel32.dll")] static extern uint PssWalkMarkerFree(IntPtr marker);
    [DllImport("kernel32.dll")] static extern uint PssWalkSnapshot(IntPtr snapshot, int info, IntPtr marker, out ThreadEntry entry, uint size);
    public sealed class ThreadSample {
        public uint ThreadId;
        public string Instruction, Entry;
        public double CpuMs;
        public List<string> StackCodeCandidates = new List<string>();
    }
    public sealed class InstructionSample {
        public string At = DateTime.UtcNow.ToString("o");
        public string Error;
        public List<ThreadSample> Threads = new List<ThreadSample>();
    }
    static string Locate(long address, List<ProcessModule> modules) {
        foreach(var module in modules) {
            long offset=address-module.BaseAddress.ToInt64();
            if(offset>=0 && offset<module.ModuleMemorySize) return module.ModuleName+"+0x"+offset.ToString("x");
        }
        return null;
    }
    static double CpuTime(FileTime time) { return (((ulong)time.High<<32)|time.Low)/10000.0; }
    // PSS captures thread contexts only: no heap clone, dump, manual thread
    // suspension, or changes to the target. Never export raw registers/stack.
    public static InstructionSample Sample(int pid) {
        var result=new InstructionSample();
        IntPtr process=IntPtr.Zero, snapshot=IntPtr.Zero, marker=IntPtr.Zero;
        try {
            process=OpenProcess(0x0400 | 0x0010, false, pid);
            if(process==IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error());
            bool wow64;
            if(!IsWow64Process(process,out wow64)) throw new Win32Exception(Marshal.GetLastWin32Error());
            if(wow64) throw new InvalidOperationException("Thread sampling requires a 64-bit target.");
            var modules=new List<ProcessModule>();
            using(var target=Process.GetProcessById(pid)) { foreach(ProcessModule module in target.Modules) modules.Add(module); }
            uint status=PssCaptureSnapshot(process,0x80|0x100,0x100001,out snapshot); // threads + AMD64 control context
            if(status!=0) throw new Win32Exception((int)status,"PssCaptureSnapshot");
            status=PssWalkMarkerCreate(IntPtr.Zero,out marker);
            if(status!=0) throw new Win32Exception((int)status,"PssWalkMarkerCreate");
            ThreadEntry entry;
            var stack=new byte[8192];
            while((status=PssWalkSnapshot(snapshot,3,marker,out entry,(uint)Marshal.SizeOf(typeof(ThreadEntry))))==0) {
                if(entry.ContextRecord==IntPtr.Zero || entry.SizeOfContextRecord<256) continue;
                // SDK AMD64 CONTEXT: Rsp offset 152; Rip offset 248.
                long ip=Marshal.ReadInt64(entry.ContextRecord,248), sp=Marshal.ReadInt64(entry.ContextRecord,152);
                var thread=new ThreadSample { ThreadId=entry.ThreadId, Instruction=Locate(ip,modules),
                    Entry=Locate(entry.Win32StartAddress.ToInt64(),modules), CpuMs=CpuTime(entry.KernelTime)+CpuTime(entry.UserTime) };
                UIntPtr got;
                if(sp>0 && ReadProcessMemory(process,new IntPtr(sp),stack,(UIntPtr)stack.Length,out got)) {
                    var seen=new HashSet<string>();
                    for(int i=0;i+8<=(int)got.ToUInt64() && thread.StackCodeCandidates.Count<48;i+=8) {
                        string location=Locate(BitConverter.ToInt64(stack,i),modules);
                        if(location!=null && seen.Add(location)) thread.StackCodeCandidates.Add(location);
                    }
                }
                result.Threads.Add(thread);
            }
            if(status!=259) throw new Win32Exception((int)status,"PssWalkSnapshot");
        } catch(Exception error) { result.Error=error.Message; }
        finally {
            if(marker!=IntPtr.Zero) PssWalkMarkerFree(marker);
            if(snapshot!=IntPtr.Zero) PssFreeSnapshot(process,snapshot);
            if(process!=IntPtr.Zero) CloseHandle(process);
        }
        return result;
    }

    public static CaptureResult Read(int pid, string weapon, long maxBytes, int seconds) {
        IntPtr handle=OpenProcess(0x0400 | 0x0010, false, pid); // QUERY_INFORMATION | VM_READ
        if(handle==IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error());
        var result=new CaptureResult(); var timer=Stopwatch.StartNew();
        try {
            bool wow64;
            if(!IsWow64Process(handle,out wow64)) throw new Win32Exception(Marshal.GetLastWin32Error());
            if(wow64) throw new InvalidOperationException("The target must be 64-bit.");
            var needle=Encoding.ASCII.GetBytes("\"runs\"");
            var weaponNeedle=Encoding.ASCII.GetBytes(weapon);
            var seen=new HashSet<string>(StringComparer.Ordinal);
            var buffer=new byte[Chunk+2*Context];
            long address=0;
            Region region;
            while(VirtualQueryEx(handle,new IntPtr(address),out region,(UIntPtr)Marshal.SizeOf(typeof(Region)))!=UIntPtr.Zero) {
                long begin=region.BaseAddress.ToInt64(), size=(long)region.RegionSize.ToUInt64();
                if(size<=0 || begin>long.MaxValue-size) break;
                long end=begin+size;
                // Only committed, readable private allocations; skip executables, mapped files and guard pages.
                uint protection=region.Protect & 0xff;
                bool readable=protection==2 || protection==4 || protection==8 || protection==0x20 || protection==0x40 || protection==0x80;
                if(region.State==0x1000 && region.Type==0x20000 && (region.Protect & 0x100)==0 && readable) {
                    for(long pos=begin; pos<end; pos+=Chunk) {
                        if(result.ReadBytes>=maxBytes || timer.Elapsed.TotalSeconds>=seconds || result.Candidates.Count>=MaxCandidates) { result.Limited=true; return result; }
                        long from=Math.Max(begin,pos-Context);
                        int wanted=(int)Math.Min(buffer.Length,Math.Min(end-from,maxBytes-result.ReadBytes));
                        UIntPtr got;
                        bool ok=ReadProcessMemory(handle,new IntPtr(from),buffer,(UIntPtr)wanted,out got);
                        int count=(int)Math.Min((long)wanted,(long)got.ToUInt64());
                        if(!ok) result.PartialReads++;
                        if(count>0) {
                            result.ReadBytes+=count;
                            for(int i=0; i<=count-weaponNeedle.Length; i++) {
                                if(buffer[i]!=weaponNeedle[0]) continue;
                                int n=1; while(n<weaponNeedle.Length && buffer[i+n]==weaponNeedle[n]) n++;
                                if(n==weaponNeedle.Length) result.WeaponHits++;
                            }
                            Find(buffer,count,needle,seen,result,timer,seconds);
                        }
                    }
                }
                if(end<=address) break;
                address=end;
                if(timer.Elapsed.TotalSeconds>=seconds) { result.Limited=true; break; }
            }
            return result;
        } finally { result.Seconds=timer.Elapsed.TotalSeconds; CloseHandle(handle); }
    }
}
}
"@
}
function Thread-Readings($p) {
    $p.Refresh()
    @($p.Threads | ForEach-Object {
        try {
            $wait = if ($_.ThreadState -eq 'Wait') { $_.WaitReason.ToString() } else { $null }
            [pscustomobject]@{id=$_.Id; cpu_ms=$_.TotalProcessorTime.TotalMilliseconds; state=$_.ThreadState.ToString(); wait=$wait}
        } catch { }
    })
}
# No settings, account IDs, network leases or arbitrary heap contents are written.
function Has-PrivateField($value) {
    if ($null -eq $value -or $value -is [string] -or $value -is [ValueType]) { return $false }
    if ($value -is [array]) {
        foreach ($entry in $value) { if (Has-PrivateField $entry) { return $true } }
    } else {
        foreach ($property in $value.PSObject.Properties) {
            if ($property.Name -match '^(verifier|lease|token|access_token|refresh_token|cookie|authorization|password|session|account)$') { return $true }
            if (Has-PrivateField $property.Value) { return $true }
        }
    }
    return $false
}
$before = @(Thread-Readings $target)
$instructionSamples = @([Simfleet.BoardCapture]::Sample($ProcessId))
Write-Host "Reading PID $ProcessId ($($target.ProcessName)); limit $MaxReadMiB MiB / $MaxSeconds seconds..."
$capture = [Simfleet.BoardCapture]::Read($ProcessId,$Weapon,([long]$MaxReadMiB * 1MB),$MaxSeconds)
$after = @(Thread-Readings $target)
$instructionSamples += [Simfleet.BoardCapture]::Sample($ProcessId)
[IO.Directory]::CreateDirectory($OutDir) | Out-Null
$utf8 = New-Object Text.UTF8Encoding($false)
$accepted = @()
$seen = @{}
foreach ($raw in $capture.Candidates) {
    try { $v = ConvertFrom-Json -InputObject $raw } catch { continue }
    $kind = $null
    if ($v.weapon -is [string] -and $null -ne $v.mods -and $null -ne $v.runs -and $null -ne $v.duration) {
        $kind = 'request'
    } elseif ($v.record.weapon -eq $Weapon -and $v.ruler -and $v.mode -and @($v.PSObject.Properties).Count -eq 3) {
        $kind = 'order'
    }
    if (-not $kind -or (Has-PrivateField $v)) { continue }
    $digest = [Security.Cryptography.SHA256]::Create()
    try { $sha = [BitConverter]::ToString($digest.ComputeHash($utf8.GetBytes($raw))).Replace('-','').ToLowerInvariant() } finally { $digest.Dispose() }
    if ($seen.ContainsKey($sha)) { continue }
    $seen[$sha] = $true
    $name = '{0}-{1:d3}.json' -f $kind,($accepted.Count+1)
    [IO.File]::WriteAllText((Join-Path $OutDir $name),$raw,$utf8)
    $accepted += [pscustomobject]@{file=$name; sha256=$sha; kind=$kind; weapon=$v.weapon; mode=$v.mode; runs=$v.runs; enemy=$v.enemy}
}
$modules = @()
try {
    $modules = @($target.Modules | Where-Object { $_.ModuleName -match '(simfleet.*\.exe$|[0-9a-f]{12}.*\.dll$|wfsim.*\.dll$)' } | ForEach-Object {
        [pscustomobject]@{name=$_.ModuleName; sha256=(Get-FileHash -LiteralPath $_.FileName -Algorithm SHA256).Hash.ToLowerInvariant()}
    })
} catch { $modules = @([pscustomobject]@{error=$_.Exception.Message}) }
$report = [ordered]@{
    captured_utc=[DateTime]::UtcNow.ToString('o'); pid=$ProcessId; process=$target.ProcessName; weapon=$Weapon
    read_bytes=$capture.ReadBytes; seconds=$capture.Seconds; limited=$capture.Limited; partial_reads=$capture.PartialReads
    note='Candidates include all retained board requests, including completed/queued rows. StackCodeCandidates are module pointers found on the stack, not an unwound call stack. No raw stack/register/heap contents are exported.'
    capture_version=3; anchor_hits=$capture.AnchorHits; weapon_hits=$capture.WeaponHits; candidates=$capture.Candidates.Count
    modules=$modules; threads_before=$before; threads_after=$after; instruction_samples=$instructionSamples; inputs=$accepted
}
[IO.File]::WriteAllText((Join-Path $OutDir 'capture.json'),(ConvertTo-Json -InputObject $report -Depth 12),$utf8)
Write-Host "Saved $($accepted.Count) inputs to: $OutDir"
if ($accepted.Count -eq 0) { Write-Warning 'No matching request found. Keep capture.json; it records limits and thread states.' }
Write-Host 'Send this capture folder. Thread snapshots do not terminate or modify the target.'
