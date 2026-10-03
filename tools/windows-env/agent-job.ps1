# maxb35t fork: the in-VM job runner for engine agent jobs on Windows (engine ADR 0020, Decision item 4).
# Runs inside a throwaway instance (environments `windows` and `windows-desktop`), copied in per job with
# its inputs in C:\job\in: job.json, prompt.txt and cred.env (deleted as soon as it is read).
#
# It applies the rules of the engine's launcher (ci/host/engine-agent/launcher.ps1): the same job.json
# fields, patterns and size bounds, a fresh clone in a fresh workdir, `claude -p` with the job's model
# alias and effort, a timeout, and status/result records (C:\job\out). In addition it refuses a job
# unless Claude Code is the version the job names. The instance itself is the boundary: it is deleted
# after the job, so there is no identity, job object or environment strip to manage. Its records are
# untrusted (the job runs as the same administrator): the driver reads them bounded and takes the
# job's outcome from GitHub.
#
# The last line on standard output is the final status as one JSON object.
param([string]$Dir = 'C:\job\in')
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

# The engine launcher's config (ci/host/engine-agent/launcher-config.json).
$Repo = 'https://github.com/maxb35t/engine'
$Models = @('opus', 'sonnet')
$Efforts = @('low', 'medium', 'high', 'xhigh', 'max')
$Agents = @('none')
$MaxTimeout = 10800
$Out = 'C:\job\out'
$Fields = @('agent', 'claude_version', 'effort', 'job_id', 'model', 'timeout_s', 'workdir')
$CredKeys = @('CLAUDE_CODE_OAUTH_TOKEN', 'GH_TOKEN')

function Write-Json([string]$Path, [hashtable]$Obj) {
    [IO.File]::WriteAllText($Path, (ConvertTo-Json -InputObject $Obj -Depth 4 -Compress), (New-Object Text.UTF8Encoding $false))
}
function Get-Stamp { (Get-Date).ToUniversalTime().ToString('o') }
function Read-Bounded([string]$Path, [int]$Cap, [string]$Name) {
    if (-not (Test-Path -LiteralPath $Path)) { throw ('REJECT: no ' + $Name) }
    $b = [IO.File]::ReadAllBytes($Path)
    if ($b.Length -gt $Cap) { throw ('REJECT: ' + $Name + ' larger than ' + $Cap + ' bytes') }
    return , $b
}

