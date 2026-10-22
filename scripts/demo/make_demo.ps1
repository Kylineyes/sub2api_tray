param(
    [string]$OutputDir = "",
    [string]$ExePath = ""
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Security
$repo = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
if ([string]::IsNullOrWhiteSpace($OutputDir)) {
    $OutputDir = Join-Path $repo "target\demo-fixture"
}
if ([string]::IsNullOrWhiteSpace($ExePath)) {
    $ExePath = Join-Path $repo "target\release\sub2api_tray.exe"
}
if (-not (Test-Path $ExePath)) {
    throw "找不到 release exe：$ExePath"
}

New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
Copy-Item $ExePath (Join-Path $OutputDir "sub2api_tray.exe") -Force
Copy-Item (Join-Path $repo "testdata\demo\app.json") (Join-Path $OutputDir "app.json") -Force

$credentialJson = '{"password":"demo-password","accessToken":"demo-access-token","refreshToken":"demo-refresh-token"}'
$entropy = [Text.Encoding]::UTF8.GetBytes("sub2api_tray/credentials/v1")
$protected = [System.Security.Cryptography.ProtectedData]::Protect(
    [Text.Encoding]::UTF8.GetBytes($credentialJson),
    $entropy,
    [System.Security.Cryptography.DataProtectionScope]::CurrentUser
)
$secret = "enc:dpapi:v1:" + [Convert]::ToBase64String($protected)
$templatePath = Join-Path $repo "testdata\demo\accounts.template.json"
$accountText = [IO.File]::ReadAllText($templatePath).Replace("__SECRET__", $secret)
[IO.File]::WriteAllText(
    (Join-Path $OutputDir "accounts.json"),
    $accountText,
    [Text.UTF8Encoding]::new($false)
)

Write-Host "演示目录已生成：$OutputDir"
Write-Host "启动假服务器：python scripts/demo/fake_server.py"
Write-Host "启动托盘：$OutputDir\sub2api_tray.exe"
