#Requires -Version 7.0
[CmdletBinding()]
param(
  [string]$Target = 'local',
  [ValidateSet('stable', 'dev')][string]$Channel = 'stable',
  [string]$NasRoot = '\\NelsonNAS\files\shows-app',
  [string]$ResultPath
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

function Invoke-Native([string]$File, [string[]]$NativeArguments) {
  $start = [Diagnostics.ProcessStartInfo]::new($File)
  $start.UseShellExecute = $false
  $start.CreateNoWindow = $true
  $start.RedirectStandardOutput = $true
  $start.RedirectStandardError = $true
  foreach ($argument in $NativeArguments) { $start.ArgumentList.Add($argument) }
  $process = [Diagnostics.Process]::Start($start)
  $output = $process.StandardOutput.ReadToEndAsync()
  $errors = $process.StandardError.ReadToEndAsync()
  if (-not $process.WaitForExit(120000)) { $process.Kill($true); throw "$File timed out" }
  if ($process.ExitCode -ne 0) { throw "$File failed: $($errors.GetAwaiter().GetResult())" }
  $output.GetAwaiter().GetResult()
}

if ($Target -ne 'local') {
  if ($Target -notmatch '^[A-Za-z0-9][A-Za-z0-9.-]{0,252}$') { throw 'Target must be a computer hostname or SSH alias' }
  $request = @{
    channel = $Channel
    nas = $NasRoot
    id = [guid]::NewGuid().ToString('N')
  } | ConvertTo-Json -Compress
  $requestObject = $request | ConvertFrom-Json
  $remoteTemp = (Invoke-Native 'ssh.exe' @('-T','-o','BatchMode=yes','-o','StrictHostKeyChecking=yes','-o','ConnectTimeout=8',$Target,'echo %TEMP%')).Trim()
  if ($remoteTemp -notmatch '^[A-Za-z]:\\[^\r\n]+$') { throw 'Target did not provide a Windows temporary directory' }
  $remoteScript = $remoteTemp.TrimEnd('\') + '\shows-update-' + $requestObject.id + '.ps1'
  Invoke-Native 'scp.exe' @('-q','-o','BatchMode=yes','-o','StrictHostKeyChecking=yes','-o','ConnectTimeout=8',$PSCommandPath, ('{0}:{1}' -f $Target, $remoteScript.Replace('\','/'))) | Out-Null
  $request64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($request))
  $bootstrap = @'
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$request = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('REQUEST64')) | ConvertFrom-Json
$script = Join-Path $env:TEMP ('shows-update-' + $request.id + '.ps1')
$result = $script + '.json'
$name = 'Shows update ' + $request.id
$arguments = '-NoProfile -NonInteractive -WindowStyle Hidden -File "' + $script + '" -Channel ' + $request.channel + ' -NasRoot "' + $request.nas + '" -ResultPath "' + $result + '"'
$action = New-ScheduledTaskAction -Execute 'C:\Program Files\PowerShell\7\pwsh.exe' -Argument $arguments
$principal = New-ScheduledTaskPrincipal -UserId ($env:COMPUTERNAME + '\' + $env:USERNAME) -LogonType Interactive -RunLevel Limited
Register-ScheduledTask -TaskName $name -Action $action -Principal $principal -Force | Out-Null
Start-ScheduledTask -TaskName $name
try {
  $deadline = (Get-Date).AddSeconds(90)
  while (-not (Test-Path -LiteralPath $result)) {
    if ((Get-Date) -gt $deadline) { throw 'Interactive update timed out; inspect the owned Shows update task' }
    Start-Sleep -Milliseconds 250
  }
  Get-Content -Raw -LiteralPath $result
} finally {
  if (Test-Path -LiteralPath $result) {
    Unregister-ScheduledTask -TaskName $name -Confirm:$false
  }
}
'@
  $bootstrap = $bootstrap.Replace('REQUEST64', $request64)
  $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($bootstrap))
  $output = Invoke-Native 'ssh.exe' @('-T','-o','BatchMode=yes','-o','StrictHostKeyChecking=yes','-o','ConnectTimeout=8',$Target,'"C:\Program Files\PowerShell\7\pwsh.exe"','-NoProfile','-NonInteractive','-EncodedCommand',$encoded)
  $result = $output | ConvertFrom-Json
  if (-not $result.ok) { throw "$Target update failed: $($result.message)" }
  $result | ConvertTo-Json -Depth 6
  return
}

try {
  $install = Join-Path $env:LOCALAPPDATA 'shows'
  New-Item -ItemType Directory -Force -Path $install | Out-Null
  $app = Get-Process -Name shows-desktop -ErrorAction SilentlyContinue | Select-Object -First 1
  $before = $null
  if ($app) {
    foreach ($port in @(Get-NetTCPConnection -State Listen -OwningProcess $app.Id | Select-Object -ExpandProperty LocalPort)) {
      try {
        $candidate = Invoke-RestMethod -Uri "http://127.0.0.1:$port/status" -TimeoutSec 2
        if ($candidate.phase) { $before = $candidate; $oldPort = $port; break }
      } catch { }
    }
    if (-not $before) { throw 'Running Shows instance did not provide a status snapshot; it was left running' }
  }
  & (Join-Path $NasRoot 'install-shows-launcher.ps1') -Channel $Channel -NasRoot $NasRoot
  if (-not (Test-Path -LiteralPath (Join-Path $install "channels\$Channel\shows-desktop.exe"))) { throw 'Verified desktop entry point is missing' }
  $state = Get-Content -Raw -LiteralPath (Join-Path $install "launcher-state-$Channel.json") | ConvertFrom-Json
  $manifest = Get-Content -Raw -LiteralPath (Join-Path $NasRoot "channels\$Channel.json") | ConvertFrom-Json
  if ($state.current -ne $manifest.version) { throw 'The selected version does not match the channel' }

  $after = $null
  $info = $null
  if ($app) {
    # Window close commits the resume position before the old process exits.
    Invoke-RestMethod -Uri "http://127.0.0.1:$oldPort/window/close" -Method Post -TimeoutSec 5 | Out-Null
    if (-not $app.WaitForExit(10000)) { throw 'Shows did not close gracefully; the new copy was not started' }
    $directory = Join-Path $install ('versions\' + $state.current)
    $newApp = Start-Process -FilePath (Join-Path $directory 'shows-desktop.exe') -WorkingDirectory $directory -PassThru
    $deadline = (Get-Date).AddSeconds(30)
    do {
      try {
        $after = Invoke-RestMethod -Uri 'http://127.0.0.1:45831/status' -TimeoutSec 1
        if ($after.phase -eq 'playing' -and $null -ne $after.playback.time_pos) {
          $wanted = $before.round[$before.round_pos]
          if ($after.round[$after.round_pos].episode_id -ne $wanted.episode_id) {
            if (-not @($after.round | Where-Object episode_id -eq $wanted.episode_id).Count) { throw 'The selected episode is no longer in the shared round' }
            Invoke-RestMethod -Uri 'http://127.0.0.1:45831/remote/control' -Method Post -ContentType 'application/json' -Body (@{action='play_show';show_id=$wanted.show_id} | ConvertTo-Json -Compress) -TimeoutSec 3 | Out-Null
            $after = Invoke-RestMethod -Uri 'http://127.0.0.1:45831/status' -TimeoutSec 2
            if ($after.round[$after.round_pos].episode_id -ne $wanted.episode_id) { Start-Sleep -Milliseconds 100; continue }
          }
          if ($before.playback.paused) {
            Invoke-RestMethod -Uri 'http://127.0.0.1:45831/remote/control' -Method Post -ContentType 'application/json' -Body '{"action":"pause"}' -TimeoutSec 3 | Out-Null
          }
          $after = Invoke-RestMethod -Uri 'http://127.0.0.1:45831/status' -TimeoutSec 2
          $offset = [double]$before.playback.time_pos - [double]$after.playback.time_pos
          if ([Math]::Abs($offset) -gt 1) {
            Invoke-RestMethod -Uri 'http://127.0.0.1:45831/remote/control' -Method Post -ContentType 'application/json' -Body (@{action='seek';seconds=$offset} | ConvertTo-Json -Compress) -TimeoutSec 3 | Out-Null
            $after = Invoke-RestMethod -Uri 'http://127.0.0.1:45831/status' -TimeoutSec 2
          }
          $info = Invoke-RestMethod -Uri 'http://127.0.0.1:45831/remote/info' -TimeoutSec 2
          break
        }
      } catch { }
      if ($newApp.HasExited) { throw 'Updated Shows exited during startup; inspect shows.log' }
      Start-Sleep -Milliseconds 100
    } while ((Get-Date) -lt $deadline)
    if (-not $info) { throw 'Updated Shows did not become playable within 30 seconds' }
    if ($after.playback.paused -ne $before.playback.paused) { throw 'Updated Shows did not retain the pause state' }
    if ($after.round[$after.round_pos].episode_id -ne $before.round[$before.round_pos].episode_id) { throw 'Updated Shows did not retain the selected episode' }
  }
  $result = [ordered]@{
    ok = $true
    computer = $env:COMPUTERNAME
    version = $state.current
    restarted = [bool]$app
    build = $info.build
    paused = $after.playback.paused
    before_episode = $(if ($before) { $before.round[$before.round_pos].episode_id } else { $null })
    episode = $(if ($after) { $after.round[$after.round_pos].episode_id } else { $null })
    time = $after.playback.time_pos
  }
} catch {
  $result = @{ok=$false; computer=$env:COMPUTERNAME; message=$_.Exception.Message}
}
$json = $result | ConvertTo-Json -Depth 6
if ($ResultPath) {
  $temp = $ResultPath + '.tmp'
  [IO.File]::WriteAllText($temp, $json, [Text.UTF8Encoding]::new($false))
  Move-Item -LiteralPath $temp -Destination $ResultPath -Force
} else { $json }
if (-not $result.ok) { throw $result.message }
