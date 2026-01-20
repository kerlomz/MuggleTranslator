$ErrorActionPreference = "Stop"

$exeName = "muggle-translator.exe"
$root = $PSScriptRoot
$distDir = Join-Path -Path $root -ChildPath "dist\\win"
$targetDir = Join-Path -Path $root -ChildPath "target-dist"

# Force CUDA 13.0 toolchain selection for llama-cpp-rs build.
$cudaRoot = "C:\\Program Files\\NVIDIA GPU Computing Toolkit\\CUDA\\v13.0"
if (Test-Path $cudaRoot) {
    $env:CUDA_PATH = $cudaRoot
    $env:CUDAToolkit_ROOT = $cudaRoot
    $env:PATH = "$cudaRoot\\bin;$cudaRoot\\bin\\x64;$env:PATH"
    Write-Host "[build] CUDA_PATH=$env:CUDA_PATH"
} else {
    throw "[build] CUDA 13.0 not found at $cudaRoot (GPU build required)"
}

# Bindgen needs libclang (provided by VS2022 LLVM toolset).
$llvmBin = "C:\\Program Files\\Microsoft Visual Studio\\2022\\Professional\\VC\\Tools\\Llvm\\x64\\bin"
if ((-not $env:LIBCLANG_PATH) -and (Test-Path (Join-Path $llvmBin "libclang.dll"))) {
    $env:LIBCLANG_PATH = $llvmBin
    Write-Host "[build] LIBCLANG_PATH=$env:LIBCLANG_PATH"
}

Write-Host "[build] cargo build --release"
# The binary can be locked if a previous run is still running.
Get-Process -Name "muggle-translator" -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 200

cargo build --release --target-dir $targetDir
if ($LASTEXITCODE -ne 0) {
    throw "cargo build failed (exit=$LASTEXITCODE)"
}

$src = Join-Path -Path $targetDir -ChildPath "release\\$exeName"
if (-not (Test-Path $src)) {
    throw "Build succeeded but binary not found: $src"
}

New-Item -ItemType Directory -Force -Path $distDir | Out-Null

$dstExe = Join-Path -Path $distDir -ChildPath $exeName
Copy-Item -Force $src $dstExe
Write-Host "[dist] exe -> $dstExe"

$cfgToml = Join-Path -Path $root -ChildPath "muggle-translator.toml"
if (Test-Path $cfgToml) {
    Copy-Item -Force $cfgToml (Join-Path -Path $distDir -ChildPath "muggle-translator.toml")
    Write-Host "[dist] config -> $distDir\\muggle-translator.toml"
}

$promptsDir = Join-Path -Path $root -ChildPath "prompts"
if (Test-Path $promptsDir) {
    $dstPrompts = Join-Path -Path $distDir -ChildPath "prompts"
    New-Item -ItemType Directory -Force -Path $dstPrompts | Out-Null
    Copy-Item -Force -Recurse (Join-Path $promptsDir "*") $dstPrompts
    Write-Host "[dist] prompts -> $distDir\\prompts"
}

$testDocx = Join-Path -Path $root -ChildPath "test.docx"
if (Test-Path $testDocx) {
    Copy-Item -Force $testDocx (Join-Path -Path $distDir -ChildPath "test.docx")
    Write-Host "[dist] test.docx -> $distDir"
}

function Get-DumpbinPath {
    $msvcRoot = "C:\\Program Files\\Microsoft Visual Studio\\2022\\Professional\\VC\\Tools\\MSVC"
    if (-not (Test-Path $msvcRoot)) { return $null }
    $latest = Get-ChildItem -Path $msvcRoot -Directory | Sort-Object Name -Descending | Select-Object -First 1
    if (-not $latest) { return $null }
    $dumpbin = Join-Path $latest.FullName "bin\\Hostx64\\x64\\dumpbin.exe"
    if (Test-Path $dumpbin) { return $dumpbin }
    return $null
}

function Get-Dependents([string]$filePath, [string]$dumpbinExe) {
    $text = (& $dumpbinExe /nologo /dependents $filePath 2>$null | Out-String)
    if (-not $text) { return @() }
    $deps = @()
    foreach ($line in ($text -split "`r?`n")) {
        $line = $line.TrimEnd()
        if ($line -match '^\s+([A-Za-z0-9_.-]+\.dll)$') {
            $deps += $matches[1].ToLowerInvariant()
        }
    }
    return ($deps | Sort-Object -Unique)
}

# Enforce GPU-capable build: the binary must link CUDA runtime dependencies.
$dumpbin = Get-DumpbinPath
if (-not $dumpbin) {
    throw "[dist] dumpbin.exe not found (GPU build verification required)"
}
$deps = Get-Dependents $dstExe $dumpbin
if (-not ($deps -contains "cublas64_13.dll")) {
    throw "[dist] GPU build verification failed: cublas64_13.dll not found in dependents. Ensure llama-cpp-2 is built with CUDA and CUDA_PATH is correct."
}

