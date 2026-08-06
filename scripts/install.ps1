# kern one-line installer (Windows).
#
#   irm https://raw.githubusercontent.com/yesitsfebreeze/kern/master/install.ps1 | iex
#
# Downloads the prebuilt kern.exe for this platform from the latest GitHub
# release and installs it to %LOCALAPPDATA%\kern\bin (override with
# $env:KERN_BIN_DIR), adding that directory to the user PATH.
$ErrorActionPreference = 'Stop'

$repo = 'yesitsfebreeze/kern'
$binDir = if ($env:KERN_BIN_DIR) { $env:KERN_BIN_DIR } else { "$env:LOCALAPPDATA\kern\bin" }

switch ($env:PROCESSOR_ARCHITECTURE) {
  'AMD64' { $target = 'x86_64-pc-windows-msvc' }
  'ARM64' { $target = 'aarch64-pc-windows-msvc' }
  'x86'   { $target = 'i686-pc-windows-msvc' }
  default { Write-Error "kern: no prebuilt Windows binary for $($env:PROCESSOR_ARCHITECTURE) (build from source)" }
}
$url = "https://github.com/$repo/releases/latest/download/kern-$target.zip"

Write-Host "kern: downloading $target ..."
New-Item -ItemType Directory -Force -Path $binDir | Out-Null
$zip = Join-Path $env:TEMP "kern-$target.zip"
try {
  Invoke-WebRequest -Uri $url -OutFile $zip
} catch {
  Write-Error "kern: download failed ($url). No release yet? See https://github.com/$repo/releases"
}
Expand-Archive -Path $zip -DestinationPath $binDir -Force
Remove-Item $zip -ErrorAction SilentlyContinue

Write-Host "kern: installed to $binDir\kern.exe"

# Add to user PATH if missing.
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($userPath -split ';') -notcontains $binDir) {
  [Environment]::SetEnvironmentVariable('Path', "$userPath;$binDir", 'User')
  Write-Host "kern: added $binDir to your user PATH (restart the shell to pick it up)"
}
Write-Host "kern: next - register the MCP server:  claude mcp add kern -- `"$binDir\kern.exe`" mcp"
