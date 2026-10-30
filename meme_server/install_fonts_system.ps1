# 把 meme_server\fonts 里的字体装成「系统字体」（C:\Windows\Fonts + HKLM 登记），需要管理员权限。
# 为什么不用当前用户安装：meme-generator 用 skia 排字，skia 在 Windows 上只读系统字体集合，
# 看不到 %LOCALAPPDATA% 里的用户字体（2026-10-21 实测）。
# 会顺带清掉 install_fonts.ps1 装的同名用户字体，避免两份。可重复运行，已装的跳过。
$ErrorActionPreference = 'Stop'
$log = Join-Path $PSScriptRoot 'install_fonts_system.log'
Start-Transcript -Path $log -Force | Out-Null
try {
    $principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw '需要管理员权限：右键「以管理员身份运行」PowerShell 再执行本脚本。'
    }
    $src = Join-Path $PSScriptRoot 'fonts'
    $sysDir = Join-Path $env:WINDIR 'Fonts'
    $hklm = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Fonts'
    $hkcu = 'HKCU:\Software\Microsoft\Windows NT\CurrentVersion\Fonts'
    $userDir = Join-Path $env:LOCALAPPDATA 'Microsoft\Windows\Fonts'
    $installed = 0; $skipped = 0
    foreach ($f in Get-ChildItem $src -File | Where-Object { $_.Extension -in '.ttf', '.otf', '.ttc' }) {
        $kind = if ($f.Extension -eq '.otf') { 'OpenType' } else { 'TrueType' }
        $valueName = "$($f.BaseName) ($kind)"
        $target = Join-Path $sysDir $f.Name
        # 系统里已有同名文件就不碰（不论版本；系统自带字体如 consola.ttf 受保护，也不该被覆盖）
        if (Test-Path $target) {
            Write-Output "跳过（系统已有）：$($f.Name)"; $skipped++
        } else {
            Copy-Item $f.FullName $target -Force
            Set-ItemProperty -Path $hklm -Name $valueName -Value $f.Name
            Write-Output "已安装到系统：$($f.Name)"; $installed++
        }
        # 清理用户级重复安装
        if (Get-ItemProperty -Path $hkcu -Name $valueName -ErrorAction SilentlyContinue) {
            Remove-ItemProperty -Path $hkcu -Name $valueName
        }
        $userCopy = Join-Path $userDir $f.Name
        if (Test-Path $userCopy) { Remove-Item $userCopy -Force -ErrorAction SilentlyContinue }
    }
    Add-Type -Namespace Win32 -Name Native -MemberDefinition @'
[DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Auto)]
public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint Msg, UIntPtr wParam, IntPtr lParam, uint fuFlags, uint uTimeout, out UIntPtr lpdwResult);
'@
    $r = [UIntPtr]::Zero
    [void][Win32.Native]::SendMessageTimeout([IntPtr]0xffff, 0x001D, [UIntPtr]::Zero, [IntPtr]::Zero, 2, 1000, [ref]$r)
    Write-Output "完成：新装 $installed 个，跳过 $skipped 个"
} catch {
    Write-Output "失败：$_"
} finally {
    Stop-Transcript | Out-Null
}
