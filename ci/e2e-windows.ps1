#Requires -Version 7.0
<#
.SYNOPSIS
End-to-end test of dd-gui.exe on Windows: a smart backup and restore, a sector-by-sector
copy and a wipe, run for real as administrator, on virtual disks (VHDX) only.

.DESCRIPTION
    pwsh -File ci/e2e-windows.ps1 C:\path\to\dd-gui.exe      (or the path in $env:DD_GUI)

It creates a VHDX with diskpart, with NTFS, FAT32 and exFAT volumes full of random files,
backs the disk up with `dd-gui.exe copy --mode=smart` (locking its volumes as the GUI
does), restores that onto a second VHDX full of garbage, and checks every volume
(chkdsk) and every file (SHA-256). It also copies sector by sector with the bundled dd,
wipes with zeros, and cancels workers.

It only ever writes to virtual disks it created itself, in its own temporary folder:
Assert-Ours checks before every write that the disk number belongs to that VHDX, and
that the disk is a "Msft Virtual Disk" on the "File Backed Virtual" bus, of its size.
Only one copy of the test disk is attached at a time: Windows takes a disk offline
when another one has the same GPT disk GUID.

Settings: E2E_WORKDIR (default $env:RUNNER_TEMP or the temp folder; needs about 5 GB),
E2E_DISK_MB (default 1024), E2E_TIME_LIMIT (seconds, default 1500).
#>
param([string]$DdGui = $env:DD_GUI)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$ProgressPreference = 'SilentlyContinue'

$script:Passed = 0
$script:Attached = [System.Collections.Generic.List[string]]::new()
$script:Work = $null
$script:Clock = [System.Diagnostics.Stopwatch]::StartNew()
$script:TimeLimit = if ($env:E2E_TIME_LIMIT) { [int]$env:E2E_TIME_LIMIT } else { 1500 }
$script:DiskMB = if ($env:E2E_DISK_MB) { [int]$env:E2E_DISK_MB } else { 1024 }
$script:DiskBytes = [long]$script:DiskMB * 1MB

function Step([string]$Text) {
    if ($script:Clock.Elapsed.TotalSeconds -gt $script:TimeLimit) { Fail "over the time limit of $($script:TimeLimit) s" }
    Write-Host ''
    Write-Host "== $Text"
}
function Pass([string]$Text) {
    $script:Passed++
    Write-Host "PASS  $Text"
}
function Fail([string]$Text) { throw "FAIL  $Text" }

# Runs a program and waits for it, at most $TimeoutSec: its exit code and output. Its stdin
# is closed right away (a worker watching stdin takes that as "cancel").
function Invoke-Tool {
    param([Parameter(Mandatory)][string]$File, [string[]]$Arguments = @(), [int]$TimeoutSec = 600)
    $info = [System.Diagnostics.ProcessStartInfo]::new($File)
    foreach ($a in $Arguments) { if (-not [string]::IsNullOrEmpty($a)) { $info.ArgumentList.Add($a) } }
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardInput = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $process = [System.Diagnostics.Process]::Start($info)
    $out = $process.StandardOutput.ReadToEndAsync()
    $err = $process.StandardError.ReadToEndAsync()
    $process.StandardInput.Close()
    if (-not $process.WaitForExit($TimeoutSec * 1000)) {
        try { $process.Kill($true) } catch { Write-Verbose "it had ended: $_" }
        Fail "$([IO.Path]::GetFileName($File)) $($Arguments -join ' ') didn't finish within $TimeoutSec s"
    }
    $process.WaitForExit()
    [pscustomobject]@{
        Code = $process.ExitCode
        Out = $out.GetAwaiter().GetResult()
        Err = $err.GetAwaiter().GetResult()
    }
}

function Invoke-DdGui([string[]]$Arguments) { Invoke-Tool -File $script:DdGui -Arguments $Arguments }

# The --ddgui-lock options for a drive from `dd-gui.exe drives`, as the GUI passes them.
function Get-LockArgs($Drive) { @(@($Drive.lock) | Where-Object { $_ } | ForEach-Object { "--ddgui-lock=$_" }) }

# The last few lines a failed run printed, for the log.
function Tail([string]$Text, [int]$Lines = 3) {
    (($Text -split "`r?`n") | Where-Object { $_ -and $_ -notmatch '^@progress' } | Select-Object -Last $Lines) -join ' | '
}

