# Provisions the Contr Windows sandbox as SYSTEM. Every step records a marker, so the script resumes after reboots.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$Root = 'C:\contr-sandbox'
$StateDir = Join-Path $Root 'state'
$DownloadDir = Join-Path $Root 'downloads'
$ToolsDir = 'C:\tools'
$RustDir = Join-Path $ToolsDir 'rust'
$TaskName = 'ContrSandboxProvision'
$Config = Get-Content -Raw -LiteralPath (Join-Path $Root 'contr-sandbox.json') | ConvertFrom-Json
$Script:CurrentStep = 'start'

$AdministratorsSid = '*S-1-5-32-544'
$UsersSid = '*S-1-5-32-545'
$SystemSid = '*S-1-5-18'

function Write-Status {
    param([string] $State, [string] $Message = '')

    $status = [ordered]@{
        state   = $State
        step    = $Script:CurrentStep
        message = $Message
        updated = (Get-Date).ToUniversalTime().ToString('o')
    }
    [IO.File]::WriteAllText((Join-Path $Root 'status.json'), ($status | ConvertTo-Json))
}

function Write-ProvisionLog {
    param([string] $Message)

    Write-Information $Message -InformationAction Continue
}

function Invoke-Step {
    param([string] $Name, [scriptblock] $Action)

    $marker = Join-Path $StateDir "$Name.done"
    if (Test-Path -LiteralPath $marker) {
        return
    }

    $Script:CurrentStep = $Name
    Write-Status 'running'
    Write-ProvisionLog "==> $Name"
    & $Action
    New-Item -ItemType File -Force -Path $marker | Out-Null
}

function Invoke-WithRetry {
    param([scriptblock] $Action, [int] $Attempts = 6)

    for ($attempt = 1; ; $attempt++) {
        try {
            return & $Action
        }
        catch {
            if ($attempt -ge $Attempts) {
                throw
            }
            $delay = [Math]::Min(60, 5 * [Math]::Pow(2, $attempt))
            Write-ProvisionLog "attempt $attempt failed: $($_.Exception.Message); retrying in $delay s"
            Start-Sleep -Seconds $delay
        }
    }
}

function Invoke-Native {
    param([string] $FilePath, [string[]] $Arguments = @(), [int[]] $SuccessCodes = @(0))

    & $FilePath @Arguments
    if ($SuccessCodes -notcontains $LASTEXITCODE) {
        throw "$FilePath exited with $LASTEXITCODE"
    }
}

function Invoke-Installer {
    param([string] $FilePath, [string[]] $Arguments)

    $process = Start-Process -FilePath $FilePath -ArgumentList $Arguments -Wait -PassThru -NoNewWindow
    if (@(0, 3010) -notcontains $process.ExitCode) {
        throw "$FilePath exited with $($process.ExitCode)"
    }
}

