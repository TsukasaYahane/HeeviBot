# 把 meme_server\fonts 里的字体装到「当前用户」（不需要管理员，不动 C:\Windows\Fonts）。
# 等同于在资源管理器里双击字体点「安装」：复制到 %LOCALAPPDATA%\Microsoft\Windows\Fonts，
# 并在 HKCU\Software\Microsoft\Windows NT\CurrentVersion\Fonts 登记。可重复运行，已装的跳过。
# 卸载：删掉该目录里对应文件 + 删掉注册表里同名的值。
$ErrorActionPreference = 'Stop'
$src = Join-Path $PSScriptRoot 'fonts'
$dst = Join-Path $env:LOCALAPPDATA 'Microsoft\Windows\Fonts'
$reg = 'HKCU:\Software\Microsoft\Windows NT\CurrentVersion\Fonts'
New-Item -ItemType Directory -Force $dst | Out-Null
if (-not (Test-Path $reg)) { New-Item -Path $reg -Force | Out-Null }

$sysFonts = Get-ChildItem "$env:WINDIR\Fonts" -File | Select-Object -ExpandProperty Name
$installed = 0; $skipped = 0
foreach ($f in Get-ChildItem $src -File | Where-Object { $_.Extension -in '.ttf', '.otf', '.ttc' }) {
    if ($sysFonts -contains $f.Name) { Write-Output "跳过（系统已有）：$($f.Name)"; $skipped++; continue }
    $target = Join-Path $dst $f.Name
    if ((Test-Path $target) -and ((Get-Item $target).Length -eq $f.Length)) {
        Write-Output "跳过（已安装）：$($f.Name)"; $skipped++
    } else {
        Copy-Item $f.FullName $target -Force
        Write-Output "已安装：$($f.Name)"; $installed++
    }
    $kind = if ($f.Extension -eq '.otf') { 'OpenType' } else { 'TrueType' }
    Set-ItemProperty -Path $reg -Name "$($f.BaseName) ($kind)" -Value $target
}

# 通知正在运行的程序字体有变化（WM_FONTCHANGE）；新启动的程序（如 meme run）本来就能看到
Add-Type -Namespace Win32 -Name Native -MemberDefinition @'
[DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Auto)]
public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint Msg, UIntPtr wParam, IntPtr lParam, uint fuFlags, uint uTimeout, out UIntPtr lpdwResult);
'@
$r = [UIntPtr]::Zero
[void][Win32.Native]::SendMessageTimeout([IntPtr]0xffff, 0x001D, [UIntPtr]::Zero, [IntPtr]::Zero, 2, 1000, [ref]$r)
Write-Output "完成：新装 $installed 个，跳过 $skipped 个（目录：$dst）"