# The @-lines a worker printed must include these, in this order.
function Assert-Protocol([string]$Out, [string[]]$Want, [string]$What) {
    $lines = @(($Out -split "`r?`n") | Where-Object { $_ -match '^@' -and $_ -notmatch '^@progress' } | ForEach-Object { ($_ -split ' ')[0] })
    $at = 0
    foreach ($line in $lines) { if ($at -lt $Want.Count -and $line -eq $Want[$at]) { $at++ } }
    if ($at -ne $Want.Count) { Fail "$What`: worker lines $($lines -join ' '), expected $($Want -join ' ')" }
}

function New-RandomFile([string]$Path, [long]$Size) {
    $buffer = [byte[]]::new([Math]::Max(1, [Math]::Min($Size, 1MB)))
    $random = [System.Security.Cryptography.RandomNumberGenerator]::Create()
    $stream = [IO.File]::Create($Path)
    try {
        $left = $Size
        while ($left -gt 0) {
            $n = [int][Math]::Min($left, $buffer.Length)
            $random.GetBytes($buffer, 0, $n)
            $stream.Write($buffer, 0, $n)
            $left -= $n
        }
    } finally {
        $stream.Dispose()
        $random.Dispose()
    }
}

# SHA-256 of $Size zero bytes, to compare a wiped disk with.
function Get-ZerosHash([long]$Size) {
    $hash = [System.Security.Cryptography.IncrementalHash]::CreateHash([System.Security.Cryptography.HashAlgorithmName]::SHA256)
    $zeros = [byte[]]::new(1MB)
    $left = $Size
    while ($left -gt 0) {
        $n = [int][Math]::Min($left, $zeros.Length)
        $hash.AppendData($zeros, 0, $n)
        $left -= $n
    }
    [Convert]::ToHexString($hash.GetHashAndReset())
}

function Invoke-Diskpart([string[]]$Commands, [string]$What) {
    $file = Join-Path $script:Work ("diskpart-" + [guid]::NewGuid().ToString('N') + '.txt')
    Set-Content -Path $file -Value $Commands -Encoding ascii
    $r = Invoke-Tool -File (Join-Path $env:SystemRoot 'System32\diskpart.exe') -Arguments @('/s', $file) -TimeoutSec 300
    if ($r.Code -ne 0) { Fail "diskpart couldn't $What (exit code $($r.Code)): $(Tail $r.Out 4)" }
}

# Creates an empty VHDX of the test size and attaches it; its disk number.
function New-TestDisk([string]$Path) {
    Invoke-Diskpart @(
        "create vdisk file=`"$Path`" maximum=$($script:DiskMB) type=expandable",
        "select vdisk file=`"$Path`"",
        'attach vdisk'
    ) "create and attach $Path"
    $script:Attached.Add($Path)
    Get-TestDiskNumber $Path
}

function Get-TestDiskNumber([string]$Path) {
    for ($i = 0; $i -lt 50; $i++) {
        $image = Get-DiskImage -ImagePath $Path
        if ($image.Attached -and $null -ne $image.Number) { return [int]$image.Number }
        Start-Sleep -Milliseconds 200
    }
    Fail "$Path is attached, but has no disk number"
}

function Dismount-TestDisk([string]$Path) {
    Dismount-DiskImage -ImagePath $Path | Out-Null
    [void]$script:Attached.Remove($Path)
}

# Before every write: disk $Number has to be the VHDX at $Path, which we made.
function Assert-Ours([int]$Number, [string]$Path) {
    if (-not $Path.StartsWith($script:Work, [StringComparison]::OrdinalIgnoreCase)) { Fail "refusing to write: $Path isn't ours" }
    $image = Get-DiskImage -ImagePath $Path
    if (-not $image.Attached -or $image.Number -ne $Number) { Fail "refusing to write to disk $Number`: it isn't $Path" }
    $disk = Get-Disk -Number $Number
    if ($disk.FriendlyName -ne 'Msft Virtual Disk' -or @('File Backed Virtual', '15') -notcontains [string]$disk.BusType) {
        Fail "refusing to write to disk $Number`: it's '$($disk.FriendlyName)' on '$($disk.BusType)', not a virtual disk"
    }
    if ($disk.Size -ne $script:DiskBytes) { Fail "refusing to write to disk $Number`: $($disk.Size) bytes, not $($script:DiskBytes)" }
    if ($disk.IsBoot -or $disk.IsSystem) { Fail "refusing to write to disk $Number`: it's a system disk" }
}