# Enforce GPU config: disallow gpu_layers = 0 anywhere in the packaged config.
$distCfg = Join-Path -Path $distDir -ChildPath "muggle-translator.toml"
if (Test-Path $distCfg) {
    $cfgText = Get-Content -Raw -Encoding utf8 $distCfg
    # Ensure the packaged config can find models located at repo root (one level above dist/win).
    $cfgText = [regex]::Replace(
        $cfgText,
        '(?m)^[ \t]*model_dir[ \t]*=[ \t]*(?:\"\.\"|''\.'')\s*$',
        "model_dir = '$root'"
    )
    Set-Content -NoNewline -Encoding utf8 $distCfg $cfgText
    if ($cfgText -match "(?m)^[ \\t]*gpu_layers[ \\t]*=[ \\t]*0[ \\t]*$") {
        throw "[dist] GPU config verification failed: found gpu_layers = 0 in $distCfg"
    }
}

# Always copy the core CUDA + MSVC runtime DLLs needed for native GPU execution.
$cudaBinX64 = Join-Path $env:CUDA_PATH "bin\\x64"
$cudaDlls = @(
    "cudart64_13.dll",
    "cublas64_13.dll",
    "cublasLt64_13.dll",
    "nvrtc64_130_0.dll",
    "nvrtc-builtins64_130.dll",
    "nvJitLink_130_0.dll",
    "nvfatbin_130_0.dll"
)
foreach ($dll in $cudaDlls) {
    $p = Join-Path $cudaBinX64 $dll
    if (Test-Path $p) {
        Copy-Item -Force $p (Join-Path $distDir $dll)
        Write-Host "[dist] dll -> $dll"
    }
}

$sys32 = Join-Path $env:SystemRoot "System32"
$vcDlls = @("MSVCP140.dll", "VCRUNTIME140.dll", "VCRUNTIME140_1.dll", "VCOMP140.DLL")
foreach ($dll in $vcDlls) {
    $p = Join-Path $sys32 $dll
    if (Test-Path $p) {
        Copy-Item -Force $p (Join-Path $distDir $dll)
        Write-Host "[dist] dll -> $dll"
    }
}

function Resolve-Dll([string]$dllName, [string[]]$searchDirs) {
    foreach ($d in $searchDirs) {
        $p = Join-Path -Path $d -ChildPath $dllName
        if (Test-Path $p) { return $p }
    }
    return $null
}

function Is-SystemDll([string]$dllName) {
    $n = $dllName.ToLowerInvariant()
    if ($n.StartsWith("api-ms-win-") -or $n.StartsWith("ext-ms-win-")) { return $true }
    $sys = @(
        "kernel32.dll","user32.dll","gdi32.dll","win32u.dll","shell32.dll","advapi32.dll",
        "ws2_32.dll","bcrypt.dll","crypt32.dll","ole32.dll","oleaut32.dll","shlwapi.dll",
        "comdlg32.dll","secur32.dll","ntdll.dll","rpcrt4.dll","imm32.dll","setupapi.dll",
        "version.dll","winmm.dll","dbghelp.dll","dbgcore.dll","psapi.dll"
    )
    return $sys -contains $n
}

$dumpbin = Get-DumpbinPath
if (-not $dumpbin) {
    Write-Host "[dist] warning: dumpbin.exe not found; copying a minimal CUDA runtime set"
    $cudaBinX64 = Join-Path $env:CUDA_PATH "bin\\x64"
    $cudaDlls = @("cudart64_13.dll","cublas64_13.dll","cublasLt64_13.dll","nvrtc64_130_0.dll","nvrtc-builtins64_130.dll","nvJitLink_130_0.dll","nvfatbin_130_0.dll")
    foreach ($dll in $cudaDlls) {
        $p = Join-Path $cudaBinX64 $dll
        if (Test-Path $p) { Copy-Item -Force $p (Join-Path $distDir $dll) }
    }
    return
}

$searchDirs = @()
if (Test-Path $distDir) { $searchDirs += $distDir }
if (Test-Path (Join-Path $env:CUDA_PATH "bin\\x64")) { $searchDirs += (Join-Path $env:CUDA_PATH "bin\\x64") }
if (Test-Path (Join-Path $env:CUDA_PATH "bin")) { $searchDirs += (Join-Path $env:CUDA_PATH "bin") }
$searchDirs += @("$env:SystemRoot\\System32")

Write-Host "[dist] dumpbin -> $dumpbin"
Write-Host "[dist] resolving DLL dependencies ..."

$queue = New-Object System.Collections.Generic.Queue[string]
$seen = New-Object System.Collections.Generic.HashSet[string]

$queue.Enqueue($dstExe)
$null = $seen.Add((Resolve-Path $dstExe).Path)

while ($queue.Count -gt 0) {
    $file = $queue.Dequeue()
    $deps = Get-Dependents $file $dumpbin
    foreach ($dll in $deps) {
        if (Is-SystemDll $dll) { continue }
        $dst = Join-Path $distDir $dll
        if (Test-Path $dst) {
            $real = (Resolve-Path $dst).Path
            if (-not $seen.Contains($real)) {
                $null = $seen.Add($real)
                $queue.Enqueue($dst)
            }
            continue
        }

        $srcDll = Resolve-Dll $dll $searchDirs
        if (-not $srcDll) {
            Write-Host "[dist] warning: dependency not found: $dll (required by $file)"
            continue
        }

        Copy-Item -Force $srcDll $dst
        Write-Host "[dist] dll -> $dll"
        $real = (Resolve-Path $dst).Path
        if (-not $seen.Contains($real)) {
            $null = $seen.Add($real)
            $queue.Enqueue($dst)
        }
    }
}

Write-Host "[dist] done -> $distDir"
