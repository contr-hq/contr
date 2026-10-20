#Requires -RunAsAdministrator
# Runs once from FirstLogonCommands: copies provisioning onto the system disk and hands it to a SYSTEM task.
$ErrorActionPreference = 'Stop'

$root = 'C:\contr-sandbox'
$taskName = 'ContrSandboxProvision'

New-Item -ItemType Directory -Force -Path $root | Out-Null
foreach ($file in 'provision.ps1', 'contr-sandbox.json') {
    $copy = Copy-Item -LiteralPath (Join-Path $PSScriptRoot $file) -Destination $root -Force -PassThru
    $copy.IsReadOnly = $false
}

$action = New-ScheduledTaskAction -Execute 'powershell.exe' `
    -Argument "-NoProfile -ExecutionPolicy Bypass -File $root\provision.ps1"
$trigger = New-ScheduledTaskTrigger -AtStartup
$principal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -ExecutionTimeLimit (New-TimeSpan -Hours 4)

Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Principal $principal `
    -Settings $settings -Force | Out-Null
Start-ScheduledTask -TaskName $taskName