$status = @{ state = 'setup'; started = (Get-Stamp) }
$credPath = Join-Path $Dir 'cred.env'
$launched = $false
try {
    $jobBytes = Read-Bounded (Join-Path $Dir 'job.json') 4096 'job.json'
    $promptPath = Join-Path $Dir 'prompt.txt'
    $promptBytes = Read-Bounded $promptPath 262144 'prompt.txt'
    if ($promptBytes.Length -eq 0) { throw 'REJECT: prompt.txt is empty' }
    $credText = [Text.Encoding]::UTF8.GetString((Read-Bounded $credPath 16384 'cred.env'))
    Remove-Item -LiteralPath $credPath -Force

    try { $job = ConvertFrom-Json ((New-Object Text.UTF8Encoding($false, $true)).GetString($jobBytes)) }
    catch { throw ('REJECT: job.json is not strict UTF-8 JSON (' + $_.Exception.GetType().FullName + ')') }
    if (-not ($job -is [System.Management.Automation.PSCustomObject])) { throw 'REJECT: job.json must be one JSON object' }
    $have = @($job.PSObject.Properties | ForEach-Object { $_.Name })
    if ($have.Count -ne $Fields.Count) { throw ('REJECT: fields must be exactly ' + ($Fields -join ',')) }
    foreach ($f in $Fields) { if (-not ($have -ccontains $f)) { throw ('REJECT: fields must be exactly ' + ($Fields -join ',')) } }
    foreach ($k in 'agent', 'claude_version', 'effort', 'job_id', 'model', 'workdir') { if (-not ($job.$k -is [string])) { throw ('REJECT: ' + $k + ' must be a string') } }
    if (-not ($job.timeout_s -is [int] -or $job.timeout_s -is [long])) { throw 'REJECT: timeout_s must be an integer' }
    if ($job.job_id -cnotmatch '^[a-z0-9][a-z0-9-]{0,63}\z') { throw 'REJECT: job_id does not match the pattern' }
    if ($job.workdir -cnotmatch '^[a-z0-9][a-z0-9-]{0,39}\z') { throw 'REJECT: workdir does not match the pattern' }
    $reserved = '^(con|prn|aux|nul|com[0-9]|lpt[0-9])\z'
    if ($job.job_id -match $reserved -or $job.workdir -match $reserved) { throw 'REJECT: a Windows reserved name' }
    if ($job.agent -cnotmatch '^[a-z0-9][a-z0-9-]{0,63}\z') { throw 'REJECT: agent does not match the pattern' }
    if ($job.claude_version -cnotmatch '^[0-9]{1,4}\.[0-9]{1,4}\.[0-9]{1,6}\z') { throw 'REJECT: claude_version does not match the pattern' }
    if (-not ($Models -ccontains $job.model)) { throw 'REJECT: model not allowed' }
    if (-not ($Efforts -ccontains $job.effort)) { throw 'REJECT: effort not allowed' }
    if (-not ($Agents -ccontains $job.agent)) { throw 'REJECT: agent not allowed' }
    if ($job.timeout_s -lt 60 -or $job.timeout_s -gt $MaxTimeout) { throw 'REJECT: timeout_s out of range' }
    $status.job_id = $job.job_id

    # Exactly the two credentials, KEY=VALUE.
    $cred = @{}
    foreach ($l in ($credText -split "`r?`n")) {
        if ($l -eq '') { continue }
        if ($l -cnotmatch '^([A-Z_]+)=([\x21-\x7e]+)\z' -or -not ($CredKeys -ccontains $matches[1]) -or $cred.ContainsKey($matches[1])) {
            throw 'REJECT: cred.env must hold exactly CLAUDE_CODE_OAUTH_TOKEN and GH_TOKEN'
        }
        $cred[$matches[1]] = $matches[2]
    }
    if ($cred.Count -ne $CredKeys.Count) { throw 'REJECT: cred.env must hold exactly CLAUDE_CODE_OAUTH_TOKEN and GH_TOKEN' }
    $credText = $null

    if (Test-Path -LiteralPath $Out) { throw 'REJECT: C:\job\out already exists (job_id already used)' }
    New-Item -ItemType Directory -Path $Out | Out-Null
    [IO.File]::WriteAllBytes((Join-Path $Out 'job.json'), $jobBytes)
    Write-Json (Join-Path $Out 'status.json') $status

    $env:DISABLE_AUTOUPDATER = '1'
    $claude = (Get-Command claude -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source
    $v = (& $claude --version 2>&1 | Select-Object -First 1) -as [string]
    $haveVer = ($v.Trim() -split '\s+')[0]
    if ($haveVer -cne $job.claude_version) { throw ('REJECT: Claude Code is ' + $haveVer + ', the job requires ' + $job.claude_version) }
    $status.claude_version = $haveVer

    # The job's environment. Certificate revocation can't be checked from an instance (no network
    # adapter, so Windows never fetches revocation data): cargo and git skip it, curl is best-effort.
    $env:CLAUDE_CODE_OAUTH_TOKEN = $cred['CLAUDE_CODE_OAUTH_TOKEN']
    $env:GH_TOKEN = $cred['GH_TOKEN']
    $cred = $null
    $env:GIT_TERMINAL_PROMPT = '0'; $env:GCM_INTERACTIVE = 'never'
    $env:GIT_CONFIG_COUNT = '2'
    $env:GIT_CONFIG_KEY_0 = 'http.schannelCheckRevoke'; $env:GIT_CONFIG_VALUE_0 = 'false'
    $env:GIT_CONFIG_KEY_1 = 'credential.https://github.com.helper'
    $env:GIT_CONFIG_VALUE_1 = '!f() { test "$1" = get && echo username=x-access-token && echo "password=$GH_TOKEN"; }; f'
    $env:CARGO_HTTP_CHECK_REVOKE = 'false'
    $env:CURL_HOME = 'C:\job\.curl'
    New-Item -ItemType Directory -Force $env:CURL_HOME | Out-Null
    foreach ($n in '.curlrc', '_curlrc') { Set-Content -Path (Join-Path $env:CURL_HOME $n) -Value 'ssl-revoke-best-effort' -Encoding ascii }

    $wd = 'C:\job\' + $job.workdir
    if (Test-Path -LiteralPath $wd) { throw 'REJECT: workdir already exists' }
    $ErrorActionPreference = 'Continue'
    $o = & git clone --quiet $Repo $wd 2>&1
    $gitExit = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    [IO.File]::WriteAllText((Join-Path $Out 'setup.log'), ('git clone exit=' + $gitExit + ' : ' + ((@($o) | ForEach-Object { [string]$_ }) -join ' | ')), (New-Object Text.UTF8Encoding $false))
    if ($gitExit -ne 0) { throw ('git clone failed exit=' + $gitExit) }

    $argv = @('-p', '--model', $job.model, '--effort', $job.effort)
    if ($job.agent -cne 'none') { $argv += @('--agent', $job.agent) }
    $argv += @('--permission-mode', 'bypassPermissions', '--permission-prompts', 'none', '--strict-mcp-config', '--no-session-persistence', '--output-format', 'json')
    $launched = $true
    $p = Start-Process -FilePath $claude -ArgumentList ($argv -join ' ') -WorkingDirectory $wd -RedirectStandardInput $promptPath -RedirectStandardOutput (Join-Path $Out 'result.json') -RedirectStandardError (Join-Path $Out 'stderr.txt') -NoNewWindow -PassThru
    $null = $p.Handle
    $status.state = 'running'; $status.pid = $p.Id
    Write-Json (Join-Path $Out 'status.json') $status
    if (-not $p.WaitForExit([int]$job.timeout_s * 1000)) {
        $status.state = 'timeout'
        $null = & taskkill.exe /T /F /PID $p.Id 2>&1
    } else {
        $p.WaitForExit()
        $status.state = 'done'; $status.exit_code = $p.ExitCode
    }
} catch {
    $m = [string]$_.Exception.Message; if ($m.Length -gt 300) { $m = $m.Substring(0, 300) }
    $status.state = 'error'; if ($m.StartsWith('REJECT:')) { $status.state = 'rejected' }
    $status.message = $m
} finally {
    if (Test-Path -LiteralPath $credPath) { Remove-Item -LiteralPath $credPath -Force -ErrorAction SilentlyContinue }
    Remove-Item Env:CLAUDE_CODE_OAUTH_TOKEN, Env:GH_TOKEN -ErrorAction SilentlyContinue
    $status.finished = (Get-Stamp)
    $status.launched = $launched
    if (Test-Path -LiteralPath $Out) { try { Write-Json (Join-Path $Out 'status.json') $status } catch {} }
    Write-Output (ConvertTo-Json -InputObject $status -Depth 4 -Compress)
}
if ($status.state -eq 'done') { exit 0 } elseif ($status.state -eq 'rejected') { exit 2 } else { exit 1 }
