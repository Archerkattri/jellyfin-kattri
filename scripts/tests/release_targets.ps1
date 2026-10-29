$ErrorActionPreference = 'Stop'

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
Push-Location $repoRoot
try {
    $metadataOutput = & cargo metadata --no-deps --locked --offline --format-version 1
    if ($LASTEXITCODE -ne 0) {
        throw "cargo metadata failed with exit code $LASTEXITCODE."
    }
    $metadata = ($metadataOutput -join "`n") | ConvertFrom-Json

    $hostPackage = @($metadata.packages | Where-Object { $_.name -eq 'egui-gl-host-proof' })
    if ($hostPackage.Count -ne 1) {
        throw "Expected one egui-gl-host-proof package; found $($hostPackage.Count)."
    }
    $releaseBinary = @($hostPackage[0].targets | Where-Object {
        $_.name -eq 'jellyfin-kattri' -and $_.kind -contains 'bin'
    })
    if ($releaseBinary.Count -ne 1) {
        throw 'egui-gl-host-proof must define one binary target named jellyfin-kattri.'
    }
    $workspaceManifest = Get-Content 'Cargo.toml' -Raw
    if ($workspaceManifest -notmatch '(?m)^version = "([^"]+)"') {
        throw 'Could not read the workspace release version.'
    }
    if ($hostPackage[0].version -ne $Matches[1]) {
        throw "Release host version $($hostPackage[0].version) must match workspace version $($Matches[1]) for deb/rpm metadata."
    }
    if (-not $releaseBinary[0].src_path.EndsWith('/src/main.rs') -and
        -not $releaseBinary[0].src_path.EndsWith('\src\main.rs')) {
        throw "jellyfin-kattri must use the existing src/main.rs host; got $($releaseBinary[0].src_path)."
    }
    $hostRequiredFeatures = $releaseBinary[0].PSObject.Properties['required-features']
    if ($null -ne $hostRequiredFeatures -and @($hostRequiredFeatures.Value).Count -gt 0) {
        throw 'The release host binary must not require a demo or other opt-in feature.'
    }
    if (@($hostPackage[0].features.PSObject.Properties.Name) -contains 'eframe-demo') {
        throw 'The release host package must not declare or enable eframe-demo.'
    }
    $demoPackage = @($metadata.packages | Where-Object { $_.name -eq 'jellyfin-egui-client' })
    $demoTarget = @($demoPackage[0].targets | Where-Object { $_.name -eq 'jellyfin-egui-client' })
    $demoRequiredFeatures = $demoTarget[0].PSObject.Properties['required-features']
    if ($null -eq $demoRequiredFeatures -or @($demoRequiredFeatures.Value) -notcontains 'eframe-demo') {
        throw 'Cargo metadata feature-field control failed: the known eframe demo target must require eframe-demo.'
    }

    $windows = Get-Content '.github/workflows/release-windows.yml' -Raw
    if ($windows -notmatch '(?s)KATTRI_BIN:\s*jellyfin-kattri' -or
        $windows -notmatch '(?s)cargo build[^\r\n]*-p egui-gl-host-proof --bin \$env:KATTRI_BIN') {
        throw 'Windows release workflow does not build the jellyfin-kattri host binary.'
    }

    $linux = Get-Content '.github/workflows/release-linux.yml' -Raw
    if ($linux -notmatch '(?s)cargo build[^\r\n]*-p egui-gl-host-proof --bin jellyfin-kattri' -or
        $linux -notmatch '(?s)path:\s*target/release/jellyfin-kattri' -or
        $linux -notmatch '(?s)build-appimage\.sh[\s\S]*?target/release/jellyfin-kattri' -or
        $linux -notmatch '(?s)cargo deb -p egui-gl-host-proof --no-build' -or
        $linux -notmatch '(?s)cargo generate-rpm -p proofs/egui-gl-host') {
        throw 'Linux release workflow does not build and package the jellyfin-kattri host binary.'
    }

    $flatpakToolsCommit = '9d86d336a39b9af38b86f114b6ce5820fe260f91'
    if ($linux -notmatch [regex]::Escape("FBT_COMMIT=$flatpakToolsCommit") -or
        $linux -notmatch 'git -C "\$RUNNER_TEMP/fbt" fetch --depth 1 origin "\$FBT_COMMIT"' -or
        $linux -notmatch 'ACTUAL_FBT_COMMIT="\$\(git -C "\$RUNNER_TEMP/fbt" rev-parse HEAD\)"' -or
        $linux -notmatch 'if \[\[ "\$ACTUAL_FBT_COMMIT" != "\$FBT_COMMIT" \]\]') {
        throw 'Flatpak release workflow must fetch the pinned flatpak-builder-tools commit and verify exact HEAD.'
    }
    $flatpakBuildStep = $linux.Substring($linux.IndexOf('Build Flatpak bundle'))
    if ($flatpakBuildStep -notmatch 'KATTRI_RELEASE_VERSION:.*?github\.ref_type == .tag' -or
        $flatpakBuildStep -notmatch 'if \[\[ "\$KATTRI_RELEASE_VERSION" != "" && "\$KATTRI_RELEASE_VERSION" != "v\$VERSION" \]\]') {
        throw 'Flatpak release workflow must reject a tag that does not match the Cargo workspace version.'
    }
    $debMetadata = Get-Content 'packaging/linux/deb/cargo-deb.toml' -Raw
    $rpmMetadata = Get-Content 'packaging/linux/rpm/generate-rpm.toml' -Raw
    if ($debMetadata -notmatch 'target/release/jellyfin-kattri' -or
        $debMetadata -notmatch '"usr/bin/kattri"' -or
        $debMetadata -notmatch 'packaging/linux/icons/kattri\.svg' -or
        $debMetadata -notmatch 'icons/hicolor/scalable/apps/kattri\.svg' -or
        $rpmMetadata -notmatch 'target/kattri-stripped' -or
        $rpmMetadata -notmatch '"/usr/bin/kattri"' -or
        $rpmMetadata -notmatch 'packaging/linux/icons/kattri\.svg' -or
        $rpmMetadata -notmatch 'icons/hicolor/scalable/apps/kattri\.svg') {
        throw 'Linux package metadata must source jellyfin-kattri, preserve the kattri command, and install the SVG icon.'
    }
    # The rpm packages a stripped copy of the same host binary, made by the release workflow.
    if ($linux -notmatch 'strip -o target/kattri-stripped target/release/jellyfin-kattri') {
        throw 'Linux release workflow must strip the jellyfin-kattri host binary for the rpm.'
    }
    if ($debMetadata -match 'jellyfin-egui-client' -or $rpmMetadata -match 'jellyfin-egui-client') {
        throw 'Linux package metadata still references the old demo executable.'
    }
    $hostManifestDir = Join-Path $repoRoot 'proofs/egui-gl-host'
    foreach ($packageRelativePath in @(
        '../../LICENSE',
        '../../packaging/linux/share/kattri.desktop',
        '../../packaging/linux/icons/kattri.svg'
    )) {
        if (-not (Test-Path -LiteralPath (Join-Path $hostManifestDir $packageRelativePath))) {
            throw "Deb package-relative path does not resolve: $packageRelativePath"
        }
    }
    foreach ($workspaceRelativePath in @(
        'packaging/linux/share/kattri.desktop',
        'packaging/linux/icons/kattri.svg'
    )) {
        if (-not (Test-Path -LiteralPath (Join-Path $repoRoot $workspaceRelativePath))) {
            throw "RPM workspace-relative path does not resolve: $workspaceRelativePath"
        }
    }
    $appImageScript = Get-Content 'packaging/linux/appimage/build-appimage.sh' -Raw
    if ($appImageScript -notmatch 'icons/kattri\.svg' -or
        $appImageScript -notmatch 'hicolor/scalable/apps/kattri\.svg' -or
        $appImageScript -notmatch '--icon-file') {
        throw 'AppImage packaging must use the scalable SVG icon with linuxdeploy.'
    }
    $flatpakManifest = Get-Content 'packaging/flatpak/org.kattri.JellyfinKattri.yml' -Raw
    if ($flatpakManifest -notmatch 'packaging/linux/icons/kattri\.svg' -or
        $flatpakManifest -notmatch 'hicolor/scalable/apps/org\.kattri\.JellyfinKattri\.svg' -or
        $flatpakManifest -notmatch 'cargo --offline build --release --locked -p egui-gl-host-proof --bin jellyfin-kattri' -or
        $flatpakManifest -notmatch 'install -Dm755 target/release/jellyfin-kattri /app/bin/kattri') {
        throw 'Flatpak manifest must build the product binary and install its SVG icon.'
    }
    if ($flatpakManifest -match 'packaging/linux/icons/kattri\.png' -or
        $flatpakManifest -match 'jellyfin-egui-client' -or
        $debMetadata -match 'packaging/linux/icons/kattri\.png' -or
        $rpmMetadata -match 'packaging/linux/icons/kattri\.png' -or
        $appImageScript -match 'packaging/linux/icons/kattri\.png') {
        throw 'Linux packaging still references the missing PNG icon.'
    }
    if (-not (Test-Path -LiteralPath (Join-Path $repoRoot 'packaging/linux/icons/kattri.svg'))) {
        throw 'The shared Linux SVG icon asset is missing.'
    }

    $sbomWorkflow = Get-Content '.github/workflows/sbom.yml' -Raw
    if ($sbomWorkflow -notmatch 'cargo auditable build --release --locked -p egui-gl-host-proof --bin jellyfin-kattri' -or
        $sbomWorkflow -notmatch 'cargo audit --json bin target/release/jellyfin-kattri' -or
        $sbomWorkflow -notmatch 'cargo cyclonedx --manifest-path proofs/egui-gl-host/Cargo\.toml' -or
        $sbomWorkflow -match 'jellyfin-egui-client') {
        throw 'SBOM workflow must build, audit, and describe the release GL host package.'
    }

    $reproWorkflow = Get-Content '.github/workflows/reproducible.yml' -Raw
    $reproBuilds = [regex]::Matches($reproWorkflow, 'cargo build --release --locked -p egui-gl-host-proof --bin jellyfin-kattri')
    $reproHashes = [regex]::Matches($reproWorkflow, 'sha256sum target/release/jellyfin-kattri')
    if ($reproBuilds.Count -ne 2 -or $reproHashes.Count -ne 2 -or
        $reproWorkflow -notmatch '\$TREE_A/target/release/jellyfin-kattri' -or
        $reproWorkflow -notmatch '\$TREE_B/target/release/jellyfin-kattri' -or
        $reproWorkflow -match 'jellyfin-egui-client') {
        throw 'Reproducibility workflow must build and compare jellyfin-kattri in both source trees.'
    }

    $macosWorkflow = Get-Content '.github/workflows/release-macos.yml' -Raw
    $macosScript = Get-Content 'packaging/macos/build-app.sh' -Raw
    if ($macosWorkflow -notmatch '(?s)CARGO_PACKAGE:\s*egui-gl-host-proof' -or
        $macosWorkflow -notmatch '(?s)BINARY_NAME:\s*jellyfin-kattri' -or
        $macosScript -notmatch '(?s)CARGO_PACKAGE=\"\$\{CARGO_PACKAGE:-egui-gl-host-proof\}' -or
        $macosScript -notmatch '(?s)BINARY_NAME=\"\$\{BINARY_NAME:-jellyfin-kattri\}') {
        throw 'macOS release workflow and build script must target the jellyfin-kattri host binary.'
    }

    if ($macosWorkflow -notmatch 'codesign --force --deep -s - "dist/macos/Jellyfin Kattri\.app"' -or
        $macosWorkflow -notmatch 'suffix=-ad-hoc' -or
        $macosWorkflow -match 'suffix=-unsigned') {
        throw 'macOS releases without Developer ID credentials must use and label ad-hoc app signing.'
    }

    foreach ($workflowPath in @(
        '.github/workflows/release-windows.yml',
        '.github/workflows/release-linux.yml',
        '.github/workflows/release-macos.yml'
    )) {
        $workflow = Get-Content $workflowPath -Raw
        if ($workflow -match 'jellyfin-egui-client') {
            throw "$workflowPath still targets the old demo executable."
        }
        if ($workflow -match 'eframe-demo') {
            throw "$workflowPath enables the eframe demo feature."
        }
    }

    Write-Output 'PASS: release package and all three OS workflows target jellyfin-kattri without demo features.'
}
finally {
    Pop-Location
}
