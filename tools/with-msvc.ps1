# このマシンのVisual Studio "18" (C:\Program Files\Microsoft Visual Studio\18\Community) は
# MSVCツールセット(VC\Tools\MSVC\<version>)にinclude/vcvarsall.batが欠落しており、
# msvcrt.lib等が見つからずwindows-msvcターゲットのリンクに失敗する(LNK1104)。
#
# 代わりにVS2019 BuildTools (C:\Program Files (x86)\Microsoft Visual Studio\2019\BuildTools) の
# vcvars64.batには完全なツールセットが入っているため、そちらから環境変数を読み込んでビルドする。
#
# 使い方:
#   powershell -File tools\with-msvc.ps1 <作業ディレクトリ> <実行するコマンド...>
# 例:
#   powershell -File tools\with-msvc.ps1 tools\dxgi-capture-poc cargo build
#   powershell -File tools\with-msvc.ps1 tools\dxgi-capture-poc cargo run

param(
    [Parameter(Mandatory = $true, Position = 0)]
    [string]$WorkDir,
    [Parameter(Mandatory = $true, ValueFromRemainingArguments = $true)]
    [string[]]$Cmd
)

$vcvars = 'C:\Program Files (x86)\Microsoft Visual Studio\2019\BuildTools\VC\Auxiliary\Build\vcvars64.bat'
if (-not (Test-Path $vcvars)) {
    Write-Error "vcvars64.bat not found at: $vcvars`nこのマシンのVSインストール構成が変わった場合はパスを更新してください。"
    exit 1
}

$out = & cmd /c "`"$vcvars`" && set"
foreach ($line in $out) {
    if ($line -match "^([^=]+)=(.*)$") {
        [System.Environment]::SetEnvironmentVariable($matches[1], $matches[2])
    }
}

Set-Location $WorkDir
$exe = $Cmd[0]
$rest = @($Cmd[1..($Cmd.Count - 1)])
& $exe @rest
exit $LASTEXITCODE
