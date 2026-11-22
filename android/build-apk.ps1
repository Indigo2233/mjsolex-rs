[CmdletBinding()]
param(
    [string] $Sdk = $env:ANDROID_HOME,
    [string] $Jdk = $env:JAVA_HOME,
    [ValidateSet('arm64-v8a', 'x86_64')]
    [string[]] $Abi = @('arm64-v8a', 'x86_64')
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent
if (-not $Sdk) { $Sdk = $env:ANDROID_SDK_ROOT }
if (-not $Sdk -or -not (Test-Path $Sdk -PathType Container)) {
    throw 'Set ANDROID_HOME or pass -Sdk.'
}
if (-not $Jdk -or -not (Test-Path (Join-Path $Jdk 'bin/jar.exe') -PathType Leaf)) {
    throw 'Set JAVA_HOME to a JDK (17 or newer) or pass -Jdk.'
}
$env:ANDROID_HOME = (Resolve-Path $Sdk).Path
$env:ANDROID_SDK_ROOT = $env:ANDROID_HOME
$env:JAVA_HOME = (Resolve-Path $Jdk).Path
$env:PATH = "$env:JAVA_HOME\bin;$env:PATH"

$platform = Join-Path $env:ANDROID_HOME 'platforms/android-34/android.jar'
$tools = Join-Path $env:ANDROID_HOME 'build-tools/34.0.0'
foreach ($required in @($platform, "$tools/aapt2.exe", "$tools/d8.bat", "$tools/zipalign.exe", "$tools/apksigner.bat")) {
    if (-not (Test-Path -LiteralPath $required -PathType Leaf)) { throw "Missing Android SDK file $required" }
}

function Invoke-Tool {
    param([string] $Tool, [string[]] $Arguments)
    & $Tool @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Tool exited with code $LASTEXITCODE" }
}

$out = (New-Item -ItemType Directory -Force (Join-Path $repo 'target/apk')).FullName
$work = (New-Item -ItemType Directory (Join-Path $out ([guid]::NewGuid().ToString('N')))).FullName
Push-Location $repo
try {
    $payload = (New-Item -ItemType Directory "$work/payload").FullName
    $classes = (New-Item -ItemType Directory "$work/classes").FullName
    $ndk = @('ndk', '--platform', '26', '--output-dir', "$payload/lib")
    foreach ($target in ($Abi | Select-Object -Unique)) { $ndk += @('-t', $target) }
    # Support Android kernels that use 16 KiB memory pages with older NDKs.
    $env:RUSTFLAGS = "$env:RUSTFLAGS -C link-arg=-Wl,-z,max-page-size=16384".Trim()
    # Older cargo-ndk versions can dump their environment in a panic report.
    $ndkOutput = & cargo @ndk build --locked --release --lib 2>&1
    $ndkExit = $LASTEXITCODE
    foreach ($line in $ndkOutput) {
        if ("$line" -match '^env:') { break }
        Write-Host "$line"
    }
    if ($ndkExit -ne 0) { throw "Android Rust build exited with code $ndkExit" }

    Invoke-Tool javac.exe @('--release', '11', '-encoding', 'UTF-8', '-classpath', $platform, '-d', $classes, "$PSScriptRoot/MainActivity.java")
    Invoke-Tool jar.exe @('cf', "$work/activity.jar", '-C', $classes, '.')
    Invoke-Tool "$tools/d8.bat" @('--lib', $platform, '--min-api', '26', '--output', $payload, "$work/activity.jar")
    Invoke-Tool "$tools/aapt2.exe" @('compile', '--dir', "$PSScriptRoot/res", '-o', "$work/resources.zip")
    Invoke-Tool "$tools/aapt2.exe" @('link', '-I', $platform, '--manifest', "$PSScriptRoot/AndroidManifest.xml", '-o', "$work/unsigned.apk", "$work/resources.zip")
    Invoke-Tool jar.exe @('uf', "$work/unsigned.apk", '-C', $payload, 'classes.dex', '-C', $payload, 'lib')
    Invoke-Tool jar.exe @('uf', "$work/unsigned.apk", '-C', $repo,
        'assets/MJSolexSansSC-OFL.txt', 'assets/bass2000-NOTICE.txt')
    Invoke-Tool "$tools/zipalign.exe" @('-f', '-p', '4', "$work/unsigned.apk", "$work/aligned.apk")

    $key = Join-Path $out 'debug.jks'
    if (-not (Test-Path -LiteralPath $key)) {
        Invoke-Tool keytool.exe @(
            '-genkeypair', '-keystore', $key, '-storetype', 'PKCS12',
            '-storepass', 'android', '-alias', 'androiddebugkey', '-keypass', 'android',
            '-dname', 'CN=Android Debug,O=Android,C=US',
            '-keyalg', 'RSA', '-keysize', '2048', '-validity', '10000'
        )
    }
    $file = if (($Abi | Select-Object -Unique).Count -gt 1) { 'mjsolex-debug.apk' } else { "mjsolex-$($Abi[0])-debug.apk" }
    Invoke-Tool "$tools/apksigner.bat" @('sign', '--ks', $key, '--ks-pass', 'pass:android', '--out', "$work/signed.apk", "$work/aligned.apk")
    Invoke-Tool "$tools/apksigner.bat" @('verify', "$work/signed.apk")
    Move-Item -Force -LiteralPath "$work/signed.apk" -Destination (Join-Path $out $file)
    Write-Output (Join-Path $out $file)
} finally {
    Pop-Location
    $resolvedWork = (Resolve-Path -LiteralPath $work).Path
    if ((Split-Path $resolvedWork -Parent) -eq $out -and (Split-Path $resolvedWork -Leaf) -match '^[a-f0-9]{32}$') {
        Remove-Item -Recurse -Force -LiteralPath $resolvedWork
    }
}
