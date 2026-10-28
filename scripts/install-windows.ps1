# Builds the task-x runtime for Windows, packages the extension with it, and installs it into VS Code.
# Run from the repository root:  .\scripts\install-windows.ps1  (add -Code code-insiders for Insiders)
param([string]$Code = 'code')

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

foreach ($tool in 'git', 'cargo', 'node', 'npm', $Code) {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        throw "$tool is not on the PATH. Install git, Rust (rustup, MSVC toolchain), Node.js 22+, and VS Code, then open a new terminal."
    }
}

function Invoke-Step([string]$What, [scriptblock]$Run) {
    Write-Host "==> $What" -ForegroundColor Cyan
    & $Run
    if ($LASTEXITCODE -ne 0) { throw "$What failed (exit $LASTEXITCODE)." }
}

Invoke-Step 'Build the runtime (release)' { cargo build --release --manifest-path runtime/Cargo.toml }

$bin = Join-Path $root 'extension\bin'
New-Item -ItemType Directory -Force -Path $bin | Out-Null
# Remove before copy: a running runtime keeps the old file locked otherwise.
Remove-Item -Force -ErrorAction SilentlyContinue (Join-Path $bin 'task-x-runtime.exe'), (Join-Path $bin 'task-x-runtime')
Copy-Item (Join-Path $root 'runtime\target\release\task-x-runtime.exe') $bin

Invoke-Step 'Install extension packages' { npm --prefix extension ci --no-audit --no-fund }
Get-ChildItem -Path (Join-Path $root 'extension') -Filter '*.vsix' | Remove-Item -Force
Invoke-Step 'Package the extension' { npm --prefix extension run package }

$vsix = Get-ChildItem -Path (Join-Path $root 'extension') -Filter 'task-x-*.vsix' | Select-Object -First 1
if (-not $vsix) { throw 'No .vsix was built.' }
Invoke-Step "Install $($vsix.Name) into VS Code" { & $Code --install-extension $vsix.FullName --force }

Write-Host "Installed $($vsix.Name). Reload VS Code windows to use it." -ForegroundColor Green
