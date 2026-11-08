$ErrorActionPreference = 'Stop'
$projectRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$cargoRoot = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
$oldFlags = $env:RUSTFLAGS
$oldEncoded = $env:CARGO_ENCODED_RUSTFLAGS
Push-Location -LiteralPath $projectRoot
try {
    $env:RUSTFLAGS = $null
    $env:CARGO_ENCODED_RUSTFLAGS = @("--remap-path-prefix=$projectRoot=FerrumCompare", "--remap-path-prefix=$cargoRoot=cargo", '-C', 'link-arg=/DEBUG:NONE') -join [char]31
    $ErrorActionPreference = 'Continue'
    $iconSource = $null
    cargo build --release --workspace --locked --message-format=json-render-diagnostics | ForEach-Object {
        $record = $_ | ConvertFrom-Json
        if ($record.reason -eq 'build-script-executed' -and $record.package_id -match '(/ferrumcompare-gui#|#ferrumcompare-gui@)') {
            $iconSource = Join-Path $record.out_dir 'ferrumcompare-gui.ico'
        }
    }
    $buildResult = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    if ($buildResult -ne 0) { throw "Build failed ($buildResult)" }
    $destination = Join-Path $projectRoot 'dist\FerrumCompare'
    New-Item -ItemType Directory -Path $destination -Force | Out-Null
    foreach ($file in 'ferrumcompare.exe','ferrumcompare-gui.exe') {
        Copy-Item -LiteralPath (Join-Path $projectRoot "target\release\$file") -Destination $destination -Force
    }
    if (-not $iconSource -or -not (Test-Path -LiteralPath $iconSource -PathType Leaf)) { throw 'Application icon was not generated' }
    Copy-Item -LiteralPath $iconSource -Destination (Join-Path $destination 'FerrumCompare.ico') -Force
    foreach ($file in 'LICENSE-MIT','LICENSE-APACHE','THIRD_PARTY_NOTICES.md') {
        Copy-Item -LiteralPath (Join-Path $projectRoot $file) -Destination $destination -Force
    }
    $archiveFiles = @('ferrumcompare.exe', 'ferrumcompare-gui.exe', 'FerrumCompare.ico', 'LICENSE-MIT', 'LICENSE-APACHE', 'THIRD_PARTY_NOTICES.md') |
        ForEach-Object { [pscustomobject]@{ Source = Join-Path $destination $_; Name = $_ } }
    foreach ($file in 'README.md', 'README.ru.md', 'CHANGELOG.md') {
        $source = Join-Path $projectRoot $file
        if (Test-Path -LiteralPath $source -PathType Leaf) {
            Copy-Item -LiteralPath $source -Destination $destination -Force
            $archiveFiles += [pscustomobject]@{ Source = Join-Path $destination $file; Name = $file }
        }
    }
    foreach ($file in 'folders-en.png', 'folders-ru.png', 'text-en.png', 'text-ru.png') {
        $source = Join-Path $projectRoot "docs\$file"
        if (Test-Path -LiteralPath $source -PathType Leaf) {
            $archiveFiles += [pscustomobject]@{ Source = $source; Name = "docs/$file" }
        }
    }
    $version = (Select-String -LiteralPath (Join-Path $projectRoot 'Cargo.toml') -Pattern '^version = "([^"]+)"$').Matches[0].Groups[1].Value
    $archiveName = "FerrumCompare-$version-x64.zip"
    $archive = Join-Path $projectRoot "dist\$archiveName"
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $stream = [IO.File]::Open($archive, [IO.FileMode]::Create, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try {
        $zip = [IO.Compression.ZipArchive]::new($stream, [IO.Compression.ZipArchiveMode]::Create, $true)
        try {
            foreach ($file in $archiveFiles) {
                [void][IO.Compression.ZipFileExtensions]::CreateEntryFromFile($zip, $file.Source, $file.Name, [IO.Compression.CompressionLevel]::Optimal)
            }
        } finally {
            $zip.Dispose()
        }
    } finally {
        $stream.Dispose()
    }
    $hash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText("$archive.sha256", "$hash  $archiveName", [Text.Encoding]::ASCII)
    Write-Output $archive
    Write-Output "SHA-256 $hash"
} finally {
    $env:RUSTFLAGS = $oldFlags
    $env:CARGO_ENCODED_RUSTFLAGS = $oldEncoded
    Pop-Location
}