# What `dd-gui.exe drives` says about disk $Number (it has to be listed, as a VHD).
function Get-ListedDrive([int]$Number) {
    $r = Invoke-DdGui @('drives')
    if ($r.Code -ne 0) { Fail "dd-gui drives failed: $(Tail $r.Err)" }
    $path = "\\.\PhysicalDrive$Number"
    $drive = @($r.Out | ConvertFrom-Json) | Where-Object { $_.path -eq $path }
    if (-not $drive) { Fail "dd-gui drives doesn't list $path" }
    $wrong = @()
    if ($drive.kind -ne 'Virtual') { $wrong += "kind $($drive.kind)" }
    if ($drive.bus -ne 'VHD') { $wrong += "bus $($drive.bus)" }
    if ([long]$drive.size -ne $script:DiskBytes) { $wrong += "size $($drive.size)" }
    if ($drive.io_path -ne $path) { $wrong += "io_path $($drive.io_path)" }
    if ($drive.system -or $drive.read_only -or $drive.removable) { $wrong += 'system, read-only or removable' }
    if ($wrong) { Fail "dd-gui drives lists $path wrong: $($wrong -join ', ')" }
    $drive
}

# Its volumes (and their letters) once Windows has read the new partition table.
function Wait-Volumes([int]$Number, [int]$Count) {
    Update-Disk -Number $Number -ErrorAction SilentlyContinue
    for ($i = 0; $i -lt 60; $i++) {
        $disk = Get-Disk -Number $Number
        if ($disk.IsOffline) { Set-Disk -Number $Number -IsOffline $false -ErrorAction SilentlyContinue }
        $parts = @(Get-Partition -DiskNumber $Number -ErrorAction SilentlyContinue | Where-Object { $_.Type -eq 'Basic' })
        foreach ($p in $parts) {
            if ([string]$p.DriveLetter -notmatch '^[A-Za-z]$') {
                $p | Add-PartitionAccessPath -AssignDriveLetter -ErrorAction SilentlyContinue
            }
        }
        $lettered = @(Get-Partition -DiskNumber $Number -ErrorAction SilentlyContinue |
                Where-Object { [string]$_.DriveLetter -match '^[A-Za-z]$' })
        $ready = @($lettered | Where-Object { Test-Path "$($_.DriveLetter):\" })
        if ($ready.Count -ge $Count) { return $ready }
        Start-Sleep -Milliseconds 500
    }
    Fail "disk $Number doesn't show $Count volumes with drive letters"
}

# chkdsk (read-only) and every file's hash on the volumes of disk $Number.
function Test-Volumes([int]$Number, [string]$What) {
    $parts = Wait-Volumes $Number 3
    $seen = @()
    foreach ($p in $parts) {
        $letter = [string]$p.DriveLetter
        $fs = (Get-Volume -DriveLetter $letter).FileSystem
        $kind = @{ 'NTFS' = 'ntfs'; 'FAT32' = 'fat'; 'exFAT' = 'exfat' }[$fs]
        if (-not $kind) { Fail "$What`: unexpected file system '$fs' on $letter`:" }
        $r = Invoke-Tool -File (Join-Path $env:SystemRoot 'System32\chkdsk.exe') -Arguments @("$letter`:") -TimeoutSec 300
        if ($r.Code -ne 0) { Fail "$What`: chkdsk found problems on the $fs volume $letter`: (exit code $($r.Code)): $(Tail $r.Out 4)" }
        foreach ($entry in $script:Sums[$kind].GetEnumerator()) {
            $file = Join-Path "$letter`:\" $entry.Key
            if (-not (Test-Path -LiteralPath $file)) { Fail "$What`: $file is missing" }
            if ((Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash -ne $entry.Value) { Fail "$What`: $file differs" }
        }
        $seen += $fs
    }
    if ((($seen | Sort-Object) -join ',') -ne 'exFAT,FAT32,NTFS') { Fail "$What`: found the volumes $($seen -join ', ')" }
    Pass "$What`: NTFS, FAT32 and exFAT pass chkdsk, and every file matches"
}

# Reads $Bytes of disk $Number back through dd-gui's dd into a file; its SHA-256.
function Get-DiskHash([int]$Number, [string]$File) {
    $r = Invoke-DdGui @('dd', "if=\\.\PhysicalDrive$Number", "of=$File", 'bs=4M', "count=$($script:DiskBytes)", 'iflag=count_bytes', 'status=none')
    if ($r.Code -ne 0) { Fail "dd couldn't read disk $Number back: $(Tail $r.Err)" }
    if ((Get-Item $File).Length -ne $script:DiskBytes) { Fail "dd read $((Get-Item $File).Length) bytes of disk $Number" }
    $hash = (Get-FileHash -LiteralPath $File -Algorithm SHA256).Hash
    Remove-Item -LiteralPath $File
    $hash
}

$code = 0
try {
    # --- Setup ---------------------------------------------------------------------------
    $identity = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
    if (-not $identity.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { Fail 'run this as administrator' }
    if (-not $DdGui) { Fail 'usage: e2e-windows.ps1 C:\path\to\dd-gui.exe (or set DD_GUI)' }
    if (-not (Test-Path -LiteralPath $DdGui -PathType Leaf)) { Fail "$DdGui doesn't exist" }
    $script:DdGui = (Resolve-Path -LiteralPath $DdGui).Path

    $base = if ($env:E2E_WORKDIR) { $env:E2E_WORKDIR } elseif ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
    $script:Work = Join-Path (Resolve-Path $base).Path ('dd-gui-e2e-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
    New-Item -ItemType Directory -Path $script:Work | Out-Null
    Write-Host "working in $($script:Work)"
    $version = Invoke-DdGui @('dd', '--version')
    if ($version.Code -ne 0) { Fail "$($script:DdGui) doesn't run: $(Tail $version.Err)" }

    # --- The source disk -----------------------------------------------------------------
    Step 'A VHDX with NTFS, FAT32 and exFAT, full of random files'
    $tree = Join-Path $script:Work 'tree'
    $files = @{
        # (The sums in parentheses: a comma binds tighter than + in PowerShell.)
        ntfs = @(@('big.bin', (11MB + 7)), @('folder\small.bin', 5000))
        fat = @(@('big.bin', (6MB + 3)), @('folder\odd.bin', 777777))
        exfat = @(@('big.bin', (4MB + 1)), @('folder\tiny.bin', 1))
    }
    $script:Sums = @{}
    foreach ($kind in $files.Keys) {
        $script:Sums[$kind] = @{}
        foreach ($f in $files[$kind]) {
            $path = Join-Path (Join-Path $tree $kind) $f[0]
            New-Item -ItemType Directory -Force -Path (Split-Path $path) | Out-Null
            New-RandomFile $path $f[1]
            $script:Sums[$kind][$f[0]] = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash
        }
    }

    $srcVhdx = Join-Path $script:Work 'src.vhdx'
    $src = New-TestDisk $srcVhdx
    Assert-Ours $src $srcVhdx
    Invoke-Diskpart @(
        "select disk $src",
        'online disk noerr',
        'attributes disk clear readonly noerr',
        'convert gpt',
        'create partition primary size=300', 'format fs=ntfs label="SRCNTFS" quick', 'assign',
        'create partition primary size=250', 'format fs=fat32 label="SRCFAT" quick', 'assign',
        'create partition primary', 'format fs=exfat label="SRCEXFAT" quick', 'assign'
    ) "partition disk $src"
    $parts = Wait-Volumes $src 3
    foreach ($p in $parts) {
        $letter = [string]$p.DriveLetter
        $kind = @{ 'NTFS' = 'ntfs'; 'FAT32' = 'fat'; 'exFAT' = 'exfat' }[(Get-Volume -DriveLetter $letter).FileSystem]
        Copy-Item -Recurse -Path (Join-Path (Join-Path $tree $kind) '*') -Destination "$letter`:\"
        # A deleted file: its space is free, which a smart copy skips.
        New-RandomFile "$letter`:\deleted.bin" 2MB
        Remove-Item "$letter`:\deleted.bin"
        Write-VolumeCache -DriveLetter $letter
    }
    Write-Host "source disk: \\.\PhysicalDrive$src, volumes $(($parts | ForEach-Object { "$($_.DriveLetter):" }) -join ' ')"
    Pass 'source disk built'
    Test-Volumes $src 'source disk'

    # --- dd-gui drives -------------------------------------------------------------------
    Step 'dd-gui drives'
    $drive = Get-ListedDrive $src
    $letters = @($parts | ForEach-Object { "$($_.DriveLetter):".ToUpper() })
    foreach ($l in $letters) {
        if (@($drive.lock) -notcontains $l) { Fail "dd-gui drives doesn't lock $l on disk $src`: $($drive.lock -join ', ')" }
        if (@($drive.mountpoints) -notcontains "$l\") { Fail "dd-gui drives doesn't show $l\ on disk $src" }
    }
    foreach ($v in 'SRCNTFS (NTFS)', 'SRCFAT (FAT32)', 'SRCEXFAT (exFAT)') {
        if (@($drive.volumes) -notcontains $v) { Fail "dd-gui drives doesn't show '$v': $($drive.volumes -join ', ')" }
    }
    Pass "dd-gui drives lists \\.\PhysicalDrive$src as a VHD, with its volumes and what to lock: $($drive.lock -join ' ')"

    # --- Smart backup --------------------------------------------------------------------
    Step 'Smart backup (dd-gui copy --mode=smart), locking the volumes as the GUI does'
    $backup = Join-Path $script:Work 'backup.img.zst'
    $r = Invoke-DdGui (@('copy', '--ddgui-worker') + @(Get-LockArgs $drive) + @('--mode=smart', "--from=\\.\PhysicalDrive$src", "--to=$backup"))
    if ($r.Code -ne 0) { Fail "smart backup failed (exit code $($r.Code)): $(Tail $r.Err)" }
    Assert-Protocol $r.Out @('@ready', '@copy', '@total', '@sync') 'smart backup'
    $used = [long](($r.Out -split "`r?`n" | Where-Object { $_ -match '^@total ' } | Select-Object -First 1) -split ' ')[1]
    if ($used -le 0 -or $used -ge $script:DiskBytes / 2) { Fail "the smart copy read $used bytes of a $($script:DiskMB) MiB disk" }
    $head = [byte[]]::new(4)
    $stream = [IO.File]::OpenRead($backup)
    try { [void]$stream.Read($head, 0, 4) } finally { $stream.Dispose() }
    $magic = [Convert]::ToHexString($head)
    $format = if ($magic -eq '28B52FFD' -or $magic -match '^5[0-9A-F]2A4D18$') { 'zstd' } elseif ($magic -like '1F8B*') { 'gzip' } else { Fail "the smart image starts with $magic" }
    Pass "smart backup: $used bytes of data, a $format image of $((Get-Item $backup).Length) bytes"

    # Windows mounts a volume again by itself once nothing holds its lock: what
    # drives::remount relies on there.
    $back = $false
    for ($i = 0; $i -lt 40 -and -not $back; $i++) {
        $back = @($letters | Where-Object { Test-Path -LiteralPath "$_\big.bin" }).Count -eq $letters.Count
        if (-not $back) { Start-Sleep -Milliseconds 250 }
    }
    if (-not $back) { Fail "the source volumes didn't come back after the worker let go of them" }
    Pass 'the locked source volumes came back by themselves'

    Step 'Sector by sector with the bundled dd, from the source (its volumes locked, so it holds still)'
    $full = Join-Path $script:Work 'full.img'
    $r = Invoke-DdGui (@('dd', '--ddgui-worker') + @(Get-LockArgs $drive) +
        @("if=\\.\PhysicalDrive$src", "of=$full", 'bs=4M', "count=$($script:DiskBytes)", 'iflag=count_bytes', 'status=progress'))
    if ($r.Code -ne 0) { Fail "dd from disk $src failed: $(Tail $r.Err)" }
    if ((Get-Item $full).Length -ne $script:DiskBytes) { Fail "dd read $((Get-Item $full).Length) bytes" }
    $fullHash = (Get-FileHash -LiteralPath $full -Algorithm SHA256).Hash
    Pass "dd read all of disk $src"
    # For the record: what Windows does when a read runs past the end of a drive (3 MiB
    # blocks don't divide the disk), which decides whether dd needs count=... there.
    $past = Join-Path $script:Work 'past-the-end.img'
    $r = Invoke-DdGui @('dd', "if=\\.\PhysicalDrive$src", "of=$past", 'bs=3M', 'status=none')
    $got = if (Test-Path $past) { (Get-Item $past).Length } else { 0 }
    Write-Host "note: dd with bs=3M over a $($script:DiskBytes)-byte disk: exit code $($r.Code), $got bytes$(if ($r.Code) { ": $(Tail $r.Err 1)" })"
    Remove-Item -LiteralPath $past -ErrorAction SilentlyContinue
    Dismount-TestDisk $srcVhdx

    # --- The image works without DD-GUI --------------------------------------------------
    Step 'The smart image unpacks with zstd too'
    $zstd = Get-Command zstd -ErrorAction SilentlyContinue
    if ($format -eq 'zstd' -and $zstd) {
        $plain = Join-Path $script:Work 'plain.img'
        $r = Invoke-Tool -File $zstd.Source -Arguments @('-q', '-d', '--long=31', '-f', $backup, '-o', $plain)
        if ($r.Code -ne 0) { Fail "zstd couldn't unpack the smart image: $(Tail $r.Err)" }
        if ((Get-Item $plain).Length -ne $script:DiskBytes) { Fail "zstd unpacked $((Get-Item $plain).Length) bytes" }
        Remove-Item -LiteralPath $plain
        Pass 'zstd unpacks the smart image into a whole disk image'
    } else {
        Write-Host "note: no zstd here (or a $format image), so that isn't checked"
    }

    # --- Raw writes, and the smart restore -----------------------------------------------
    Step 'dd onto a raw virtual disk, as the GUI runs it on Windows'
    $tgtVhdx = Join-Path $script:Work 'tgt.vhdx'
    $tgt = New-TestDisk $tgtVhdx
    $tgtPath = "\\.\PhysicalDrive$tgt"
    $garbage = Join-Path $script:Work 'garbage.bin'
    New-RandomFile $garbage $script:DiskBytes
    $garbageHash = (Get-FileHash -LiteralPath $garbage -Algorithm SHA256).Hash
    $drive = Get-ListedDrive $tgt
    Assert-Ours $tgt $tgtVhdx
    $r = Invoke-DdGui (@('dd', '--ddgui-worker', "--ddgui-sync=$tgtPath") + @(Get-LockArgs $drive) +
        @("if=$garbage", "of=$tgtPath", 'bs=4M', 'conv=notrunc,nocreat', 'status=progress'))
    if ($r.Code -ne 0) { Fail "dd onto $tgtPath failed: $(Tail $r.Err)" }
    Assert-Protocol $r.Out @('@ready', '@copy', '@sync') 'dd'
    if ((Get-DiskHash $tgt (Join-Path $script:Work 'readback.bin')) -ne $garbageHash) { Fail "$tgtPath doesn't hold what dd wrote" }
    Remove-Item -LiteralPath $garbage
    Pass "dd filled $tgtPath with garbage, and read it back the same"

    Step 'Smart restore (dd-gui copy --mode=restore) onto the disk full of garbage'
    $drive = Get-ListedDrive $tgt
    Assert-Ours $tgt $tgtVhdx
    $r = Invoke-DdGui (@('copy', '--ddgui-worker', "--ddgui-sync=$tgtPath") + @(Get-LockArgs $drive) +
        @('--mode=restore', "--from=$backup", "--to=$tgtPath"))
    if ($r.Code -ne 0) { Fail "smart restore failed (exit code $($r.Code)): $(Tail $r.Err)" }
    Assert-Protocol $r.Out @('@ready', '@copy', '@total', '@sync') 'smart restore'
    Pass "smart restore onto $tgtPath"
    Test-Volumes $tgt 'restored disk'

    # --- Sector by sector onto the disk, with its volumes locked -------------------------
    Step 'Sector by sector onto a disk with mounted volumes, locking them as the GUI does'
    $drive = Get-ListedDrive $tgt
    if (@($drive.lock).Count -lt 3) { Fail "dd-gui drives has $(@($drive.lock).Count) volumes to lock on $tgtPath, not 3" }
    Assert-Ours $tgt $tgtVhdx
    $r = Invoke-DdGui (@('dd', '--ddgui-worker', "--ddgui-sync=$tgtPath") + @(Get-LockArgs $drive) +
        @("if=$full", "of=$tgtPath", 'bs=4M', 'conv=notrunc,nocreat', 'status=progress'))
    if ($r.Code -ne 0) { Fail "dd onto $tgtPath failed: $(Tail $r.Err)" }
    if ((Get-DiskHash $tgt (Join-Path $script:Work 'readback.bin')) -ne $fullHash) { Fail "$tgtPath differs from the sector copy dd wrote" }
    Pass "dd cloned the source onto $tgtPath, identical sector by sector"
    Test-Volumes $tgt 'cloned disk'

    # --- Wiping --------------------------------------------------------------------------
    Step 'Wipe with dd-gui copy --mode=zeros (what the GUI does on Windows)'
    $drive = Get-ListedDrive $tgt
    Assert-Ours $tgt $tgtVhdx
    $r = Invoke-DdGui (@('copy', '--ddgui-worker', "--ddgui-sync=$tgtPath") + @(Get-LockArgs $drive) +
        @('--mode=zeros', "--to=$tgtPath"))
    if ($r.Code -ne 0) { Fail "copy --mode=zeros failed (exit code $($r.Code)): $(Tail $r.Err)" }
    Assert-Protocol $r.Out @('@ready', '@copy', '@total', '@sync') 'zeros'
    if ((Get-DiskHash $tgt (Join-Path $script:Work 'readback.bin')) -ne (Get-ZerosHash $script:DiskBytes)) { Fail "$tgtPath isn't all zeros" }
    Pass "copy --mode=zeros wiped $tgtPath"

    # --- Cancelling ----------------------------------------------------------------------
    # The GUI cancels a worker by closing its stdin. Each worker here waits for an answer to
    # --ask that never comes, so only the cancelling can end it.
    Step 'Cancelling'
    $never = Join-Path $script:Work 'never'
    $cancelled = @(
        @('closing stdin', @('--ddgui-watch-stdin')),
        @('a --ddgui-cancel-file that exists', @("--ddgui-cancel-file=$(Join-Path $script:Work 'cancel')", "--ddgui-answer-file=$never"))
    )
    Set-Content -Path (Join-Path $script:Work 'cancel') -Value ''
    $gone = Start-Process -FilePath (Join-Path $env:SystemRoot 'System32\cmd.exe') -ArgumentList '/c', 'exit' -PassThru -WindowStyle Hidden
    $gone.WaitForExit()
    $cancelled += , @('a --ddgui-parent that is gone', @("--ddgui-parent=$($gone.Id)", "--ddgui-answer-file=$never"))
    foreach ($case in $cancelled) {
        $r = Invoke-DdGui (@('copy', '--ddgui-worker') + $case[1] + @('--mode=smart', '--ask', "--from=$full", "--to=$(Join-Path $script:Work 'cancelled.img.zst')"))
        if ($r.Code -ne 130) { Fail "$($case[0]) should cancel the worker (exit code 130), not end it with $($r.Code): $(Tail $r.Err)" }
        Pass "$($case[0]) cancels the worker"
    }
} catch {
    Write-Host ($_.Exception.Message)
    if ($_.Exception.Message -notmatch '^FAIL') { Write-Host ($_.ScriptStackTrace) }
    $code = 1
} finally {
    foreach ($vhdx in @($script:Attached)) {
        try { Dismount-DiskImage -ImagePath $vhdx | Out-Null } catch { Write-Host "note: couldn't detach $vhdx`: $($_.Exception.Message)" }
    }
    if ($script:Work -and (Test-Path $script:Work)) {
        Remove-Item -Recurse -Force -LiteralPath $script:Work -ErrorAction SilentlyContinue
    }
}
if ($code -eq 0) {
    Write-Host ''
    Write-Host "E2E Windows: PASS ($($script:Passed) checks)"
} else {
    Write-Host ''
    Write-Host "E2E Windows: FAIL (after $($script:Passed) passed checks)"
}
exit $code
