# 只查暂存区，只依赖 git + pwsh 7
# 启用：git config --local core.hooksPath .githooks
$ErrorActionPreference = 'Stop'

# ---------- 按本仓库调整 ----------
$AbsPath     = '(?i)([A-Za-z]:[\\/]|\\\\|\b/(home|Users)/[A-Za-z0-9._-]+/|file://)'
# 本仓库允许的通用/占位路径：占位符、系统临时目录、测试用驱动路径
$AbsAllow    = '(?i)(%USERPROFILE%|%APPDATA%|%LOCALAPPDATA%|%TEMP%|\$HOME|~/|<repo>|<user>|<hostname>|<device|C:\\+Windows)'
$ProcName    = '(?i)(handoff|交接|笔记|草稿|调研|临时|scratch|wip|todo|worklog|session|git-tree|branch-tree)'
$ProcDir     = '(?i)(^|/)(docs/plans|docs/research|notes|scratch|handover|logs|screenshots|\.screenshots)(/|$)'
$ArtifactDir = '(?i)(^|/)(target|node_modules|APP|dist|build|out|obj|venv|\.venv|__pycache__)(/|$)'
$ArtifactExt = '(?i)\.(log|tmp|bak|swp|swo|pyc|class|zip|7z|exe|dll|so|dylib|pdb)$'
$Secret      = '(?i)(BEGIN .{0,20}PRIVATE KEY|AKIA[0-9A-Z]{16}|gh[pousr]_[A-Za-z0-9]{30,}|(api[_-]?key|access[_-]?token|password|secret)\s*[:=]\s*["'']?[A-Za-z0-9/+_.-]{12,})'
$AllowName   = '(?i)^(README|CHANGELOG|CONTRIBUTING|SECURITY|LICENSE|NOTICE)'
$BinaryExt   = @('.png','.jpg','.jpeg','.gif','.webp','.pdf','.zip','.7z','.gz','.exe','.dll','.so','.dylib','.woff','.woff2','.ttf','.mp4','.mp3','.ico','.class','.jar','.pyc')
$MaxLines    = 2000
# --------------------------------------

$files = @(& git diff --cached --name-only --diff-filter=ACMR) | Where-Object { $_ -ne '' }
$files = @($files)
if ($files.Count -eq 0) { exit 0 }

$issues = [System.Collections.Generic.List[string]]::new()
function Add-Issue([string]$file, [string]$reason) { $script:issues.Add(('{0} :: {1}' -f $file, $reason)) }

foreach ($f in $files) {
    $name = [System.IO.Path]::GetFileName($f)
    $ext  = [System.IO.Path]::GetExtension($f).ToLowerInvariant()

    if ($f -match $ArtifactDir) { Add-Issue $f '构建/依赖目录'; continue }
    if ($f -match $ProcDir)     { Add-Issue $f '过程资料目录'; continue }
    if ($ext -match $ArtifactExt) { Add-Issue $f '日志/临时/产物'; continue }

    if ($name -match $ProcName -and $name -notmatch $AllowName) { Add-Issue $f '过程文档命名' }
    if ($f -match $AbsPath) { Add-Issue $f '文件名含绝对路径' }

    if ($BinaryExt -contains $ext) { continue }
    $blob = @(& git show ":$f") 2>$null
    if (-not $blob) { continue }
    $lines = $blob | Select-Object -First $MaxLines

    foreach ($line in $lines) { if ($line -match $Secret) { Add-Issue $f '疑似凭据'; break } }
    foreach ($line in $lines) {
        if ($line -match $AbsAllow) { continue }
        if ($line -match $AbsPath) { Add-Issue $f '内容含绝对路径'; break }
    }
}

if ($issues.Count -gt 0) {
    Write-Host ''
    Write-Host 'commit blocked by pre-commit check' -ForegroundColor Red
    $issues | ForEach-Object { Write-Host "  - $_" -ForegroundColor Yellow }
    Write-Host 'unstage with: git restore --staged <path>' -ForegroundColor Cyan
    exit 1
}
exit 0
