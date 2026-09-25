<# :
@echo off
rem Builds DD-GUI on Windows, pacman style:
rem   1. checks the build dependencies and offers to install what's missing,
rem   2. downloads the crates,
rem   3. builds the release exe,
rem   4. puts it in dist\dd-gui.exe.
rem
rem   build.bat [--check] [--clean] [--noconfirm]
rem
rem Double-click it to build with the defaults. The rest of this file is Windows PowerShell
rem (5.1, part of every Windows 10 and 11): these lines only start it.
setlocal
set "DD_GUI_BUILD_SCRIPT=%~f0"
set "DD_GUI_BUILD_ARGS=%*"
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -Command "Invoke-Expression ([IO.File]::ReadAllText($env:DD_GUI_BUILD_SCRIPT))"
exit /b %errorlevel%
#>

# ---------------------------------------------------------------------------------------
# PowerShell from here on.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$Root = Split-Path -Parent $env:DD_GUI_BUILD_SCRIPT
$RustMin = [version]'1.93' # sevenz-rust2 1.93, Slint 1.92
$CargoToml = [IO.File]::ReadAllText((Join-Path $Root 'Cargo.toml'))
$Name = [regex]::Match($CargoToml, '(?m)^name = "([^"]+)"').Groups[1].Value
$Version = [regex]::Match($CargoToml, '(?m)^version = "([^"]+)"').Groups[1].Value
$Target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $Root 'target' }
$Dist = Join-Path $Root 'dist'
$CargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $HOME '.cargo' }
$ProgramFilesX86 = if (${env:ProgramFiles(x86)}) { ${env:ProgramFiles(x86)} } else { "$env:ProgramFiles" }
$Arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
$HostTriple = if ($Arch -eq 'ARM64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
$VcComponent = if ($Arch -eq 'ARM64') { 'Microsoft.VisualStudio.Component.VC.Tools.ARM64' } else { 'Microsoft.VisualStudio.Component.VC.Tools.x86.x64' }

$Check = $false; $Clean = $false; $NoConfirm = $false

# ---------------------------------------------------------------------------------------
# Looks: pacman's colors, "::" headers and progress bars.

$Tty = -not [Console]::IsOutputRedirected
$Colors = $false
if ($Tty -and -not $env:NO_COLOR) {
    try {
        Add-Type -Namespace DdGuiBuild -Name Native -MemberDefinition @'
[DllImport("kernel32.dll")] public static extern IntPtr GetStdHandle(int nStdHandle);
[DllImport("kernel32.dll")] public static extern bool GetConsoleMode(IntPtr handle, out uint mode);
[DllImport("kernel32.dll")] public static extern bool SetConsoleMode(IntPtr handle, uint mode);
'@
        $out = [DdGuiBuild.Native]::GetStdHandle(-11)
        $mode = [uint32]0
        # ENABLE_VIRTUAL_TERMINAL_PROCESSING: ANSI colors, as on Linux and macOS.
        if ([DdGuiBuild.Native]::GetConsoleMode($out, [ref]$mode)) {
            $Colors = [DdGuiBuild.Native]::SetConsoleMode($out, $mode -bor 4)
        }
    } catch { $Colors = $false }
}
$E = [char]27
if ($Colors) {
    $AllOff = "$E[0m"; $Bold = "$E[1m"; $Blue = "$E[1;34m"; $Red = "$E[1;31m"; $Yellow = "$E[1;33m"
} else {
    $AllOff = ''; $Bold = ''; $Blue = ''; $Red = ''; $Yellow = ''
}
$Cols = 80
if ($Tty) {
    try { $Cols = [Console]::WindowWidth } catch { $Cols = 80 }
    if ($Cols -lt 40) { $Cols = 80 }
}

function Write-Out([string]$Text) { [Console]::Out.Write($Text) }
function Section([string]$Text) { Write-Out ("{0}::{1}{2} {3}{1}`n" -f $Blue, $AllOff, $Bold, $Text) }
function Plain([string]$Text) { Write-Out " $Text`n" }
function Warn([string]$Text) { Write-Out ("{0}warning:{1} {2}`n" -f $Yellow, $AllOff, $Text) }
function Fail([string]$Text) { Write-Out ("{0}error:{1} {2}`n" -f $Red, $AllOff, $Text) }

function Die([string]$Text) { throw $Text }

# Ask "Proceed with installation?": true for yes. Enter means yes, as in pacman.
function Ask([string]$Question) {
    Write-Out ("{0}::{1}{2} {3} [Y/n] {1}" -f $Blue, $AllOff, $Bold, $Question)
    if ($script:NoConfirm) { Write-Out "`n"; return $true }
    if ([Console]::IsInputRedirected) {
        Write-Out "`n"
        Fail 'not a terminal, so nothing was installed (use --noconfirm to say yes)'
        return $false
    }
    $reply = [Console]::ReadLine()
    return ($null -ne $reply) -and ($reply -eq '' -or $reply -match '^[Yy]')
}

# One line of a bar, like pacman's: "(  5/709) compiling libc    [####-------]  42%".
function Progress([int]$I, [int]$N, [string]$Label) {
    if (-not $Tty) { return }
    if ($N -lt 1) { $N = 1 }
    if ($I -gt $N) { $I = $N }
    $pct = [int][Math]::Floor($I * 100 / $N)
    $width = $Cols - 1
    $infolen = [Math]::Max([int][Math]::Floor($width * 6 / 10), 50)
    $hashlen = $width - $infolen - 8
    $text = '({0}/{1}) {2}' -f $I.ToString().PadLeft("$N".Length), $N, $Label
    if ($text.Length -gt $infolen) { $text = $text.Substring(0, $infolen) }
    if ($hashlen -lt 5) {
        Write-Out ("`r{0} {1,3}%" -f $text.PadRight($infolen), $pct)
        return
    }
    $hash = [int][Math]::Floor($hashlen * $pct / 100)
    $bar = ('#' * $hash) + ('-' * ($hashlen - $hash))
    Write-Out ("`r{0} [{1}] {2,3}%" -f $text.PadRight($infolen), $bar, $pct)
}

function Progress-Done { if ($Tty) { Write-Out "`n" } }
function Hide-Cursor { if ($Tty) { try { [Console]::CursorVisible = $false } catch {} } }
function Show-Cursor { if ($Tty) { try { [Console]::CursorVisible = $true } catch {} } }

function Human-Size([long]$Bytes) {
    $units = 'B', 'KiB', 'MiB', 'GiB'
    $value = [double]$Bytes; $i = 0
    while ($value -ge 1024 -and $i -lt 3) { $value /= 1024; $i++ }
    if ($i -eq 0) { return "$Bytes B" }
    return ('{0:0.00} {1}' -f $value, $units[$i]).Replace(',', '.')
}

function Took([int]$Seconds) {
    if ($Seconds -ge 60) { return '{0}m {1:00}s' -f [Math]::Floor($Seconds / 60), ($Seconds % 60) }
    return "${Seconds}s"
}

# Runs a program and collects what it prints, without PowerShell's stream handling
# (Windows PowerShell turns a native program's stderr into errors).
function Run([string]$File, [string]$Arguments, [hashtable]$Vars = @{}) {
    $psi = New-Object Diagnostics.ProcessStartInfo
    $psi.FileName = $File
    $psi.Arguments = $Arguments
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.StandardOutputEncoding = [Text.Encoding]::UTF8
    $psi.StandardErrorEncoding = [Text.Encoding]::UTF8
    $psi.WorkingDirectory = $Root
    foreach ($k in $Vars.Keys) { $psi.EnvironmentVariables[$k] = $Vars[$k] }
    try { $p = [Diagnostics.Process]::Start($psi) } catch { return @{ Code = -1; Out = ''; Err = "$_" } }
    $err = $p.StandardError.ReadToEndAsync()
    $out = $p.StandardOutput.ReadToEnd()
    $p.WaitForExit()
    return @{ Code = $p.ExitCode; Out = $out; Err = $err.Result }
}

# Starts cargo with stderr joined to stdout (like 2>&1), to read line by line.
function Start-Cargo([string]$Arguments) {
    $psi = New-Object Diagnostics.ProcessStartInfo
    $psi.FileName = $env:ComSpec
    $psi.Arguments = "/d /s /c `"cargo $Arguments 2>&1`""
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.StandardOutputEncoding = [Text.Encoding]::UTF8
    $psi.WorkingDirectory = $Root
    return [Diagnostics.Process]::Start($psi)
}

# ---------------------------------------------------------------------------------------
# Build dependencies: Rust (MSVC), the Visual C++ build tools and a Windows SDK (rc.exe
# puts the icon and version info into dd-gui.exe).

function Find-Cargo {
    if (Get-Command cargo -ErrorAction SilentlyContinue) { return $true }
    # Installed by rustup, but this window's PATH predates it.
    $bin = Join-Path $CargoHome 'bin'
    if (Test-Path (Join-Path $bin 'cargo.exe')) { $env:PATH = "$bin;$env:PATH"; return $true }
    return $false
}

function Rust-Info {
    if (-not (Find-Cargo)) { return $null }
    $r = Run 'rustc' '-vV'
    if ($r.Code -ne 0) { return $null }
    $v = [regex]::Match($r.Out, '(?m)^release: (\d+\.\d+(\.\d+)?)').Groups[1].Value
    $h = [regex]::Match($r.Out, '(?m)^host: (\S+)').Groups[1].Value
    if (-not $v) { return $null }
    return @{ Version = [version]$v; Host = $h }
}

function Vswhere {
    if (-not $ProgramFilesX86) { return $null }
    $exe = Join-Path $ProgramFilesX86 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (Test-Path $exe) { return $exe }
    return $null
}

# The Visual Studio (Build Tools) installation with the C++ tools, if any.
function Msvc-Info {
    $vswhere = Vswhere
    if (-not $vswhere) { return $null }
    $r = Run $vswhere "-latest -products * -requires $VcComponent -format value -property installationVersion"
    $v = $r.Out.Trim()
    if ($r.Code -ne 0 -or -not $v) { return $null }
    $path = (Run $vswhere "-latest -products * -requires $VcComponent -format value -property installationPath").Out.Trim()
    return @{ Version = $v; Path = $path }
}

# The newest Windows 10/11 SDK that has rc.exe for this machine.
function Sdk-Version {
    $roots = @()
    foreach ($key in 'HKLM:\SOFTWARE\Microsoft\Windows Kits\Installed Roots', 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows Kits\Installed Roots') {
        try { $roots += (Get-ItemProperty -Path $key -Name KitsRoot10 -ErrorAction Stop).KitsRoot10 } catch {}
    }
    $binArch = if ($Arch -eq 'ARM64') { 'arm64' } else { 'x64' }
    foreach ($root in $roots) {
        $found = Get-ChildItem -Path (Join-Path $root 'bin') -Directory -Filter '10.*' -ErrorAction SilentlyContinue |
            Where-Object { Test-Path (Join-Path $_.FullName "$binArch\rc.exe") } |
            Sort-Object { [version]$_.Name } -Descending | Select-Object -First 1
        if ($found) { return $found.Name }
    }
    return $null
}

$script:Missing = @()
$script:RustPlan = ''
$script:VsPlan = ''
function Check-Deps([bool]$Print) {
    $script:Missing = @(); $script:RustPlan = ''; $script:VsPlan = ''
    function Row([string]$What, [string]$Value) { if ($Print) { Write-Out (" {0,-12} {1}`n" -f $What, $Value) } }
    function Missing-Row([string]$What, [string]$Why = 'missing') {
        if ($Print) { Write-Out (" {0,-12} {1}{2}{3}`n" -f $What, $Red, $Why, $AllOff) }
    }

    $rust = Rust-Info
    if ($rust -and $rust.Version -ge $RustMin -and $rust.Host -like '*-msvc') {
        Row 'rust' ('{0} ({1})' -f $rust.Version, $rust.Host)
    } else {
        $script:Missing += 'rust'
        if (-not $rust) {
            Missing-Row 'rust'
            $script:RustPlan = 'install'
        } elseif ($rust.Version -lt $RustMin) {
            Missing-Row 'rust' ('{0} ({1} or newer needed)' -f $rust.Version, $RustMin)
            $script:RustPlan = 'update'
        } else {
            Missing-Row 'rust' ('{0} ({1}: the MSVC toolchain is needed)' -f $rust.Version, $rust.Host)
            $script:RustPlan = 'update'
        }
    }

    $msvc = Msvc-Info
    if ($msvc) { Row 'msvc' $msvc.Version } else { Missing-Row 'msvc'; $script:Missing += 'msvc'; $script:VsPlan = 'install' }

    $sdk = Sdk-Version
    if ($sdk) { Row 'windows-sdk' $sdk } else {
        Missing-Row 'windows-sdk'
        $script:Missing += 'windows-sdk'
        if (-not $script:VsPlan) { $script:VsPlan = 'modify' }
    }
}

function Download([string]$Url, [string]$File) {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    Invoke-WebRequest -Uri $Url -OutFile $File -UseBasicParsing
}

function Install-BuildTools {
    Section 'Installing the Visual C++ build tools...'
    Plain 'The Visual Studio Installer asks for administrator rights, then shows its progress.'
    $vsArgs = @('--wait', '--passive', '--norestart', '--add', 'Microsoft.VisualStudio.Workload.VCTools', '--includeRecommended')
    if ($Arch -eq 'ARM64') { $vsArgs += @('--add', $VcComponent) }
    if ($script:VsPlan -eq 'modify') {
        # The C++ tools are there but no SDK: add the workload's recommended parts to them.
        $setup = Join-Path $ProgramFilesX86 'Microsoft Visual Studio\Installer\setup.exe'
        $vsArgs = @('modify', '--installPath', "`"$((Msvc-Info).Path)`"") + $vsArgs
    } else {
        $setup = Join-Path $env:TEMP 'vs_BuildTools.exe'
        Download 'https://aka.ms/vs/17/release/vs_BuildTools.exe' $setup
    }
    $p = Start-Process -FilePath $setup -ArgumentList $vsArgs -Wait -PassThru
    # 3010: installed, a restart is due some time.
    if ($p.ExitCode -ne 0 -and $p.ExitCode -ne 3010) { Die "the Visual Studio Installer stopped with code $($p.ExitCode)" }
}

function Install-Rust {
    Section 'Installing Rust (rustup)...'
    $init = Join-Path $env:TEMP 'rustup-init.exe'
    Download "https://static.rust-lang.org/rustup/dist/$HostTriple/rustup-init.exe" $init
    & $init -y --profile minimal --default-toolchain stable --default-host $HostTriple
    if ($LASTEXITCODE -ne 0) { Die "couldn't install Rust with rustup" }
    $env:PATH = "$(Join-Path $CargoHome 'bin');$env:PATH"
}

function Update-Rust {
    Section 'Updating Rust (rustup)...'
    & rustup toolchain install "stable-$HostTriple" --profile minimal --no-self-update
    if ($LASTEXITCODE -ne 0) { Die "couldn't update Rust" }
    # Build with it without changing the default toolchain.
    $env:RUSTUP_TOOLCHAIN = "stable-$HostTriple"
}

function Resolve-Deps {
    Section 'Checking build dependencies...'
    Check-Deps $true
    if ($script:Missing.Count -eq 0) { return }

    Write-Out "resolving dependencies...`n`n"
    $list = @()
    if ($script:VsPlan -eq 'install') { $list += 'vs-buildtools (Visual C++, Windows SDK)' }
    if ($script:VsPlan -eq 'modify') { $list += 'windows-sdk (Visual Studio Installer)' }
    if ($script:RustPlan -eq 'install') { $list += 'rust (rustup.rs)' }
    if ($script:RustPlan -eq 'update') { $list += "rust (rustup: stable-$HostTriple)" }
    Write-Out ("{0}Packages ({1}){2} {3}`n`n" -f $Bold, $list.Count, $AllOff, ($list -join '  '))
    if (-not (Ask 'Proceed with installation?')) { Die "missing build dependencies: $($script:Missing -join ' ')" }

    if ($script:VsPlan) { Install-BuildTools }
    if ($script:RustPlan -eq 'install') { Install-Rust }
    if ($script:RustPlan -eq 'update') {
        if (Get-Command rustup -ErrorAction SilentlyContinue) { Update-Rust } else { Install-Rust }
    }

    Check-Deps $false
    if ($script:Missing.Count -ne 0) { Die "still missing after installing: $($script:Missing -join ' ')" }
    Section 'Checking build dependencies...'
    Check-Deps $true
}

# ---------------------------------------------------------------------------------------
# Sources and the build itself.

function Crates-ToDownload {
    $cached = New-Object 'Collections.Generic.HashSet[string]'
    Get-ChildItem -Path (Join-Path $CargoHome 'registry\cache') -Filter '*.crate' -Recurse -ErrorAction SilentlyContinue |
        ForEach-Object { [void]$cached.Add($_.Name) }
    $n = 0; $name = ''; $ver = ''
    foreach ($line in [IO.File]::ReadAllLines((Join-Path $Root 'Cargo.lock'))) {
        if ($line -match '^name = "(.+)"$') { $name = $Matches[1] }
        elseif ($line -match '^version = "(.+)"$') { $ver = $Matches[1] }
        elseif ($line.StartsWith('source = "registry+') -and -not $cached.Contains("$name-$ver.crate")) { $n++ }
    }
    return $n
}

function Fetch-Sources {
    Section 'Retrieving sources...'
    $total = Crates-ToDownload
    if ($total -eq 0) {
        $all = @([IO.File]::ReadAllLines((Join-Path $Root 'Cargo.lock')) | Where-Object { $_.StartsWith('source = "registry+') }).Count
        Plain "all $all crates are here already"
        return
    }
    $other = New-Object Collections.Generic.List[string]
    $i = 0
    Hide-Cursor
    $p = Start-Cargo 'fetch --locked'
    while ($null -ne ($line = $p.StandardOutput.ReadLine())) {
        if ($line -match 'Downloaded\s+(\S+)\s+v') {
            $i++
            Progress $i $total "downloading $($Matches[1])"
        } else { $other.Add($line) }
    }
    $p.WaitForExit()
    if ($p.ExitCode -eq 0) {
        Progress $total $total "downloaded $i crates"
        Progress-Done
        Show-Cursor
    } else {
        Progress-Done
        Show-Cursor
        $other | ForEach-Object { Write-Out "$_`n" }
        Die "couldn't download the crates"
    }
}

# Exactly how many units cargo will build and build scripts it will run (the unit graph is
# an unstable cargo flag, so it can go away: then this is 0 and the bar guesses).
function Count-Units([string]$Arguments) {
    $r = Run 'cargo' "$Arguments --unit-graph -Z unstable-options" @{ RUSTC_BOOTSTRAP = '1' }
    if ($r.Code -ne 0) { return 0 }
    return ([regex]::Matches($r.Out, '"mode":"(build|run-custom-build|test)"')).Count
}

# Runs cargo with a progress bar instead of its output, which goes to target\<what>.log.
# Shows the errors if it fails.
function Cargo-WithBar([string]$What, [string]$Arguments) {
    $log = Join-Path $Target "$What.log"
    New-Item -ItemType Directory -Force -Path $Target | Out-Null
    $total = Count-Units $Arguments
    if ($total -le 0) { $total = 600 }
    $lines = New-Object Collections.Generic.List[string]
    $inflight = New-Object Collections.Generic.List[string]
    $current = $Name; $i = 0
    $color = if ($Colors) { 'always' } else { 'never' }
    Hide-Cursor
    Progress 0 $total 'compiling'
    $p = Start-Cargo "$Arguments --message-format=json-render-diagnostics --color $color"
    while ($null -ne ($line = $p.StandardOutput.ReadLine())) {
        if ($line.StartsWith('{"reason":"compiler-artifact"') -or $line.StartsWith('{"reason":"build-script-executed"')) {
            $i++
            if ($i -gt $total) { $total = $i }
            # A crate is done once its library or binary is (not its build script).
            if ($line.StartsWith('{"reason":"compiler-artifact"') -and -not $line.Contains('"kind":["custom-build"]')) {
                $m = [regex]::Match($line, '"package_id":"[^"]*#([^"@]+)@')
                if (-not $m.Success) { $m = [regex]::Match($line, '"package_id":"[^"]*/([^"/#]+)#') }
                if ($m.Success) { [void]$inflight.Remove($m.Groups[1].Value) }
                $current = if ($inflight.Count) { $inflight[$inflight.Count - 1] } else { $Name }
            }
            Progress $i $total "compiling $current"
        } elseif ($line.StartsWith('{')) {
        } else {
            $lines.Add($line)
            $m = [regex]::Match($line, 'Compiling(\x1b\[[0-9;]*m)*\s+([\w-]+)\s+v')
            if ($m.Success) {
                $current = $m.Groups[2].Value
                $inflight.Add($current)
                Progress $i $total "compiling $current"
            }
        }
    }
    $p.WaitForExit()
    [IO.File]::WriteAllLines($log, $lines)
    $shown = $log.Replace("$Root\", '')
    if ($p.ExitCode -eq 0) {
        Progress $total $total "compiling $Name"
        Progress-Done
        Show-Cursor
        # cargo sums them up per crate: "`dd-gui` (bin "dd-gui") generated 2 warnings".
        $warnings = 0
        foreach ($m in [regex]::Matches(($lines -join "`n"), 'generated (\d+) warnings?')) { $warnings += [int]$m.Groups[1].Value }
        if ($warnings) { Warn "the compiler printed $warnings warnings (see $shown)" }
    } else {
        Progress-Done
        Show-Cursor
        # The compiler's messages, without cargo's "Compiling ..." lines.
        $status = '^\s*(\x1b\[[0-9;]*m)*\s*(Compiling|Checking|Fresh|Finished|Running|Downloaded|Downloading|Blocking|Locking|Updating)\W'
        $lines | Where-Object { $_ -notmatch $status } | ForEach-Object { Write-Out "$_`n" }
        Die "the build failed (full log: $shown)"
    }
}

function Build-Release {
    Section "Building $Name $Version..."
    Cargo-WithBar 'build' 'build --release --locked'
}

function Run-Tests {
    Section 'Building the tests...'
    Cargo-WithBar 'tests' 'test --release --locked --no-run'
    Section 'Running the tests...'
    & cargo test --release --locked --quiet
    if ($LASTEXITCODE -ne 0) { Die 'some tests failed' }
}

function Package-Exe {
    Section 'Packaging...'
    New-Item -ItemType Directory -Force -Path $Dist | Out-Null
    $exe = Join-Path $Dist "$Name.exe"
    Copy-Item -Force (Join-Path $Target "release\$Name.exe") $exe
    $info = (Get-Item $exe).VersionInfo
    Write-Out (" {0,-40} {1}`n" -f "dist\$Name.exe", (Human-Size (Get-Item $exe).Length))
    if ($info.ProductName -ne 'DD-GUI') { Warn "$Name.exe has no version info or icon (was rc.exe found?)" }
}

function Usage {
    Write-Out @"
Usage: build.bat [options]

Builds $Name $Version for Windows into dist\$Name.exe. Missing build dependencies (Rust
$RustMin or newer, the Visual C++ build tools, a Windows SDK) are installed after asking.

Options:
  -c, --check       also build and run the tests (needs an administrator prompt)
  -C, --clean       delete the previous build first
      --noconfirm   install missing dependencies without asking
  -h, --help        show this help

"@
}

# Double-clicked in Explorer: keep the window open to read the result.
function Started-FromExplorer {
    try {
        $me = Get-CimInstance Win32_Process -Filter "ProcessId=$PID"
        $cmd = Get-CimInstance Win32_Process -Filter "ProcessId=$($me.ParentProcessId)"
        $parent = Get-CimInstance Win32_Process -Filter "ProcessId=$($cmd.ParentProcessId)"
        return $parent.Name -ieq 'explorer.exe'
    } catch { return $false }
}

function Main {
    foreach ($arg in (($env:DD_GUI_BUILD_ARGS -split '\s+') | ForEach-Object { $_.Trim('"') } | Where-Object { $_ })) {
        # -c and -C differ, so case-sensitive.
        if ($arg -cin '-c', '--check', '/check') { $script:Check = $true }
        elseif ($arg -cin '-C', '--clean', '/clean') { $script:Clean = $true }
        elseif ($arg -cin '--noconfirm', '/noconfirm') { $script:NoConfirm = $true }
        elseif ($arg -cin '-h', '--help', '/?', '/h', '/help') { Usage; return }
        else { Usage; Die "unknown option: $arg" }
    }
    if ($script:Check) {
        $admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
            [Security.Principal.WindowsBuiltInRole]::Administrator)
        # The test build carries dd-gui.exe's manifest, which asks for administrator rights.
        if (-not $admin) { Die '--check needs an administrator prompt: the tests run with dd-gui.exe''s admin manifest' }
    }
    Set-Location $Root
    $start = Get-Date

    Resolve-Deps
    if ($script:Clean) {
        Section 'Removing the previous build...'
        & cargo clean --quiet
    }
    Fetch-Sources
    Build-Release
    if ($script:Check) { Run-Tests }
    Package-Exe
    Section ("Finished {0} {1} in {2}: dist\{0}.exe" -f $Name, $Version, (Took ([int]((Get-Date) - $start).TotalSeconds)))
}

$code = 0
$savedEncoding = [Console]::OutputEncoding
try {
    try { [Console]::OutputEncoding = [Text.Encoding]::UTF8 } catch {}
    Main
} catch {
    Fail "$($_.Exception.Message)"
    $code = 1
} finally {
    Show-Cursor
    try { [Console]::OutputEncoding = $savedEncoding } catch {}
}
if (-not $env:CI -and (Started-FromExplorer)) {
    Write-Out "`n"
    Read-Host 'Press Enter to close' | Out-Null
}
exit $code