function Install-Msi {
    param([string] $Path, [string[]] $Properties = @())

    Invoke-Installer 'msiexec.exe' (@('/i', "`"$Path`"", '/qn', '/norestart') + $Properties)
}

function Get-File {
    param([string] $Url, [string] $Name = ([Uri] $Url).Segments[-1])

    $path = Join-Path $DownloadDir $Name
    if (-not (Test-Path -LiteralPath $path)) {
        $partial = "$path.partial"
        Invoke-WithRetry { Invoke-WebRequest -Uri $Url -OutFile $partial -UseBasicParsing }
        Move-Item -LiteralPath $partial -Destination $path -Force
    }

    return $path
}

function Get-GitHubAsset {
    param([string] $Repository, [string] $Pattern)

    $release = Invoke-WithRetry {
        Invoke-RestMethod -Uri "https://api.github.com/repos/$Repository/releases/latest" `
            -Headers @{ 'User-Agent' = 'contr-sandbox' }
    }
    $assets = @($release.assets | Where-Object { $_.name -match $Pattern })
    if ($assets.Count -ne 1) {
        throw "expected one asset of $Repository $($release.tag_name) matching $Pattern, found $($assets.Count)"
    }

    return Get-File -Url $assets[0].browser_download_url -Name $assets[0].name
}

function Find-VolumeFile {
    param([string] $Name)

    foreach ($drive in Get-PSDrive -PSProvider FileSystem) {
        $candidate = Join-Path $drive.Root $Name
        if (Test-Path -LiteralPath $candidate) {
            return $candidate
        }
    }
    throw "no drive contains $Name"
}

function Set-MachineVariable {
    param([string] $Name, [string] $Value)

    [Environment]::SetEnvironmentVariable($Name, $Value, 'Machine')
    Set-Item -Path "Env:$Name" -Value $Value
}

function Add-MachinePath {
    param([string] $Directory)

    $entries = [Environment]::GetEnvironmentVariable('Path', 'Machine') -split ';' | Where-Object { $_ }
    if ($entries -notcontains $Directory) {
        [Environment]::SetEnvironmentVariable('Path', (($entries + $Directory) -join ';'), 'Machine')
    }
    $env:Path = [Environment]::GetEnvironmentVariable('Path', 'Machine')
}

function Set-RegistryValue {
    param([string] $Path, [string] $Name, $Value, [string] $Type = 'DWord')

    if (-not (Test-Path -LiteralPath $Path)) {
        New-Item -Path $Path -Force | Out-Null
    }
    New-ItemProperty -Path $Path -Name $Name -Value $Value -PropertyType $Type -Force | Out-Null
}

function Get-CommandVersion {
    param([string] $FilePath, [string[]] $Arguments = @('--version'))

    try {
        return ((& $FilePath @Arguments 2>&1 | Select-Object -First 1) -as [string]).Trim()
    }
    catch {
        return "unavailable: $($_.Exception.Message)"
    }
}

New-Item -ItemType Directory -Force -Path $StateDir, $DownloadDir | Out-Null
Start-Transcript -LiteralPath (Join-Path $Root 'provision.log') -Append | Out-Null

try {
    Invoke-Step 'guest-tools' {
        Invoke-Installer (Find-VolumeFile 'virtio-win-guest-tools.exe') @('/install', '/quiet', '/norestart')
    }

    Invoke-Step 'system-settings' {
        Invoke-Native 'powercfg.exe' @('/change', 'standby-timeout-ac', '0')
        Invoke-Native 'powercfg.exe' @('/change', 'monitor-timeout-ac', '0')
        Invoke-Native 'powercfg.exe' @('/hibernate', 'off')

        Set-RegistryValue 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock' 'AllowDevelopmentWithoutDevLicense' 1
        Set-RegistryValue 'HKLM:\SYSTEM\CurrentControlSet\Control\FileSystem' 'LongPathsEnabled' 1
        Set-RegistryValue 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU' 'NoAutoRebootWithLoggedOnUsers' 1

        Set-RegistryValue 'HKLM:\SYSTEM\CurrentControlSet\Control\Terminal Server' 'fDenyTSConnections' 0
        Enable-NetFirewallRule -Group '@FirewallAPI.dll,-28752'
        Get-NetConnectionProfile | Set-NetConnectionProfile -NetworkCategory Private

        New-Item -ItemType Directory -Force -Path 'C:\src', $ToolsDir | Out-Null
        Add-MpPreference -ExclusionPath 'C:\src', $ToolsDir
    }

    Invoke-Step 'openssh' {
        $capability = Get-WindowsCapability -Online -Name 'OpenSSH.Server*'
        if ($capability.State -ne 'Installed') {
            Invoke-WithRetry { Add-WindowsCapability -Online -Name $capability.Name | Out-Null }
        }

        $keys = 'C:\ProgramData\ssh\administrators_authorized_keys'
        New-Item -ItemType Directory -Force -Path (Split-Path $keys) | Out-Null
        [IO.File]::WriteAllText($keys, "$($Config.authorizedKey)`n")
        Invoke-Native 'icacls.exe' @($keys, '/inheritance:r', '/grant', "${AdministratorsSid}:F", '/grant', "${SystemSid}:F")

        Set-Service -Name 'sshd' -StartupType Automatic
        Start-Service -Name 'sshd'
        if (-not (Get-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -ErrorAction SilentlyContinue)) {
            New-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -DisplayName 'OpenSSH Server (sshd)' `
                -Direction Inbound -Protocol TCP -LocalPort 22 -Action Allow | Out-Null
        }
    }

    Invoke-Step 'powershell' {
        Install-Msi (Get-GitHubAsset 'PowerShell/PowerShell' '^PowerShell-[\d.]+-win-x64\.msi$') `
            @('ADD_PATH=1', 'ENABLE_PSREMOTING=0', 'REGISTER_MANIFEST=1', 'USE_MU=0', 'ENABLE_MU=0')
        Set-RegistryValue 'HKLM:\SOFTWARE\OpenSSH' 'DefaultShell' "$env:ProgramFiles\PowerShell\7\pwsh.exe" 'String'
    }

    Invoke-Step 'timezone' {
        $iana = $Config.timeZone
        $script = "`$id = `$null; if ([TimeZoneInfo]::TryConvertIanaIdToWindowsId('$iana', [ref] `$id)) " +
            "{ Set-TimeZone -Id `$id } else { throw 'unknown time zone $iana' }"
        Invoke-Native "$env:ProgramFiles\PowerShell\7\pwsh.exe" @('-NoProfile', '-Command', $script)
    }

    Invoke-Step 'git' {
        Invoke-Installer (Get-GitHubAsset 'git-for-windows/git' '^Git-[\d.]+-64-bit\.exe$') `
            @('/VERYSILENT', '/NORESTART', '/NOCANCEL', '/SP-', '/SUPPRESSMSGBOXES')
        $git = "$env:ProgramFiles\Git\cmd\git.exe"
        Invoke-Native $git @('config', '--system', 'core.autocrlf', 'false')
        Invoke-Native $git @('config', '--system', 'core.longpaths', 'true')
    }

    Invoke-Step 'github-cli' {
        Install-Msi (Get-GitHubAsset 'cli/cli' '^gh_[\d.]+_windows_amd64\.msi$')
    }

    Invoke-Step 'dotnet' {
        $installDir = "$env:ProgramFiles\dotnet"
        & (Get-File 'https://dot.net/v1/dotnet-install.ps1') -Channel '10.0' -InstallDir $installDir -NoPath
        Add-MachinePath $installDir
        Set-MachineVariable 'DOTNET_CLI_TELEMETRY_OPTOUT' '1'
        Set-MachineVariable 'DOTNET_NOLOGO' '1'
    }

    Invoke-Step 'build-tools' {
        Invoke-Installer (Get-File 'https://aka.ms/vs/18/stable/vs_buildtools.exe' 'vs_buildtools.exe') `
            @('--quiet', '--wait', '--norestart', '--nocache',
              '--add', 'Microsoft.VisualStudio.Workload.VCTools', '--includeRecommended')
    }

    Invoke-Step 'rust' {
        New-Item -ItemType Directory -Force -Path $RustDir | Out-Null
        Invoke-Native 'icacls.exe' @($RustDir, '/grant', "${UsersSid}:(OI)(CI)M")
        Set-MachineVariable 'RUSTUP_HOME' (Join-Path $RustDir 'rustup')
        Set-MachineVariable 'CARGO_HOME' (Join-Path $RustDir 'cargo')

        $rustupInit = Get-File 'https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe'
        Invoke-Native $rustupInit @('-y', '--no-modify-path', '--profile', 'minimal',
            '--default-toolchain', 'stable', '--component', 'clippy,rustfmt')

        # rustup links its proxies symbolically when it may, and Windows refuses to execute them through the link.
        $bin = Join-Path $RustDir 'cargo\bin'
        $rustup = Join-Path $bin 'rustup.exe'
        foreach ($proxy in Get-ChildItem -LiteralPath $bin -Filter '*.exe' | Where-Object LinkType -eq 'SymbolicLink') {
            Remove-Item -LiteralPath $proxy.FullName
            New-Item -ItemType HardLink -Path $proxy.FullName -Target $rustup | Out-Null
        }
        Add-MachinePath $bin
    }

    Invoke-Step 'wsl' {
        foreach ($feature in 'VirtualMachinePlatform', 'Microsoft-Windows-Subsystem-Linux') {
            Enable-WindowsOptionalFeature -Online -FeatureName $feature -All -NoRestart | Out-Null
        }
        Install-Msi (Get-GitHubAsset 'microsoft/WSL' '^wsl\.[\d.]+\.x64\.msi$')
    }

    $rebootMarker = Join-Path $StateDir 'reboot.done'
    if (-not (Test-Path -LiteralPath $rebootMarker)) {
        New-Item -ItemType File -Force -Path $rebootMarker | Out-Null
        $Script:CurrentStep = 'reboot'
        Write-Status 'running'
        Restart-Computer -Force
        return
    }

    Invoke-Step 'verify' {
        $env:Path = [Environment]::GetEnvironmentVariable('Path', 'Machine')
        $env:WSL_UTF8 = '1'
        $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
        $windows = Get-CimInstance Win32_OperatingSystem

        $versions = [ordered]@{
            windows   = "$($windows.Caption) $($windows.Version)"
            sshd      = (Get-Service -Name 'sshd').Status.ToString()
            pwsh      = Get-CommandVersion "$env:ProgramFiles\PowerShell\7\pwsh.exe"
            git       = Get-CommandVersion 'git.exe'
            gh        = Get-CommandVersion 'gh.exe'
            dotnet    = Get-CommandVersion 'dotnet.exe'
            msvc      = Get-CommandVersion $vswhere @('-latest', '-products', '*', '-requires',
                'Microsoft.VisualStudio.Component.VC.Tools.x86.x64', '-property', 'catalog_productDisplayVersion')
            rustc     = Get-CommandVersion 'rustc.exe'
            cargo     = Get-CommandVersion 'cargo.exe'
            wsl       = Get-CommandVersion 'wsl.exe'
        }
        [IO.File]::WriteAllText((Join-Path $Root 'versions.json'), ($versions | ConvertTo-Json))
    }

    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
    Remove-Item -LiteralPath $DownloadDir -Recurse -Force
    $Script:CurrentStep = 'complete'
    Write-Status 'done'
}
catch {
    Write-Status 'failed' $_.Exception.Message
    throw
}
finally {
    Stop-Transcript | Out-Null
}
